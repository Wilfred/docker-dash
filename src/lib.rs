use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{OnceCell, Semaphore};
use tokio::task::JoinSet;

mod diagnostics;
mod engine;
mod web;

const PAGE_SIZE: usize = 100;
const MAX_PAGE: usize = 99;

#[derive(Clone)]
pub struct AppState {
    pub socket_path: String,
    pub uptime_path: String,
    snapshots: Arc<Mutex<VecDeque<Snapshot>>>,
    requests: Arc<Semaphore>,
    api: Arc<OnceCell<(String, Value)>>,
}
impl Default for AppState {
    fn default() -> Self {
        Self {
            socket_path: "/var/run/docker.sock".into(),
            uptime_path: "/proc/uptime".into(),
            snapshots: Arc::default(),
            requests: Arc::new(Semaphore::new(4)),
            api: Arc::default(),
        }
    }
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/", get(web::index))
        .route("/containers/{id}", get(web::container_detail))
        .route("/containers/{id}/logs", get(web::container_logs))
        .route(
            "/style.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../static/style.css"),
                )
            }),
        )
        .route("/api/overview", get(overview))
        .route("/usage", get(web::usage))
        .route("/api/usage", get(usage))
        .route("/api/containers/{id}", get(container_json))
        .route("/api/containers/{id}/logs", get(logs))
        .layer(axum::middleware::map_response(
            |mut response: Response| async move {
                let headers = response.headers_mut();
                headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
                headers.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
                headers.insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
                headers.insert("x-robots-tag", "noindex, nofollow".parse().unwrap());
                headers.insert(
                    header::CONTENT_SECURITY_POLICY,
                    "default-src 'none'; style-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"
                        .parse()
                        .unwrap(),
                );
                response
            },
        ))
        .with_state(state)
}

type ApiResult<T> = Result<Json<T>, ApiError>;
struct ApiError(StatusCode, String);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}
fn unavailable(message: impl Into<String>) -> ApiError {
    ApiError(StatusCode::SERVICE_UNAVAILABLE, message.into())
}

struct Snapshot {
    id: String,
    name: String,
    until: String,
    lines: Vec<String>,
    created: Instant,
}

#[derive(Serialize)]
struct Container {
    id: String,
    name: String,
    image: String,
    version: String,
    image_id: Option<String>,
    image_created_at: Option<String>,
    status: String,
    started_at: Option<String>,
    finished_at: Option<String>,
    exit_code: Option<i64>,
    uptime_seconds: Option<i64>,
    #[serde(flatten)]
    diagnostics: Value,
}
fn image_version(image: &str) -> String {
    if image.starts_with("sha256:") || valid_id(image) {
        return "untagged".into();
    }
    if let Some((_, digest)) = image.split_once('@') {
        return digest.into();
    }
    image
        .rsplit('/')
        .next()
        .and_then(|name| name.split_once(':'))
        .map(|(_, tag)| tag.to_owned())
        .unwrap_or_else(|| "latest (implicit)".into())
}
fn container(value: &Value, images: &[Value]) -> Container {
    let image_id = value["Image"].as_str();
    let image_created_at = image_id
        .and_then(|id| images.iter().find(|image| image["Id"].as_str() == Some(id)))
        .and_then(|image| image["Created"].as_str())
        .and_then(|created| DateTime::parse_from_rfc3339(created).ok())
        .map(|created| {
            created
                .with_timezone(&Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
        });
    let image = value["Config"]["Image"]
        .as_str()
        .unwrap_or("unknown")
        .to_owned();
    let status = value["State"]["Status"].as_str().unwrap_or("unknown");
    let parse_timestamp = |field: &str| {
        value["State"][field]
            .as_str()
            .filter(|date| !date.starts_with("0001-"))
            .and_then(|date| DateTime::parse_from_rfc3339(date).ok())
            .map(|date| date.with_timezone(&Utc))
    };
    let started = parse_timestamp("StartedAt");
    let uptime_seconds = started
        .filter(|_| matches!(status, "running" | "paused"))
        .map(|start| (Utc::now() - start).num_seconds().max(0));
    let finished_at = parse_timestamp("FinishedAt")
        .map(|date| date.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true));
    let started_at = started.map(|date| date.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true));
    let exit_code = matches!(status, "exited" | "dead")
        .then(|| value["State"]["ExitCode"].as_i64())
        .flatten();
    Container {
        id: value["Id"].as_str().unwrap_or_default().into(),
        name: value["Name"]
            .as_str()
            .unwrap_or("unnamed")
            .trim_start_matches('/')
            .into(),
        version: image_version(&image),
        image_id: image_id.map(str::to_owned),
        image_created_at,
        image,
        status: status.into(),
        started_at,
        finished_at,
        exit_code,
        uptime_seconds,
        diagnostics: diagnostics::container_details(value),
    }
}
async fn inspect_image(state: &AppState, id: &str) -> Result<Value, ApiError> {
    let digest = id.strip_prefix("sha256:").unwrap_or(id);
    if digest.is_empty() || digest.len() > 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(unavailable("Docker returned an invalid image ID"));
    }
    engine::json(state, &format!("/images/{id}/json")).await
}
async fn container_json(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult<Value> {
    if !valid_id(&id) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Invalid container ID".into(),
        ));
    }
    let inspected = engine::json(&state, &format!("/containers/{id}/json")).await?;
    let mut images = Vec::new();
    if let Some(id) = inspected["Image"].as_str() {
        match inspect_image(&state, id).await {
            Ok(image) => images.push(image),
            Err(ApiError(StatusCode::NOT_FOUND, _)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(Json(json!(container(&inspected, &images))))
}
async fn container_list(state: &AppState, all: bool) -> Result<Vec<Value>, ApiError> {
    let value = engine::json(
        state,
        if all {
            "/containers/json?all=1"
        } else {
            "/containers/json"
        },
    )
    .await?;
    let list = value
        .as_array()
        .ok_or_else(|| unavailable("Expected a container list"))?;
    if list.iter().any(|c| !c["Id"].as_str().is_some_and(valid_id)) {
        return Err(unavailable("Docker returned an invalid container ID"));
    }
    Ok(list.clone())
}
async fn overview(State(state): State<AppState>) -> ApiResult<Value> {
    let (version, list, host) = tokio::try_join!(
        engine::version(&state),
        container_list(&state, true),
        engine::json(&state, "/info")
    )?;
    let mut pending = JoinSet::new();
    for c in list {
        let state = state.clone();
        let id = c["Id"].as_str().unwrap().to_owned();
        pending.spawn(async move { engine::json(&state, &format!("/containers/{id}/json")).await });
    }
    let mut inspected = Vec::new();
    while let Some(result) = pending.join_next().await {
        match result.map_err(|_| unavailable("Container inspection interrupted"))? {
            Ok(value) => inspected.push(value),
            Err(ApiError(StatusCode::NOT_FOUND, _)) => {} // Removed during collection.
            Err(error) => return Err(error),
        }
    }
    let mut image_ids: Vec<_> = inspected
        .iter()
        .filter_map(|c| c["Image"].as_str())
        .collect();
    image_ids.sort_unstable();
    image_ids.dedup();
    let mut pending = JoinSet::new();
    for id in image_ids {
        let state = state.clone();
        let id = id.to_owned();
        pending.spawn(async move { inspect_image(&state, &id).await });
    }
    let mut images = Vec::new();
    while let Some(result) = pending.join_next().await {
        match result.map_err(|_| unavailable("Image inspection interrupted"))? {
            Ok(value) => images.push(value),
            Err(ApiError(StatusCode::NOT_FOUND, _)) => {}
            Err(error) => return Err(error),
        }
    }
    let mut containers: Vec<_> = inspected
        .iter()
        .map(|value| container(value, &images))
        .collect();
    containers.sort_by(|a, b| a.name.cmp(&b.name));
    let uptime = tokio::fs::read_to_string(&state.uptime_path)
        .await
        .ok()
        .and_then(|text| text.split_whitespace().next()?.parse::<f64>().ok());
    Ok(Json(
        json!({"docker_version": version["Version"], "host": diagnostics::host_details(&host),
        "host_uptime_seconds": uptime, "containers": containers,
        "checked_at": Utc::now().to_rfc3339()}),
    ))
}
async fn usage(State(state): State<AppState>) -> ApiResult<Value> {
    let list = container_list(&state, false).await?;
    let mut pending = JoinSet::new();
    for c in list {
        let state = state.clone();
        pending.spawn(async move {
            let id = c["Id"].as_str().unwrap();
            let stats =
                engine::json(&state, &format!("/containers/{id}/stats?stream=false")).await?;
            Ok::<_, ApiError>(diagnostics::usage(&c, &stats))
        });
    }
    let mut containers = Vec::new();
    while let Some(result) = pending.join_next().await {
        match result.map_err(|_| unavailable("Usage collection interrupted"))? {
            Ok(value) => containers.push(value),
            Err(ApiError(StatusCode::NOT_FOUND | StatusCode::CONFLICT, _)) => {}
            Err(error) => return Err(error),
        }
    }
    containers.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(Json(
        json!({"checked_at": Utc::now().to_rfc3339(), "containers": containers}),
    ))
}

#[derive(Default, Deserialize)]
struct LogQuery {
    page: Option<usize>,
    until: Option<String>,
}
fn valid_id(id: &str) -> bool {
    (12..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit())
}
fn paginate(mut lines: Vec<String>, page: usize) -> (Vec<String>, bool) {
    // Docker pads timestamps to nanoseconds, so lexical order is chronological.
    lines.sort_by(|a, b| {
        a.split_once(' ')
            .map(|v| v.0)
            .cmp(&b.split_once(' ').map(|v| v.0))
    });
    let end = lines.len().saturating_sub(page * PAGE_SIZE);
    let start = end.saturating_sub(PAGE_SIZE);
    let older = start > 0 && page < MAX_PAGE;
    (lines[start..end].to_vec(), older)
}
async fn logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LogQuery>,
) -> ApiResult<Value> {
    let page = query.page.unwrap_or(0);
    if !valid_id(&id) || page > MAX_PAGE {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Invalid container ID or page (allowed: 0–99)".into(),
        ));
    }
    let (until, name, lines) = if let Some(until) = query.until {
        let mut snapshots = state.snapshots.lock().unwrap();
        snapshots.retain(|s| s.created.elapsed() < Duration::from_secs(300));
        let snapshot = snapshots
            .iter()
            .find(|s| s.until == until && s.id == id)
            .ok_or_else(|| {
                ApiError(
                    StatusCode::GONE,
                    "Log snapshot expired. Select Latest to start again.".into(),
                )
            })?;
        (until, snapshot.name.clone(), snapshot.lines.clone())
    } else {
        if page != 0 {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "History pages require a snapshot timestamp".into(),
            ));
        }
        // Capture once: Docker may apply --tail before --until. Re-reading for
        // each page could otherwise skip entries as new logs arrive.
        let until = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let tail = (MAX_PAGE + 1) * PAGE_SIZE + 1;
        let inspected = engine::json(&state, &format!("/containers/{id}/json")).await?;
        let secrets = diagnostics::secret_values(std::slice::from_ref(&inspected));
        let name = inspected["Name"]
            .as_str()
            .unwrap_or("unnamed")
            .trim_start_matches('/')
            .to_owned();
        let output = engine::get(
            &state,
            &format!("/containers/{id}/logs?stdout=1&stderr=1&timestamps=1&tail={tail}"),
        )
        .await?;
        let streams = engine::log_streams(
            &output,
            inspected["Config"]["Tty"].as_bool().unwrap_or(false),
        )?;
        let lines: Vec<String> = streams
            .iter()
            .flat_map(|bytes| {
                diagnostics::redact(&String::from_utf8_lossy(bytes), &secrets)
                    .lines()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect();
        let mut snapshots = state.snapshots.lock().unwrap();
        snapshots.retain(|s| s.created.elapsed() < Duration::from_secs(300));
        while snapshots.len() >= 8 {
            snapshots.pop_front();
        }
        snapshots.push_back(Snapshot {
            id,
            name: name.clone(),
            until: until.clone(),
            lines: lines.clone(),
            created: Instant::now(),
        });
        (until, name, lines)
    };
    let (lines, has_older) = paginate(lines, page);
    Ok(Json(
        json!({"name": name, "lines": lines, "page": page, "until": until, "has_older": has_older, "history_limit_reached": page == MAX_PAGE}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn versions_handle_registry_ports_and_digests() {
        assert_eq!(image_version("localhost:5000/team/app:1.2"), "1.2");
        assert_eq!(
            image_version("localhost:5000/team/app"),
            "latest (implicit)"
        );
        assert_eq!(image_version("app@sha256:abc"), "sha256:abc");
    }
    #[test]
    fn pages_have_no_overlap_even_with_identical_timestamps() {
        let lines: Vec<_> = (0..251)
            .map(|i| format!("2026-01-01T00:00:00.000000000Z line {i}"))
            .collect();
        let (latest, older) = paginate(lines.clone(), 0);
        assert_eq!(latest, lines[151..]);
        assert!(older);
        let (second, older) = paginate(lines.clone(), 1);
        assert_eq!(second, lines[51..151]);
        assert!(older);
        let (last, older) = paginate(lines.clone(), 2);
        assert_eq!(last, lines[..51]);
        assert!(!older);
        assert!(paginate(lines, 3).0.is_empty());
    }
}
