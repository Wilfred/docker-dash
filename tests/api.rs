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
info) echo '{{"Name":"test-host","OperatingSystem":"Debian","KernelVersion":"6.1","Architecture":"x86_64","NCPU":4,"MemTotal":8589934592,"DriverStatus":[["private","HOST_SECRET"]]}}';;
stats) echo '{{"ID":"aaaaaaaaaaaa","Name":"web","CPUPerc":"1.25%","MemUsage":"32MiB / 1GiB","MemPerc":"3.12%","NetIO":"1kB / 2kB","BlockIO":"0B / 4kB","PIDs":"3","Extra":"STATS_SECRET"}}';;
version) printf '%s\n' '{{"Server":{{"Version":"29.0.0"}}}}';;
ps)
  [ "$2" = "--all" ] || exit 1
  {ps} ;;
container) printf '%s\n' '[{{"Id":"aaaaaaaaaaaa","Name":"/web","Image":"sha256:abc123","Config":{{"Image":"registry:5000/web:2.1"}},"State":{{"Status":"running","StartedAt":"2026-01-01T00:00:00Z"}}}}]';;
image)
  [ "$2" = "inspect" ] && [ "$3" = "sha256:abc123" ] || exit 1
  printf '%s\n' '[{{"Id":"sha256:newtag","Created":"2026-10-27T10:00:00Z"}},{{"Id":"sha256:abc123","Created":"2025-12-01T13:30:00+01:00"}}]';;
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
    assert_eq!(data["containers"][0]["image_id"], "sha256:abc123");
    assert_eq!(
        data["containers"][0]["image_created_at"],
        "2025-12-01T12:30:00Z"
    );
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
    for path in ["/", "/style.css"] {
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

async fn html(app: Router, url: &str) -> (StatusCode, String) {
    let response = app
        .oneshot(Request::builder().uri(url).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn overview_html_contains_data_without_javascript() {
    let f = Fixture::new("running");
    let (status, body) = html(f.app(), "/").await;
    assert_eq!(status, StatusCode::OK);
    for expected in [
        "29.0.0",
        "90061 seconds",
        "Containers (1)",
        "aaaaaaaaaaaa",
        "registry:5000/web:2.1",
        "Image built (UTC)</dt><dd>2025-12-01T12:30:00Z",
        "2026-01-01T00:00:00Z",
        "href=\"/containers/aaaaaaaaaaaa/logs\"",
        "href=\"/api/overview\"",
    ] {
        assert!(body.contains(expected), "missing {expected}");
    }
    assert!(!body.contains("<script"));
    assert!(!body.contains("<input"));
    let (_, empty) = html(Fixture::new("empty").app(), "/").await;
    assert!(empty.contains("No containers."));
}

#[tokio::test]
async fn html_log_links_preserve_snapshot_and_expose_json() {
    let f = Fixture::new("running");
    let app = f.app();
    let (status, first) = html(app.clone(), "/containers/aaaaaaaaaaaa/logs").await;
    assert_eq!(status, StatusCode::OK);
    assert!(first.contains("Page</dt><dd>1 (100 lines)"));
    let older = first
        .split("href=\"")
        .find_map(|part| {
            let (url, rest) = part.split_once('"')?;
            rest.starts_with(">Older</a>")
                .then(|| url.replace("&amp;", "&"))
        })
        .unwrap();
    let (status, second) = html(app.clone(), &older).await;
    assert_eq!(status, StatusCode::OK);
    assert!(second.contains("Page</dt><dd>2 (100 lines)"));
    let json_url = second
        .split("href=\"")
        .find_map(|part| {
            let (url, rest) = part.split_once('"')?;
            rest.starts_with(">This page as JSON</a>")
                .then(|| url.replace("&amp;", "&"))
        })
        .unwrap();
    let (status, data) = get(app.clone(), &json_url).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data["page"], 1);
    assert_eq!(data["lines"].as_array().unwrap().len(), 100);
    let (status, expired) = html(app, "/containers/aaaaaaaaaaaa/logs?until=expired").await;
    assert_eq!(status, StatusCode::GONE);
    assert!(expired.contains("Log snapshot expired"));
    assert!(expired.contains("href=\"/containers/aaaaaaaaaaaa/logs\">Latest"));
}

#[tokio::test]
async fn html_escapes_metadata_logs_and_errors() {
    let f = Fixture::new("running");
    let path = f.0.join("docker");
    let script = std::fs::read_to_string(&path)
        .unwrap()
        .replace("/web\"", "/<script>alert(1)</script>\"");
    std::fs::write(&path, script).unwrap();
    let (_, overview) = html(f.app(), "/").await;
    assert!(overview.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(!overview.contains("<script>"));
    std::fs::write(
        &path,
        "#!/bin/sh\ncase \"$1\" in\ncontainer) echo '[]';;\nlogs) printf '%s\\n' '2026-01-01T00:00:00Z <script>alert(1)</script>';;\nesac\n",
    )
    .unwrap();
    let (_, logs) = html(f.app(), "/containers/aaaaaaaaaaaa/logs").await;
    assert!(logs.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(!logs.contains("<script>"));
    std::fs::write(
        &path,
        "#!/bin/sh\necho '<script>denied</script>' >&2\nexit 1\n",
    )
    .unwrap();
    let (status, error) = html(f.app(), "/").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(error.contains("&lt;script&gt;denied&lt;/script&gt;"));
    assert!(error.contains("Retry overview"));
}

#[tokio::test]
async fn latest_images_show_actual_image_creation_time() {
    let f = Fixture::new("running");
    let path = f.0.join("docker");
    let script = std::fs::read_to_string(&path)
        .unwrap()
        .replace("web:2.1", "web:latest");
    std::fs::write(&path, &script).unwrap();
    let (_, data) = get(f.app(), "/api/overview").await;
    assert_eq!(data["containers"][0]["version"], "latest");
    assert_eq!(
        data["containers"][0]["image_created_at"],
        "2025-12-01T12:30:00Z"
    );
    for missing in ["", "invalid"] {
        std::fs::write(&path, script.replace("2025-12-01T13:30:00+01:00", missing)).unwrap();
        let (status, data) = get(f.app(), "/api/overview").await;
        assert_eq!(status, StatusCode::OK);
        assert!(data["containers"][0]["image_created_at"].is_null());
        let (_, page) = html(f.app(), "/").await;
        assert!(page.contains("Image built (UTC)</dt><dd>Unavailable"));
    }
}

#[tokio::test]
async fn all_container_states_are_visible_with_accurate_lifecycle_data() {
    let f = Fixture::new("running");
    let records = serde_json::json!([
        {"Id":"aaaaaaaaaaaa","Name":"/running","Image":"sha256:abc123","Config":{"Image":"web:latest"},"State":{"Status":"running","StartedAt":"2026-01-01T00:00:00Z","FinishedAt":"0001-01-01T00:00:00Z","ExitCode":0}},
        {"Id":"bbbbbbbbbbbb","Name":"/created","Image":"sha256:missing","Config":{"Image":"old-image"},"State":{"Status":"created","StartedAt":"0001-01-01T00:00:00Z","FinishedAt":"0001-01-01T00:00:00Z","ExitCode":0}},
        {"Id":"cccccccccccc","Name":"/exited","Image":"sha256:abc123","Config":{"Image":"web:latest"},"State":{"Status":"exited","StartedAt":"2025-01-01T00:00:00Z","FinishedAt":"2025-01-01T00:00:05Z","ExitCode":2}}
    ]);
    let script = format!(
        r#"#!/bin/sh
case "$1" in
info) echo '{{"Name":"test-host"}}';;
version) echo '{{"Server":{{"Version":"29.0.0"}}}}';;
ps)
  [ "$2" = "--all" ] || exit 1
  printf '%s\n' aaaaaaaaaaaa bbbbbbbbbbbb cccccccccccc;;
container) printf '%s\n' '{records}';;
image)
  [ "$3" = "sha256:abc123" ] && [ "$4" = "sha256:missing" ] && [ "$#" -eq 4 ] || exit 1
  echo '[{{"Id":"sha256:abc123","Created":"2025-01-01T00:00:00Z"}}]'
  echo 'No such image: sha256:missing' >&2
  exit 1;;
esac
"#
    );
    std::fs::write(f.0.join("docker"), script).unwrap();
    let app = f.app();
    let (status, data) = get(app.clone(), "/api/overview").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data["containers"].as_array().unwrap().len(), 3);
    let created = &data["containers"][0];
    assert_eq!(created["status"], "created");
    for field in [
        "started_at",
        "finished_at",
        "exit_code",
        "uptime_seconds",
        "image_created_at",
    ] {
        assert!(created[field].is_null(), "{field}");
    }
    let exited = &data["containers"][1];
    assert_eq!(exited["status"], "exited");
    assert_eq!(exited["exit_code"], 2);
    assert_eq!(exited["finished_at"], "2025-01-01T00:00:05Z");
    assert!(exited["uptime_seconds"].is_null());
    let running = &data["containers"][2];
    assert!(running["uptime_seconds"].as_i64().unwrap() > 0);
    assert!(running["exit_code"].is_null());
    let (status, page) = html(app, "/").await;
    assert_eq!(status, StatusCode::OK);
    for expected in [
        "Containers (3)",
        ">created<",
        ">exited<",
        ">running<",
        "Never started",
        "Not running",
        "Exit code</dt><dd>2",
        "Finished at (UTC)</dt><dd>2025-01-01T00:00:05Z",
        "href=\"/containers/bbbbbbbbbbbb/logs\"",
    ] {
        assert!(page.contains(expected), "missing {expected}");
    }
    assert!(!page.contains("0001-01-01"));
}

impl Fixture {
    fn set_inspect(&self, records: Value) {
        let path = self.0.join("docker");
        let script = std::fs::read_to_string(&path).unwrap();
        let quoted = records.to_string().replace('\'', "'\"'\"'");
        let replacement = format!("container) printf '%s\\n' '{quoted}';;");
        let script = script
            .lines()
            .map(|line| {
                if line.starts_with("container)") {
                    replacement.as_str()
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(path, script).unwrap();
    }
}

fn diagnostic_records() -> Value {
    serde_json::json!([{
        "Id":"aaaaaaaaaaaa", "Name":"/web", "Image":"sha256:abc123", "Created":"2026-01-01T00:00:00Z", "RestartCount":7,
        "Config":{"Image":"web:latest", "Env":["API_KEY=ENV_SECRET", "DATABASE_URL=postgres://user:DB_PASSWORD@db/app", "PUBLIC_VALUE=UNEXPOSED_ENV"], "Cmd":["COMMAND_SECRET"], "Labels":{"key":"LABEL_SECRET"}, "Healthcheck":{"Test":["HEALTH_COMMAND_SECRET"]}, "ExposedPorts":{"80/tcp":{},"53/udp":{}}},
        "State":{"Status":"running","StartedAt":"2026-01-01T00:00:00Z","OOMKilled":true,"Error":"startup failed ENV_SECRET", "Health":{"Status":"unhealthy", "FailingStreak":3, "Log":[{"Output":"HEALTH_OUTPUT_SECRET"}]}},
        "HostConfig":{"NetworkMode":"bridge", "PortBindings":{"80/tcp":[{"HostIp":"127.0.0.1","HostPort":"8080"}],"443/tcp":[{"HostIp":"::","HostPort":"8443"}]}, "RestartPolicy":{"Name":"on-failure","MaximumRetryCount":5}, "Memory":536870912, "NanoCpus":1500000000, "CpuQuota":0,"CpuPeriod":0,"PidsLimit":100,"LogConfig":{"Type":"json-file","Config":{"private":"LOG_OPTIONS_SECRET"}}, "Binds":["MOUNT_PATH_SECRET"]},
        "NetworkSettings":{"Ports":{"80/tcp":[{"HostIp":"127.0.0.1","HostPort":"8080","Unexpected":"PORT_SECRET"}], "53/udp":null}, "Networks":{"frontend":{"IPAddress":"172.18.0.2","GlobalIPv6Address":"fd00::2","Unexpected":"NETWORK_SECRET"}}},
        "Mounts":[{"Source":"MOUNT_PATH_SECRET"}]
    }])
}

#[tokio::test]
async fn diagnostics_are_useful_without_exposing_inspect_secrets() {
    let f = Fixture::new("running");
    f.set_inspect(diagnostic_records());
    let (status, data) = get(f.app(), "/api/overview").await;
    assert_eq!(status, StatusCode::OK);
    let c = &data["containers"][0];
    assert_eq!(c["restart_count"], 7);
    assert_eq!(c["health_status"], "unhealthy");
    assert_eq!(c["health_failing_streak"], 3);
    assert_eq!(c["oom_killed"], true);
    assert_eq!(c["memory_limit_bytes"], 536870912);
    assert_eq!(c["nano_cpus"], 1500000000u64);
    assert_eq!(c["state_error"], "startup failed [REDACTED]");
    assert_eq!(c["ports"][0]["container_port"], "443/tcp");
    assert_eq!(c["ports"][0]["published"], serde_json::json!([]));
    assert_eq!(c["ports"][0]["configured"][0]["host_port"], "8443");
    assert_eq!(c["ports"][2]["published"][0]["host_ip"], "127.0.0.1");
    assert_eq!(c["networks"][0]["ipv4_address"], "172.18.0.2");
    assert_eq!(data["host"]["cpus"], 4);
    let (_, html) = html(f.app(), "/").await;
    for expected in [
        "unhealthy",
        "Restarts",
        "127.0.0.1",
        "8080",
        "8443",
        "not currently published",
        "exposed only",
        "172.18.0.2",
        "512.0 MiB",
        "1.5 CPUs",
        "[REDACTED]",
    ] {
        assert!(html.contains(expected), "missing {expected}");
    }
    for secret in [
        "ENV_SECRET",
        "DB_PASSWORD",
        "UNEXPOSED_ENV",
        "COMMAND_SECRET",
        "LABEL_SECRET",
        "HEALTH_COMMAND_SECRET",
        "HEALTH_OUTPUT_SECRET",
        "LOG_OPTIONS_SECRET",
        "MOUNT_PATH_SECRET",
        "PORT_SECRET",
        "NETWORK_SECRET",
        "HOST_SECRET",
    ] {
        assert!(!data.to_string().contains(secret), "JSON exposed {secret}");
        assert!(!html.contains(secret), "HTML exposed {secret}");
    }
}

#[tokio::test]
async fn logs_mask_known_environment_secrets_before_caching() {
    let f = Fixture::new("running");
    f.set_inspect(diagnostic_records());
    let path = f.0.join("docker");
    let script = std::fs::read_to_string(&path).unwrap();
    let before_logs = script.split("logs)\n").next().unwrap();
    std::fs::write(&path, format!("{before_logs}logs) printf '%s\\n' '2026-01-01T00:00:00Z ENV_SECRET postgres://user:DB_PASSWORD@db/app DB_PASSWORD' >&2;;\nesac\n")).unwrap();
    let app = f.app();
    let (status, data) = get(app.clone(), "/api/containers/aaaaaaaaaaaa/logs").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!data.to_string().contains("ENV_SECRET"));
    assert!(!data.to_string().contains("DB_PASSWORD"));
    assert_eq!(
        data["lines"][0],
        "2026-01-01T00:00:00Z [REDACTED] [REDACTED] [REDACTED]"
    );
    // A changed environment cannot expose original values from the cache.
    std::fs::write(path, "#!/bin/sh\nexit 1\n").unwrap();
    let url = format!(
        "/containers/aaaaaaaaaaaa/logs?until={}",
        data["until"].as_str().unwrap()
    );
    let (status, body) = html(app, &url).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("[REDACTED]"));
    assert!(!body.contains("ENV_SECRET"));
}

#[tokio::test]
async fn logs_can_be_disabled_for_tailnet_sharing() {
    let mut state = AppState::default();
    state.docker = "/no/docker/needed".into();
    state.logs_enabled = false;
    let app = app(state);
    assert_eq!(
        get(app.clone(), "/api/containers/aaaaaaaaaaaa/logs")
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        html(app, "/containers/aaaaaaaaaaaa/logs").await.0,
        StatusCode::FORBIDDEN
    );
    let f = Fixture::new("running");
    f.set_inspect(diagnostic_records());
    let mut state = AppState::default();
    state.docker = f.0.join("docker").to_string_lossy().into_owned();
    state.logs_enabled = false;
    let app = docker_dash::app(state);
    let (_, data) = get(app.clone(), "/api/overview").await;
    assert!(data["containers"][0]["state_error"].is_null());
    let (_, body) = html(app, "/").await;
    assert!(body.contains("Log access disabled."));
    assert!(!body.contains("href=\"/containers/aaaaaaaaaaaa/logs\""));
}

#[tokio::test]
async fn usage_returns_selected_fields_and_readable_html() {
    let f = Fixture::new("running");
    let (status, data) = get(f.app(), "/api/usage").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data["containers"][0]["cpu_percent"], "1.25%");
    assert_eq!(data["containers"][0]["pids"], "3");
    assert!(!data.to_string().contains("STATS_SECRET"));
    let (status, body) = html(f.app(), "/usage").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("32MiB / 1GiB"));
    assert!(!body.contains("STATS_SECRET"));
    std::fs::write(f.0.join("docker"), "#!/bin/sh\nexit 0\n").unwrap();
    assert_eq!(
        get(f.app(), "/api/usage").await.1["containers"],
        serde_json::json!([])
    );
}
