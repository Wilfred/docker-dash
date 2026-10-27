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
    let mut content = format!(
        "<h2>Host</h2><dl><dt>Docker version</dt><dd>{}</dd><dt>Host uptime</dt><dd>{}</dd><dt>Checked at (UTC)</dt><dd>{}</dd></dl>\
         <p><a href=\"/\">Refresh</a></p><h2>Containers ({})</h2>",
        text(&data["docker_version"]),
        duration(&data["host_uptime_seconds"]),
        text(&data["checked_at"]),
        containers.len()
    );
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
        content.push_str(&format!(
            "<section class=\"container\" aria-label=\"Container {}\"><h3>{}</h3><dl>\
             <dt>ID</dt><dd>{id}</dd>\
             <dt>Image</dt><dd>{}</dd>\
             <dt>Version / tag</dt><dd>{}</dd>\
             <dt>Image built (UTC)</dt><dd>{}</dd>\
             <dt>Status</dt><dd>{}</dd>\
             <dt>Started at (UTC)</dt><dd>{started_at}</dd>\
             <dt>Finished at (UTC)</dt><dd>{finished_at}</dd>\
             <dt>Exit code</dt><dd>{exit_code}</dd>\
             <dt>Uptime</dt><dd>{uptime}</dd></dl>\
             <p><a href=\"/containers/{id}/logs\">Logs for {}</a> · <a href=\"/api/containers/{id}/logs\">Logs JSON for {}</a></p></section>",
            text(&c["name"]), text(&c["name"]), text(&c["image"]), text(&c["version"]), text(&c["image_created_at"]), text(&c["status"]), text(&c["name"]), text(&c["name"])
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
         <p>Oldest first. Up to 100 lines per page. Latest captures fresh output.</p><nav aria-label=\"Log pages\">{navigation}</nav>{output}{limit}",
            page + 1,
            lines.len()
        ),
    )
}
