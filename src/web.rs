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
    let mut content = "<h4>Diagnostics</h4><dl>".to_owned();
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
    content.push_str("</dl><h4>Ports</h4>");
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
    content.push_str("<h4>Networks</h4>");
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

fn document(status: StatusCode, content: String) -> Response {
    (
        status,
        Html(include_str!("../static/index.html").replace("{{content}}", &content)),
    )
        .into_response()
}
fn error(error: ApiError, recovery: &str) -> Response {
    document(
        error.0,
        format!(
            "<h2>Request failed</h2><p class=\"error\" role=\"alert\">{}</p>{recovery}",
            escape(&error.1)
        ),
    )
}

pub(super) async fn index(State(state): State<AppState>) -> Response {
    let data = match overview(State(state)).await {
        Ok(Json(data)) => data,
        Err(err) => return error(err, "<p><a href=\"/\">Retry overview</a></p>"),
    };
    let containers = data["containers"].as_array().unwrap();
    let mut content = "<h2>Host</h2><dl>".to_owned();
    field(
        &mut content,
        "Docker version",
        &scalar(&data["docker_version"]),
    );
    field(
        &mut content,
        "Host uptime",
        &duration(&data["host_uptime_seconds"]),
    );
    field(
        &mut content,
        "Checked at (UTC)",
        &scalar(&data["checked_at"]),
    );
    for (label, key) in [
        ("Docker host", "name"),
        ("Operating system", "operating_system"),
        ("Kernel", "kernel_version"),
        ("Architecture", "architecture"),
        ("Host CPUs", "cpus"),
    ] {
        field(&mut content, label, &scalar(&data["host"][key]));
    }
    field(
        &mut content,
        "Host memory",
        &bytes(&data["host"]["memory_bytes"]),
    );
    content.push_str(&format!(
        "</dl><p><a href=\"/\">Refresh</a></p><h2>Containers ({})</h2>",
        containers.len()
    ));
    if containers.is_empty() {
        content.push_str("<p>No containers.</p>");
    }
    for c in containers {
        let id = text(&c["id"]);
        let started_at = c["started_at"]
            .as_str()
            .map(escape)
            .unwrap_or_else(|| "Never started".into());
        let finished_at = c["finished_at"]
            .as_str()
            .map(escape)
            .unwrap_or_else(|| "Not finished".into());
        let exit_code = c["exit_code"]
            .as_i64()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "Not exited".into());
        let uptime = if c["uptime_seconds"].is_null() {
            "Not running".into()
        } else {
            duration(&c["uptime_seconds"])
        };
        let details = diagnostics_fields(c);
        let log_links = if data["logs_enabled"] == true {
            format!(
                "<p><a href=\"/containers/{id}/logs\">Logs for {}</a> · <a href=\"/api/containers/{id}/logs\">Logs JSON for {}</a></p>",
                text(&c["name"]),
                text(&c["name"])
            )
        } else {
            "<p>Log access disabled.</p>".into()
        };
        content.push_str(&format!(
            "<section class=\"container\" aria-label=\"Container {}\"><h3>{}</h3><dl>\
             <dt>ID</dt><dd>{id}</dd>\
             <dt>Image</dt><dd>{}</dd>\
             <dt>Image ID</dt><dd>{}</dd>\
             <dt>Version / tag</dt><dd>{}</dd>\
             <dt>Image built (UTC)</dt><dd>{}</dd>\
             <dt>Status</dt><dd>{}</dd>\
             <dt>Started at (UTC)</dt><dd>{started_at}</dd>\
             <dt>Finished at (UTC)</dt><dd>{finished_at}</dd>\
             <dt>Exit code</dt><dd>{exit_code}</dd>\
             <dt>Uptime</dt><dd>{uptime}</dd></dl>{details}\
             {log_links}</section>",
            text(&c["name"]),
            text(&c["name"]),
            text(&c["image"]),
            text(&c["image_id"]),
            text(&c["version"]),
            text(&c["image_created_at"]),
            text(&c["status"])
        ));
    }
    document(StatusCode::OK, content)
}

pub(super) async fn container_logs(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LogQuery>,
) -> Response {
    // Validate before placing the ID into any links, including error recovery.
    if !valid_id(&id) {
        return error(
            ApiError(StatusCode::BAD_REQUEST, "Invalid container ID".into()),
            "",
        );
    }
    let latest = format!("<a href=\"/containers/{id}/logs\">Latest</a>");
    let data = match logs(State(state), Path(id.clone()), Query(query)).await {
        Ok(Json(data)) => data,
        Err(err) => return error(err, &format!("<p>{latest}</p>")),
    };
    let page = data["page"].as_u64().unwrap();
    let until = text(&data["until"]);
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
        StatusCode::OK,
        format!(
            "<h2>Container logs</h2><dl><dt>Container ID</dt><dd>{id}</dd><dt>Snapshot (UTC)</dt><dd>{until}</dd><dt>Page</dt><dd>{} ({} lines)</dd></dl>\
         <p>Oldest first. Up to 100 lines per page. Latest captures fresh output. Known environment secrets are masked; other sensitive application data may remain.</p><nav aria-label=\"Log pages\">{navigation}</nav>{output}{limit}",
            page + 1,
            lines.len()
        ),
    )
}

pub(super) async fn usage(State(state): State<AppState>) -> Response {
    let data = match super::usage(State(state)).await {
        Ok(Json(data)) => data,
        Err(err) => return error(err, "<p><a href=\"/usage\">Retry usage</a></p>"),
    };
    let mut content = format!(
        "<h2>Resource usage</h2><p>Sampled at (UTC): {}. Network and block I/O are cumulative counters.</p><p><a href=\"/usage\">Refresh</a> · <a href=\"/api/usage\">Usage JSON</a></p><div class=\"table-wrap\"><table><thead><tr><th>Container</th><th>CPU</th><th>Memory used / limit</th><th>Memory %</th><th>Network RX / TX</th><th>Disk read / write</th><th>Processes</th></tr></thead><tbody>",
        text(&data["checked_at"])
    );
    let containers = data["containers"].as_array().unwrap();
    for c in containers {
        content.push_str("<tr>");
        for field in [
            "name",
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
    document(StatusCode::OK, content)
}
