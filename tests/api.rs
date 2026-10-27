#![cfg(unix)]
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use docker_dash::{AppState, app};
use http_body_util::BodyExt;
use serde_json::Value;
use std::{
    os::unix::fs::PermissionsExt,
    sync::atomic::{AtomicUsize, Ordering},
};
use tower::ServiceExt;

struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new(mode: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "docker-dash-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = format!(
            r#"#!/bin/sh
case "$1" in
version) printf '%s\n' '{{"Server":{{"Version":"29.0.0"}}}}';;
ps) {ps} ;;
container) printf '%s\n' '[{{"Id":"aaaaaaaaaaaa","Name":"/web","Config":{{"Image":"registry:5000/web:2.1"}},"State":{{"Status":"running","StartedAt":"2026-01-01T00:00:00Z"}}}}]';;
logs)
  if [ -f "{dir}/read" ]; then echo 'should only capture logs once' >&2; exit 1; fi
  touch "{dir}/read"
  i=0
  while [ "$i" -lt 251 ]; do
    if [ "$((i % 2))" -eq 0 ]; then
      printf '2026-01-01T00:00:%02d.000000000Z line %s\n' "$((i / 5))" "$i"
    else
      printf '2026-01-01T00:00:%02d.000000000Z line %s\n' "$((i / 5))" "$i" >&2
    fi
    i=$((i + 1))
  done;;
esac
"#,
            ps = if mode == "empty" {
                "true"
            } else {
                "echo aaaaaaaaaaaa"
            },
            dir = dir.display()
        );
        std::fs::write(dir.join("docker"), script).unwrap();
        std::fs::set_permissions(dir.join("docker"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::write(dir.join("uptime"), "90061.5 1234.0\n").unwrap();
        Self(dir)
    }
    fn app(&self) -> Router {
        let mut state = AppState::default();
        state.docker = self.0.join("docker").to_string_lossy().into_owned();
        state.uptime_path = self.0.join("uptime").to_string_lossy().into_owned();
        app(state)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
async fn get(app: Router, url: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(Request::builder().uri(url).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}
#[tokio::test]
async fn overview_reads_docker_and_host() {
    let f = Fixture::new("running");
    let (status, data) = get(f.app(), "/api/overview").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data["docker_version"], "29.0.0");
    assert_eq!(data["host_uptime_seconds"], 90061.5);
    assert_eq!(data["containers"][0]["name"], "web");
    assert_eq!(data["containers"][0]["version"], "2.1");
    assert!(data["containers"][0]["uptime_seconds"].as_i64().unwrap() > 0);
}
#[tokio::test]
async fn empty_host_is_successful() {
    let f = Fixture::new("empty");
    let (status, data) = get(f.app(), "/api/overview").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data["containers"], serde_json::json!([]));
}
#[tokio::test]
async fn logs_merge_streams_and_page_through_one_snapshot() {
    let f = Fixture::new("running");
    let app = f.app();
    let (status, first) = get(app.clone(), "/api/containers/aaaaaaaaaaaa/logs").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["lines"].as_array().unwrap().len(), 100);
    assert_eq!(first["has_older"], true);
    let until = first["until"].as_str().unwrap();
    let (_, second) = get(
        app.clone(),
        &format!("/api/containers/aaaaaaaaaaaa/logs?page=1&until={until}"),
    )
    .await;
    assert_eq!(second["lines"].as_array().unwrap().len(), 100);
    let (_, third) = get(
        app.clone(),
        &format!("/api/containers/aaaaaaaaaaaa/logs?page=2&until={until}"),
    )
    .await;
    assert_eq!(third["lines"].as_array().unwrap().len(), 51);
    assert_eq!(third["has_older"], false);
    let mut all = third["lines"].as_array().unwrap().clone();
    all.extend(second["lines"].as_array().unwrap().clone());
    all.extend(first["lines"].as_array().unwrap().clone());
    let unique: std::collections::HashSet<_> = all.iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(unique.len(), 251);
    assert_eq!(
        get(app, "/api/containers/bbbbbbbbbbbb/logs?until=missing")
            .await
            .0,
        StatusCode::GONE
    );
}
#[tokio::test]
async fn invalid_queries_and_docker_failures_are_clear() {
    let f = Fixture::new("running");
    for url in [
        "/api/containers/--help/logs",
        "/api/containers/aaaaaaaaaaaa/logs?page=100",
        "/api/containers/aaaaaaaaaaaa/logs?page=1",
    ] {
        assert_eq!(get(f.app(), url).await.0, StatusCode::BAD_REQUEST);
    }
    std::fs::write(
        f.0.join("docker"),
        "#!/bin/sh\necho 'permission denied' >&2\nexit 1\n",
    )
    .unwrap();
    let (status, data) = get(f.app(), "/api/overview").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        data["error"]
            .as_str()
            .unwrap()
            .contains("permission denied")
    );
}
#[tokio::test]
async fn assets_are_embedded_and_protected() {
    let f = Fixture::new("empty");
    for path in ["/", "/app.js", "/style.css"] {
        let response = f
            .app()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert!(response.headers().contains_key("content-security-policy"));
        assert!(
            !response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .is_empty()
        );
    }
}
