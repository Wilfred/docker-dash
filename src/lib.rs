use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

const PAGE_SIZE: usize = 100;
const MAX_PAGE: usize = 99;

#[derive(Clone)]
pub struct AppState {
    pub docker: String,
    pub uptime_path: String,
    snapshots: Arc<Mutex<VecDeque<Snapshot>>>,
}
impl Default for AppState {
    fn default() -> Self {
        Self {
            docker: "docker".into(),
            uptime_path: "/proc/uptime".into(),
            snapshots: Arc::default(),
        }
    }
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../static/index.html")) }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../static/app.js"),
                )
            }),
        )
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
        .route("/api/containers/{id}/logs", get(logs))
        .layer(axum::middleware::map_response(
            |mut response: Response| async move {
                let headers = response.headers_mut();
                headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
                headers.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
                headers.insert(
                    header::CONTENT_SECURITY_POLICY,
                    "default-src 'self'; frame-ancestors 'none'; base-uri 'none'"
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
    until: String,
    lines: Vec<String>,
    created: Instant,
}

async fn read_output(reader: impl tokio::io::AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    const MAX_BYTES: u64 = 4 * 1024 * 1024;
    let mut bytes = Vec::new();
    reader.take(MAX_BYTES + 1).read_to_end(&mut bytes).await?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(std::io::Error::other(
            "Docker output exceeds 4 MiB per stream; narrow the container logs",
        ));
    }
    Ok(bytes)
}

async fn docker(state: &AppState, args: &[&str]) -> Result<std::process::Output, ApiError> {
    let mut cmd = Command::new(&state.docker);
    cmd.args(args).stdin(Stdio::null()).kill_on_drop(true);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| unavailable(format!("Cannot run Docker CLI: {e}")))?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (status, stdout, stderr) = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::try_join!(child.wait(), read_output(stdout), read_output(stderr))
    })
    .await
    .map_err(|_| unavailable("Docker command timed out after 15 seconds"))?
    .map_err(|e| unavailable(format!("Cannot read Docker output: {e}")))?;
    let result = std::process::Output {
        status,
        stdout,
        stderr,
    };
    if !result.status.success() {
        let message = String::from_utf8_lossy(&result.stderr);
        return Err(unavailable(format!(
            "Docker: {}",
            message.chars().take(2000).collect::<String>().trim()
        )));
    }
    Ok(result)
}
async fn docker_json(state: &AppState, args: &[&str]) -> Result<Value, ApiError> {
    serde_json::from_slice(&docker(state, args).await?.stdout)
        .map_err(|e| unavailable(format!("Invalid Docker response: {e}")))
}

#[derive(Serialize)]
struct Container {
    id: String,
    name: String,
    image: String,
    version: String,
    status: String,
    started_at: String,
    uptime_seconds: i64,
}
fn image_version(image: &str) -> String {
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
fn container(value: &Value) -> Container {
    let image = value["Config"]["Image"]
        .as_str()
        .unwrap_or("unknown")
        .to_owned();
    let started_at = value["State"]["StartedAt"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    let uptime_seconds = DateTime::parse_from_rfc3339(&started_at)
        .map(|start| {
            (Utc::now() - start.with_timezone(&Utc))
                .num_seconds()
                .max(0)
        })
        .unwrap_or(0);
    Container {
        id: value["Id"].as_str().unwrap_or_default().into(),
        name: value["Name"]
            .as_str()
            .unwrap_or("unnamed")
            .trim_start_matches('/')
            .into(),
        version: image_version(&image),
        image,
        status: value["State"]["Status"]
            .as_str()
            .unwrap_or("unknown")
            .into(),
        started_at,
        uptime_seconds,
    }
}
async fn overview(State(state): State<AppState>) -> ApiResult<Value> {
    let (version, ids) = tokio::try_join!(
        docker_json(&state, &["version", "--format", "{{json .}}"]),
        docker(&state, &["ps", "--quiet", "--no-trunc"])
    )?;
    let ids = String::from_utf8_lossy(&ids.stdout);
    let mut containers = Vec::new();
    if !ids.trim().is_empty() {
        let mut args = vec!["container", "inspect"];
        args.extend(ids.split_whitespace());
        let inspected = docker_json(&state, &args).await?;
        containers = inspected
            .as_array()
            .ok_or_else(|| unavailable("Expected a container array"))?
            .iter()
            .map(container)
            .collect();
        containers.sort_by(|a, b| a.name.cmp(&b.name));
    }
    let uptime = tokio::fs::read_to_string(&state.uptime_path)
        .await
        .ok()
        .and_then(|text| text.split_whitespace().next()?.parse::<f64>().ok());
    Ok(Json(json!({
        "docker_version": version["Server"]["Version"],
        "host_uptime_seconds": uptime,
        "containers": containers,
        "checked_at": Utc::now().to_rfc3339(),
    })))
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
    let (until, lines) = if let Some(until) = query.until {
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
        (until, snapshot.lines.clone())
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
        let tail = ((MAX_PAGE + 1) * PAGE_SIZE + 1).to_string();
        let output = docker(&state, &["logs", "--timestamps", "--tail", &tail, &id]).await?;
        let lines: Vec<String> = [&output.stdout, &output.stderr]
            .iter()
            .flat_map(|bytes| {
                String::from_utf8_lossy(bytes)
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
            until: until.clone(),
            lines: lines.clone(),
            created: Instant::now(),
        });
        (until, lines)
    };
    let (lines, has_older) = paginate(lines, page);
    Ok(Json(
        json!({"lines": lines, "page": page, "until": until, "has_older": has_older, "history_limit_reached": page == MAX_PAGE}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn command_output_is_bounded() {
        assert_eq!(read_output(&b"hello"[..]).await.unwrap(), b"hello");
        let oversized = vec![b'x'; 4 * 1024 * 1024 + 1];
        assert!(read_output(oversized.as_slice()).await.is_err());
    }
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
