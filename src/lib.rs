use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::{StatusCode, header, uri::Authority},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::Utc;
use serde_json::json;
use std::{
    collections::VecDeque,
    net::IpAddr,
    sync::{Arc, Mutex},
};
use tokio::sync::{OnceCell, Semaphore};
use tokio::task::JoinSet;

mod diagnostics;
mod docker;
mod engine;
mod logs;
mod web;

pub use engine::Endpoint;

#[derive(Clone)]
pub struct AppState {
    pub docker: Endpoint,
    pub uptime_path: String,
    /// Host names accepted in addition to localhost, IP addresses, and `*.ts.net`.
    pub allowed_hosts: Vec<String>,
    snapshots: Arc<Mutex<VecDeque<logs::Snapshot>>>,
    requests: Arc<Semaphore>,
    api: Arc<OnceCell<(String, Option<String>)>>,
}
impl Default for AppState {
    fn default() -> Self {
        Self {
            docker: Endpoint::default(),
            uptime_path: "/proc/uptime".into(),
            allowed_hosts: Vec::new(),
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
        .route("/api/containers/{id}/logs", get(logs::logs))
        .layer(middleware::from_fn_with_state(state.clone(), check_host))
        .layer(middleware::map_response(
            |mut response: Response| async move {
                let headers = response.headers_mut();
                headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
                headers.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
                headers.insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
                headers.insert("x-robots-tag", "noindex, nofollow".parse().unwrap());
                headers.insert(
                    header::CONTENT_SECURITY_POLICY,
                    "default-src 'none'; style-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"
                        .parse()
                        .unwrap(),
                );
                response
            },
        ))
        .with_state(state)
}

/// Rejects unexpected Host headers so DNS rebinding cannot reach the dashboard
/// through a browser. Rebinding needs a domain name, so IP addresses are safe.
fn host_allowed(authority: &str, allowed: &[String]) -> bool {
    let Ok(authority) = authority.parse::<Authority>() else {
        return false;
    };
    let host = authority.host().trim_end_matches('.').to_ascii_lowercase();
    let ip = host.trim_start_matches('[').trim_end_matches(']');
    host == "localhost"
        || ip.parse::<IpAddr>().is_ok()
        || host.ends_with(".ts.net")
        || allowed.iter().any(|name| name.eq_ignore_ascii_case(&host))
}
async fn check_host(State(state): State<AppState>, request: Request, next: Next) -> Response {
    // Browsers always send Host, so requests without one cannot be rebinding.
    let host = request
        .headers()
        .get(header::HOST)
        .map(|host| host.to_str().unwrap_or_default());
    if host.is_some_and(|host| !host_allowed(host, &state.allowed_hosts)) {
        return (
            StatusCode::FORBIDDEN,
            "Host not allowed. Add it to DOCKER_DASH_ALLOWED_HOSTS.\n",
        )
            .into_response();
    }
    next.run(request).await
}

type ApiResult<T> = Result<Json<T>, ApiError>;
#[derive(Debug)]
struct ApiError(StatusCode, String);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}
fn unavailable(message: impl Into<String>) -> ApiError {
    ApiError(StatusCode::SERVICE_UNAVAILABLE, message.into())
}
fn valid_id(id: &str) -> bool {
    (12..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit())
}

async fn inspect_image(state: &AppState, id: &str) -> Result<docker::ImageInspect, ApiError> {
    let digest = id.strip_prefix("sha256:").unwrap_or(id);
    if digest.is_empty() || digest.len() > 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(unavailable("Docker returned an invalid image ID"));
    }
    engine::json(state, &format!("/images/{id}/json")).await
}
async fn container_json(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<diagnostics::Container> {
    if !valid_id(&id) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Invalid container ID".into(),
        ));
    }
    let inspected: docker::ContainerInspect =
        engine::json(&state, &format!("/containers/{id}/json")).await?;
    let mut images = Vec::new();
    if let Some(id) = &inspected.image {
        match inspect_image(&state, id).await {
            Ok(image) => images.push(image),
            Err(ApiError(StatusCode::NOT_FOUND, _)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(Json(diagnostics::container(&inspected, &images)))
}
async fn container_list(
    state: &AppState,
    all: bool,
) -> Result<Vec<docker::ContainerSummary>, ApiError> {
    let list: Vec<docker::ContainerSummary> = engine::json(
        state,
        if all {
            "/containers/json?all=1"
        } else {
            "/containers/json"
        },
    )
    .await?;
    if list.iter().any(|c| !valid_id(&c.id)) {
        return Err(unavailable("Docker returned an invalid container ID"));
    }
    Ok(list)
}
/// Runs one Docker request per item concurrently, skipping items Docker reports
/// as missing (or, for stats, stopped) since they changed during collection.
async fn collect<T: Send + 'static>(
    items: impl IntoIterator<Item = impl Future<Output = Result<T, ApiError>> + Send + 'static>,
) -> Result<Vec<T>, ApiError> {
    let mut pending = JoinSet::new();
    for item in items {
        pending.spawn(item);
    }
    let mut results = Vec::new();
    while let Some(result) = pending.join_next().await {
        match result.map_err(|_| unavailable("Docker collection interrupted"))? {
            Ok(value) => results.push(value),
            Err(ApiError(StatusCode::NOT_FOUND | StatusCode::CONFLICT, _)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(results)
}
async fn overview(State(state): State<AppState>) -> ApiResult<diagnostics::Overview> {
    let (docker_version, list, host) = tokio::try_join!(
        engine::version(&state),
        container_list(&state, true),
        engine::json::<docker::Info>(&state, "/info")
    )?;
    let inspected: Vec<docker::ContainerInspect> = collect(list.into_iter().map(|c| {
        let state = state.clone();
        async move { engine::json(&state, &format!("/containers/{}/json", c.id)).await }
    }))
    .await?;
    let mut image_ids: Vec<_> = inspected.iter().filter_map(|c| c.image.clone()).collect();
    image_ids.sort_unstable();
    image_ids.dedup();
    let images = collect(image_ids.into_iter().map(|id| {
        let state = state.clone();
        async move { inspect_image(&state, &id).await }
    }))
    .await?;
    let mut containers: Vec<_> = inspected
        .iter()
        .map(|c| diagnostics::container(c, &images))
        .collect();
    containers.sort_by(|a, b| a.name.cmp(&b.name));
    let host_uptime_seconds = tokio::fs::read_to_string(&state.uptime_path)
        .await
        .ok()
        .and_then(|text| text.split_whitespace().next()?.parse::<f64>().ok());
    Ok(Json(diagnostics::Overview {
        docker_version,
        host: host.into(),
        host_uptime_seconds,
        containers,
        checked_at: Utc::now().to_rfc3339(),
    }))
}
async fn usage(State(state): State<AppState>) -> ApiResult<diagnostics::Usage> {
    let list = container_list(&state, false).await?;
    let mut containers = collect(list.into_iter().map(|c| {
        let state = state.clone();
        async move {
            let stats: docker::Stats =
                engine::json(&state, &format!("/containers/{}/stats?stream=false", c.id)).await?;
            Ok(diagnostics::usage(&c, &stats))
        }
    }))
    .await?;
    containers.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(diagnostics::Usage {
        checked_at: Utc::now().to_rfc3339(),
        containers,
    }))
}

#[cfg(test)]
mod tests {
    use super::host_allowed;
    #[test]
    fn hosts_must_be_local_addresses_tailnet_or_configured() {
        let allowed = ["dash.example.com".to_owned()];
        for host in [
            "localhost:8080",
            "127.0.0.1:8080",
            "[::1]:8080",
            "100.64.0.1",
            "server.tail1234.ts.net",
            "DASH.example.com.",
        ] {
            assert!(host_allowed(host, &allowed), "{host}");
        }
        for host in [
            "attacker.example",
            "localhost.attacker.example",
            "ts.net.evil",
            "",
        ] {
            assert!(!host_allowed(host, &allowed), "{host}");
        }
    }
}
