//! HTML views of the JSON API. Templates escape every value they render.
use super::{
    ApiError, AppState, container_json,
    diagnostics::{Binding, Container},
    docker, engine,
    logs::{self, LogQuery},
    overview, valid_id,
};
use askama::Template;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};

type Fields = Vec<(&'static str, String)>;

fn or_unavailable<T: ToString>(value: Option<T>) -> String {
    value.map_or_else(|| "Unavailable".into(), |v| v.to_string())
}
fn duration(seconds: u64) -> String {
    format!(
        "{}d {}h {}m {}s ({seconds} seconds)",
        seconds / 86400,
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60
    )
}
fn short_duration(seconds: u64) -> String {
    if seconds >= 86400 {
        format!("{}d {}h", seconds / 86400, seconds / 3600 % 24)
    } else if seconds >= 3600 {
        format!("{}h {}m", seconds / 3600, seconds / 60 % 60)
    } else if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}
fn human_bytes(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", units[unit])
}
fn pair(a: Option<u64>, b: Option<u64>) -> String {
    or_unavailable(
        a.zip(b)
            .map(|(a, b)| format!("{} / {}", human_bytes(a), human_bytes(b))),
    )
}
fn percent(value: Option<f64>) -> String {
    or_unavailable(value.map(|p| format!("{p:.2}%")))
}

/// Names the Docker host in page titles, so tabs for several hosts are distinct.
async fn title(state: &AppState) -> String {
    match engine::json::<docker::Info>(state, "/info").await {
        Ok(docker::Info {
            name: Some(name), ..
        }) if !name.is_empty() => format!("docker-dash - {name}"),
        _ => "docker-dash".into(),
    }
}
fn render(status: StatusCode, template: impl Template) -> Response {
    match template.render() {
        Ok(html) => (status, Html(html)).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response(),
    }
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorPage {
    title: String,
    message: String,
    recovery: Option<(String, &'static str)>,
}
async fn error(
    state: &AppState,
    error: ApiError,
    recovery: Option<(String, &'static str)>,
) -> Response {
    render(
        error.0,
        ErrorPage {
            title: title(state).await,
            message: error.1,
            recovery,
        },
    )
}

struct ContainerRow {
    id: String,
    name: String,
    status: String,
    image: String,
    image_created_at: String,
    uptime: String,
}
#[derive(Template)]
#[template(path = "overview.html")]
struct OverviewPage {
    title: String,
    host: Fields,
    containers: Vec<ContainerRow>,
}

pub(super) async fn index(State(state): State<AppState>) -> Response {
    let data = match overview(State(state.clone())).await {
        Ok(Json(data)) => data,
        Err(err) => return error(&state, err, Some(("/".into(), "Retry overview"))).await,
    };
    let host = vec![
        ("Name", or_unavailable(data.host.name)),
        ("Docker version", or_unavailable(data.docker_version)),
        (
            "Host uptime",
            or_unavailable(
                data.host_uptime_seconds
                    .filter(|s| s.is_finite() && *s >= 0.0)
                    .map(|s| short_duration(s as u64)),
            ),
        ),
        ("Checked at (UTC)", data.checked_at),
    ];
    let containers = data
        .containers
        .into_iter()
        .map(|c| {
            let mut status = c.status;
            if let Some(code) = c.exit_code {
                status.push_str(&format!(" (exit {code})"));
            }
            if let Some(health) = c.health_status.filter(|health| health != "healthy") {
                status.push_str(&format!(" / {health}"));
            }
            if c.oom_killed == Some(true) {
                status.push_str(" / OOM killed");
            }
            ContainerRow {
                id: c.id,
                name: c.name,
                status,
                image: c.image,
                image_created_at: or_unavailable(c.image_created_at),
                uptime: c
                    .uptime_seconds
                    .map_or("Not running".into(), |s| short_duration(s.max(0) as u64)),
            }
        })
        .collect();
    render(
        StatusCode::OK,
        OverviewPage {
            title: title(&state).await,
            host,
            containers,
        },
    )
}

#[derive(Template)]
#[template(path = "container.html")]
struct ContainerPage {
    title: String,
    id: String,
    name: String,
    summary: Fields,
    diagnostics: Fields,
    host_networking: bool,
    ports: Vec<String>,
    networks: Vec<String>,
}

fn binding_text(bindings: &[Binding]) -> String {
    bindings
        .iter()
        .map(|binding| {
            let ip = Some(binding.host_ip.as_str())
                .filter(|ip| !ip.is_empty())
                .unwrap_or("any address");
            let port = Some(binding.host_port.as_str())
                .filter(|port| !port.is_empty())
                .unwrap_or("automatic port");
            format!("[{ip}]:{port}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}
fn diagnostics_fields(c: &Container) -> Fields {
    let health = match (&c.health_status, c.healthcheck_configured) {
        (Some(status), _) => status.clone(),
        (None, true) => "Not reported yet".into(),
        (None, false) => "Not configured".into(),
    };
    let cpu = match (c.nano_cpus, c.cpu_quota, c.cpu_period) {
        (Some(n), _, _) if n > 0 => format!("{} CPUs", n as f64 / 1_000_000_000.0),
        (_, Some(quota), Some(period)) if quota > 0 && period > 0 => {
            format!("{} CPUs", quota as f64 / period as f64)
        }
        (Some(0), _, _) => "No limit".into(),
        _ => "Unavailable".into(),
    };
    let mut fields = vec![
        ("Created at (UTC)", or_unavailable(c.created_at.as_ref())),
        ("Health", health),
        ("Health failures", or_unavailable(c.health_failing_streak)),
        ("OOM killed", or_unavailable(c.oom_killed)),
        ("Restarts", or_unavailable(c.restart_count)),
        ("Restart policy", or_unavailable(c.restart_policy.as_ref())),
        ("Maximum retries", or_unavailable(c.restart_max_retries)),
        ("Network mode", or_unavailable(c.network_mode.as_ref())),
        ("Log driver", or_unavailable(c.log_driver.as_ref())),
    ];
    if let Some(error) = &c.state_error {
        fields.push(("Last Docker error", error.clone()));
    }
    fields.extend([
        (
            "Memory limit",
            match c.memory_limit_bytes {
                Some(0) => "No limit".into(),
                Some(bytes) => format!("{:.1} MiB ({bytes} bytes)", bytes as f64 / 1048576.0),
                None => "Unavailable".into(),
            },
        ),
        ("CPU limit", cpu),
        (
            "Process limit",
            match c.pids_limit {
                Some(n) if n > 0 => n.to_string(),
                Some(_) => "No limit".into(),
                None => "Default".into(),
            },
        ),
    ]);
    fields
}

pub(super) async fn container_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let c = match container_json(State(state.clone()), Path(id)).await {
        Ok(Json(data)) => data,
        Err(err) => return error(&state, err, Some(("/".into(), "Host overview"))).await,
    };
    let summary = vec![
        ("ID", c.id.clone()),
        ("Image", c.image.clone()),
        ("Image ID", or_unavailable(c.image_id.as_ref())),
        ("Version / tag", c.version.clone()),
        (
            "Image built (UTC)",
            or_unavailable(c.image_created_at.as_ref()),
        ),
        ("Status", c.status.clone()),
        (
            "Started at (UTC)",
            c.started_at
                .clone()
                .unwrap_or_else(|| "Never started".into()),
        ),
        (
            "Finished at (UTC)",
            c.finished_at
                .clone()
                .unwrap_or_else(|| "Not finished".into()),
        ),
        (
            "Exit code",
            c.exit_code.map_or("Not exited".into(), |n| n.to_string()),
        ),
        (
            "Uptime",
            c.uptime_seconds
                .map_or("Not running".into(), |s| duration(s.max(0) as u64)),
        ),
    ];
    let ports = c
        .ports
        .iter()
        .map(|port| {
            let state = if !port.published.is_empty() {
                format!("published on {}", binding_text(&port.published))
            } else if !port.configured.is_empty() {
                format!(
                    "configured on {}; not currently published",
                    binding_text(&port.configured)
                )
            } else {
                "exposed only; not published".into()
            };
            format!("{}: {state}", port.container_port)
        })
        .collect();
    let networks = c
        .networks
        .iter()
        .map(|network| {
            let addresses: Vec<_> = [&network.ipv4_address, &network.ipv6_address]
                .into_iter()
                .flatten()
                .filter(|s| !s.is_empty())
                .map(String::as_str)
                .collect();
            if addresses.is_empty() {
                format!("{}: No container IP", network.name)
            } else {
                format!("{}: {}", network.name, addresses.join(", "))
            }
        })
        .collect();
    render(
        StatusCode::OK,
        ContainerPage {
            title: title(&state).await,
            diagnostics: diagnostics_fields(&c),
            host_networking: c.network_mode.as_deref() == Some("host"),
            id: c.id,
            name: c.name,
            summary,
            ports,
            networks,
        },
    )
}

#[derive(Template)]
#[template(path = "logs.html")]
struct LogsPage {
    title: String,
    id: String,
    name: String,
    until: String,
    since: Option<String>,
    since_input: String,
    page: usize,
    navigation: Vec<(String, &'static str)>,
    lines: Vec<String>,
    history_limit_reached: bool,
}

fn logs_link(prefix: &str, id: &str, query: &[(&str, String)]) -> String {
    let query = serde_urlencoded::to_string(query).unwrap();
    if query.is_empty() {
        format!("{prefix}/containers/{id}/logs")
    } else {
        format!("{prefix}/containers/{id}/logs?{query}")
    }
}

pub(super) async fn container_logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LogQuery>,
) -> Response {
    // Validate before placing the ID into any links, including error recovery.
    if !valid_id(&id) {
        return error(
            &state,
            ApiError(StatusCode::BAD_REQUEST, "Invalid container ID".into()),
            None,
        )
        .await;
    }
    // Links keep the requested since, so Latest re-resolves durations like 15m.
    let since_input = query.since.clone().unwrap_or_default();
    let since_query: Vec<_> = query
        .since
        .iter()
        .filter(|since| !since.trim().is_empty())
        .map(|since| ("since", since.clone()))
        .collect();
    let data = match logs::logs(State(state.clone()), Path(id.clone()), Query(query)).await {
        Ok(Json(data)) => data,
        Err(err) => {
            let latest = logs_link("", &id, &[]);
            return error(&state, err, Some((latest, "Latest"))).await;
        }
    };
    let page_link = |page: usize| {
        let mut query = vec![("page", page.to_string()), ("until", data.until.clone())];
        query.extend(since_query.iter().cloned());
        query
    };
    let mut navigation = vec![(logs_link("", &id, &since_query), "Latest")];
    if data.page > 0 {
        navigation.push((logs_link("", &id, &page_link(data.page - 1)), "Newer"));
    }
    if data.has_older {
        navigation.push((logs_link("", &id, &page_link(data.page + 1)), "Older"));
    }
    navigation.push((
        logs_link("/api", &id, &page_link(data.page)),
        "This page as JSON",
    ));
    render(
        StatusCode::OK,
        LogsPage {
            title: title(&state).await,
            id,
            name: data.name,
            until: data.until,
            since: data.since,
            since_input,
            page: data.page + 1,
            navigation,
            lines: data.lines,
            history_limit_reached: data.history_limit_reached,
        },
    )
}

#[derive(Template)]
#[template(path = "usage.html")]
struct UsagePage {
    title: String,
    checked_at: String,
    rows: Vec<UsageRow>,
}
struct UsageRow {
    id: String,
    name: String,
    cells: [String; 6],
}

pub(super) async fn usage(State(state): State<AppState>) -> Response {
    let data = match super::usage(State(state.clone())).await {
        Ok(Json(data)) => data,
        Err(err) => return error(&state, err, Some(("/usage".into(), "Retry usage"))).await,
    };
    let rows = data
        .containers
        .into_iter()
        .map(|c| UsageRow {
            cells: [
                percent(c.cpu_percent),
                pair(c.memory_usage_bytes, c.memory_limit_bytes),
                percent(c.memory_percent),
                pair(c.network_rx_bytes, c.network_tx_bytes),
                pair(c.block_read_bytes, c.block_write_bytes),
                or_unavailable(c.pids),
            ],
            id: c.id,
            name: c.name,
        })
        .collect();
    render(
        StatusCode::OK,
        UsagePage {
            title: title(&state).await,
            checked_at: data.checked_at,
            rows,
        },
    )
}
