use super::*;
use axum::response::Html;

// Escape every Docker-provided value, including log output and link attributes.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
fn text(value: &Value) -> String {
    escape(value.as_str().unwrap_or("Unavailable"))
}
fn duration(value: &Value) -> String {
    match value
        .as_f64()
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
    {
        Some(seconds) => {
            let seconds = seconds as u64;
            format!(
                "{}d {}h {}m {}s ({seconds} seconds)",
                seconds / 86400,
                seconds / 3600 % 24,
                seconds / 60 % 60,
                seconds % 60
            )
        }
        None => "Unavailable".into(),
    }
}
fn field(content: &mut String, label: &str, value: &str) {
    content.push_str(&format!(
        "<dt>{}</dt><dd>{}</dd>",
        escape(label),
        escape(value)
    ));
}
fn scalar(value: &Value) -> String {
    match value {
        Value::Null => "Unavailable".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
fn bytes(value: &Value) -> String {
    match value.as_u64() {
        Some(0) => "No limit".into(),
        Some(bytes) => format!("{:.1} MiB ({bytes} bytes)", bytes as f64 / 1048576.0),
        None => "Unavailable".into(),
    }
}
fn diagnostics_fields(c: &Value) -> String {
    let mut content = "<h3>Diagnostics</h3><dl>".to_owned();
    for (label, key) in [
        ("Created at (UTC)", "created_at"),
        ("Health", "health_status"),
        ("Health failures", "health_failing_streak"),
        ("OOM killed", "oom_killed"),
        ("Restarts", "restart_count"),
        ("Restart policy", "restart_policy"),
        ("Maximum retries", "restart_max_retries"),
        ("Network mode", "network_mode"),
        ("Log driver", "log_driver"),
    ] {
        let value = if key == "health_status" && c[key].is_null() {
            if c["healthcheck_configured"] == true {
                "Not reported yet".into()
            } else {
                "Not configured".into()
            }
        } else {
            scalar(&c[key])
        };
        field(&mut content, label, &value);
    }
    if let Some(error) = c["state_error"].as_str() {
        field(&mut content, "Last Docker error", error);
    }
    field(
        &mut content,
        "Memory limit",
        &bytes(&c["memory_limit_bytes"]),
    );
    let cpu = match c["nano_cpus"].as_u64() {
        Some(n) if n > 0 => format!("{} CPUs", n as f64 / 1_000_000_000.0),
        _ => match (c["cpu_quota"].as_i64(), c["cpu_period"].as_u64()) {
            (Some(quota), Some(period)) if quota > 0 && period > 0 => {
                format!("{} CPUs", quota as f64 / period as f64)
            }
            _ if c["nano_cpus"].as_u64() == Some(0) => "No limit".into(),
            _ => "Unavailable".into(),
        },
    };
    field(&mut content, "CPU limit", &cpu);
    let pids = match c["pids_limit"].as_i64() {
        Some(n) if n > 0 => n.to_string(),
        Some(_) => "No limit".into(),
        None => "Default".into(),
    };
    field(&mut content, "Process limit", &pids);
    content.push_str("</dl><h3>Ports</h3>");
    if c["network_mode"] == "host" {
        content.push_str("<p>Host networking: listens directly on the host. Docker does not report port bindings.</p>");
    }
    let ports = c["ports"].as_array().unwrap();
    if ports.is_empty() {
        content.push_str("<p>None declared or published.</p>");
    } else {
        content.push_str("<ul>");
        for port in ports {
            let published = port["published"].as_array().unwrap();
            let configured = port["configured"].as_array().unwrap();
            let bindings = if published.is_empty() {
                configured
            } else {
                published
            };
            let binding_text = bindings
                .iter()
                .map(|binding| {
                    let ip = binding["host_ip"]
                        .as_str()
                        .filter(|ip| !ip.is_empty())
                        .unwrap_or("any address");
                    let port = binding["host_port"]
                        .as_str()
                        .filter(|port| !port.is_empty())
                        .unwrap_or("automatic port");
                    format!("[{ip}]:{port}")
                })
                .collect::<Vec<_>>()
                .join(", ");
            let state = if !published.is_empty() {
                format!("published on {binding_text}")
            } else if !configured.is_empty() {
                format!("configured on {binding_text}; not currently published")
            } else {
                "exposed only; not published".into()
            };
            content.push_str(&format!(
                "<li>{}: {}</li>",
                text(&port["container_port"]),
                escape(&state)
            ));
        }
        content.push_str("</ul>");
    }
    content.push_str("<h3>Networks</h3>");
    let networks = c["networks"].as_array().unwrap();
    if networks.is_empty() {
        content.push_str("<p>No network attachments.</p>");
    } else {
        content.push_str("<ul>");
        for network in networks {
            let addresses: Vec<_> = ["ipv4_address", "ipv6_address"]
                .iter()
                .filter_map(|key| network[*key].as_str().filter(|s| !s.is_empty()))
                .collect();
            content.push_str(&format!(
                "<li>{}: {}</li>",
                text(&network["name"]),
                if addresses.is_empty() {
                    "No container IP".into()
                } else {
                    escape(&addresses.join(", "))
                }
            ));
        }
        content.push_str("</ul>");
    }
    content
}

async fn document(state: &AppState, status: StatusCode, content: String) -> Response {
    let title = match engine::json(state, "/info").await {
        Ok(host) => host["Name"]
            .as_str()
            .filter(|name| !name.is_empty())
            .map(|name| format!("docker-dash - {}", escape(name)))
            .unwrap_or_else(|| "docker-dash".into()),
        Err(_) => "docker-dash".into(),
    };
    (
        status,
        Html(
            include_str!("../static/index.html")
                .replace("{{title}}", &title)
                .replace("{{content}}", &content),
        ),
    )
        .into_response()
}
async fn error(state: &AppState, error: ApiError, recovery: &str) -> Response {
    document(
        state,
        error.0,
        format!(
            "<h2>Request failed</h2><p class=\"error\" role=\"alert\">{}</p>{recovery}",
            escape(&error.1)
        ),
    )
    .await
}

fn short_duration(value: &Value) -> String {
    let Some(seconds) = value.as_f64().filter(|n| n.is_finite() && *n >= 0.0) else {
        return "Not running".into();
    };
    let seconds = seconds as u64;
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

pub(super) async fn index(State(state): State<AppState>) -> Response {
    let data = match overview(State(state.clone())).await {
        Ok(Json(data)) => data,
        Err(err) => return error(&state, err, "<p><a href=\"/\">Retry overview</a></p>").await,
    };
    let mut content = "<h2>Host</h2><dl>".to_owned();
    field(&mut content, "Name", &scalar(&data["host"]["name"]));
    field(
        &mut content,
        "Docker version",
        &scalar(&data["docker_version"]),
    );
    field(
        &mut content,
        "Host uptime",
        &data["host_uptime_seconds"]
            .as_f64()
            .map(|_| short_duration(&data["host_uptime_seconds"]))
            .unwrap_or_else(|| "Unavailable".into()),
    );
    field(
        &mut content,
        "Checked at (UTC)",
        &scalar(&data["checked_at"]),
    );
    let containers = data["containers"].as_array().unwrap();
    content.push_str(&format!(
        "</dl><p><a href=\"/\">Refresh</a></p><h2>Containers ({})</h2>",
        containers.len()
    ));
    if containers.is_empty() {
        content.push_str("<p>No containers.</p>");
    } else {
        content.push_str("<div class=\"table-wrap\"><table><thead><tr><th scope=\"col\">Container</th><th scope=\"col\">Status</th><th scope=\"col\">Image</th><th scope=\"col\">Image built (UTC)</th><th scope=\"col\">Uptime</th><th scope=\"col\">Logs</th></tr></thead><tbody>");
        for c in containers {
            let id = text(&c["id"]);
            let mut status = scalar(&c["status"]);
            if let Some(code) = c["exit_code"].as_i64() {
                status.push_str(&format!(" (exit {code})"));
            }
            if let Some(health) = c["health_status"]
                .as_str()
                .filter(|health| *health != "healthy")
            {
                status.push_str(&format!(" / {health}"));
            }
            if c["oom_killed"] == true {
                status.push_str(" / OOM killed");
            }
            content.push_str(&format!("<tr><th scope=\"row\"><a href=\"/containers/{id}\">{}</a></th><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td><a href=\"/containers/{id}/logs\" aria-label=\"Logs for {}\">Logs</a></td></tr>", text(&c["name"]), escape(&status), text(&c["image"]), text(&c["image_created_at"]), short_duration(&c["uptime_seconds"]), text(&c["name"])));
        }
        content.push_str("</tbody></table></div>");
    }
    document(&state, StatusCode::OK, content).await
}

pub(super) async fn container_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let c = match container_json(State(state.clone()), Path(id)).await {
        Ok(Json(data)) => data,
        Err(err) => return error(&state, err, "<p><a href=\"/\">Host overview</a></p>").await,
    };
    let id = text(&c["id"]);
    let mut content = format!(
        "<h2>{}</h2><p><a href=\"/api/containers/{id}\">Container JSON</a> · <a href=\"/containers/{id}/logs\">Logs</a> · <a href=\"/api/containers/{id}/logs\">Logs JSON</a></p><dl>",
        text(&c["name"])
    );
    for (label, key) in [
        ("ID", "id"),
        ("Image", "image"),
        ("Image ID", "image_id"),
        ("Version / tag", "version"),
        ("Image built (UTC)", "image_created_at"),
        ("Status", "status"),
    ] {
        field(&mut content, label, &scalar(&c[key]));
    }
    field(
        &mut content,
        "Started at (UTC)",
        c["started_at"].as_str().unwrap_or("Never started"),
    );
    field(
        &mut content,
        "Finished at (UTC)",
        c["finished_at"].as_str().unwrap_or("Not finished"),
    );
    field(
        &mut content,
        "Exit code",
        &c["exit_code"]
            .as_i64()
            .map(|n| n.to_string())
            .unwrap_or_else(|| "Not exited".into()),
    );
    field(
        &mut content,
        "Uptime",
        &if c["uptime_seconds"].is_null() {
            "Not running".into()
        } else {
            duration(&c["uptime_seconds"])
        },
    );
    content.push_str("</dl>");
    content.push_str(&diagnostics_fields(&c));
    document(&state, StatusCode::OK, content).await
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
            "",
        )
        .await;
    }
    let latest = format!("<a href=\"/containers/{id}/logs\">Latest</a>");
    let data = match logs(State(state.clone()), Path(id.clone()), Query(query)).await {
        Ok(Json(data)) => data,
        Err(err) => return error(&state, err, &format!("<p>{latest}</p>")).await,
    };
    let page = data["page"].as_u64().unwrap();
    let until = text(&data["until"]);
    let name = text(&data["name"]);
    let mut navigation = latest;
    if page > 0 {
        navigation.push_str(&format!(
            " · <a href=\"/containers/{id}/logs?page={}&amp;until={until}\">Newer</a>",
            page - 1
        ));
    }
    if data["has_older"] == true {
        navigation.push_str(&format!(
            " · <a href=\"/containers/{id}/logs?page={}&amp;until={until}\">Older</a>",
            page + 1
        ));
    }
    navigation.push_str(&format!(" · <a href=\"/api/containers/{id}/logs?page={page}&amp;until={until}\">This page as JSON</a>"));
    let lines = data["lines"].as_array().unwrap();
    let output = if lines.is_empty() {
        "<p>No log output for this page.</p>".into()
    } else {
        format!(
            "<pre aria-label=\"Container log output\">{}</pre>",
            lines.iter().map(text).collect::<Vec<_>>().join("\n")
        )
    };
    let limit = if data["history_limit_reached"] == true {
        "<p>History limit reached (10,000 lines).</p>"
    } else {
        ""
    };
    document(
        &state,
        StatusCode::OK,
        format!(
            "<h2>Logs: {name}</h2><dl><dt>Container</dt><dd><a href=\"/containers/{id}\">{name}</a></dd><dt>Container ID</dt><dd>{id}</dd><dt>Snapshot (UTC)</dt><dd>{until}</dd><dt>Page</dt><dd>{} ({} lines)</dd></dl>\
         <p>Oldest first. Up to 100 lines per page. Latest captures fresh output. Known environment secrets are masked; other sensitive application data may remain.</p><nav aria-label=\"Log pages\">{navigation}</nav>{output}{limit}",
            page + 1,
            lines.len()
        ),
    )
    .await
}

pub(super) async fn usage(State(state): State<AppState>) -> Response {
    let data = match super::usage(State(state.clone())).await {
        Ok(Json(data)) => data,
        Err(err) => return error(&state, err, "<p><a href=\"/usage\">Retry usage</a></p>").await,
    };
    let mut content = format!(
        "<h2>Resource usage</h2><p>Sampled at (UTC): {}. Network and block I/O are cumulative counters.</p><p><a href=\"/usage\">Refresh</a> · <a href=\"/api/usage\">Usage JSON</a></p><div class=\"table-wrap\"><table><thead><tr><th>Container</th><th>CPU</th><th>Memory used / limit</th><th>Memory %</th><th>Network RX / TX</th><th>Disk read / write</th><th>Processes</th></tr></thead><tbody>",
        text(&data["checked_at"])
    );
    let containers = data["containers"].as_array().unwrap();
    for c in containers {
        content.push_str(&format!(
            "<tr><td><a href=\"/containers/{}\">{}</a></td>",
            text(&c["id"]),
            text(&c["name"])
        ));
        for field in [
            "cpu_percent",
            "memory_usage",
            "memory_percent",
            "network_io",
            "block_io",
            "pids",
        ] {
            content.push_str(&format!("<td>{}</td>", text(&c[field])));
        }
        content.push_str("</tr>");
    }
    content.push_str("</tbody></table></div>");
    if containers.is_empty() {
        content.push_str("<p>No running containers.</p>");
    }
    document(&state, StatusCode::OK, content).await
}
