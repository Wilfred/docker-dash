#![cfg(unix)]
use axum::{
    Router,
    body::Body,
    http::{Request, Response, StatusCode},
};
use docker_dash::{AppState, app};
use http_body_util::{BodyExt, Full};
use hyper::{body::Bytes, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::net::UnixListener;
use tower::ServiceExt;

type Reply = (StatusCode, Vec<u8>);
struct Mock {
    containers: Vec<Value>,
    images: Vec<Value>,
    host: Value,
    stats: Value,
    version: Value,
    logs: Vec<u8>,
    requests: Vec<String>,
    log_reads: usize,
    overrides: HashMap<String, Reply>,
}
fn frame(stream: u8, bytes: &[u8]) -> Vec<u8> {
    let mut output = vec![stream, 0, 0, 0];
    output.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    output.extend_from_slice(bytes);
    output
}
impl Mock {
    fn new() -> Self {
        let mut logs = Vec::new();
        for i in 0..251 {
            logs.extend(frame(
                if i % 2 == 0 { 1 } else { 2 },
                format!(
                    "2026-01-01T00:00:{:02}.000000000Z line {i} ENV_SECRET\n",
                    i / 5
                )
                .as_bytes(),
            ));
        }
        Self {
            containers: diagnostic_records().as_array().unwrap().clone(),
            images: vec![json!({"Id":"sha256:abc123", "Created":"2025-12-01T13:30:00+01:00"})],
            host: json!({"Name":"test-host", "OperatingSystem":"Debian", "KernelVersion":"6.1", "Architecture":"x86_64", "NCPU":4, "MemTotal":8589934592u64, "Private":"HOST_SECRET"}),
            stats: json!({"cpu_stats":{"cpu_usage":{"total_usage":20},"system_cpu_usage":2000,"online_cpus":4},"precpu_stats":{"cpu_usage":{"total_usage":10},"system_cpu_usage":1000},"memory_stats":{"usage":41943040,"limit":1073741824,"stats":{"inactive_file":8388608}},"pids_stats":{"current":3},"networks":{"eth0":{"rx_bytes":1024,"tx_bytes":2048}},"blkio_stats":{"io_service_bytes_recursive":[{"op":"Read","value":4096},{"op":"Write","value":8192}]},"Private":"STATS_SECRET"}),
            version: json!({"Version":"29.0.0","ApiVersion":"1.56","MinAPIVersion":"1.40"}),
            logs,
            requests: Vec::new(),
            log_reads: 0,
            overrides: HashMap::new(),
        }
    }
    fn reply(&mut self, uri: &str) -> Reply {
        self.requests.push(uri.into());
        if let Some(reply) = self.overrides.get(uri) {
            return reply.clone();
        }
        if uri == "/version" {
            return (StatusCode::OK, self.version.to_string().into_bytes());
        }
        let Some((_, route)) = uri
            .strip_prefix("/v1.")
            .and_then(|tail| tail.split_once('/'))
        else {
            return (StatusCode::BAD_REQUEST, b"bad API version".to_vec());
        };
        let path = format!("/{route}");
        if let Some(reply) = self.overrides.get(&path) {
            return reply.clone();
        }
        let value = if path == "/info" {
            Some(self.host.clone())
        } else if path.starts_with("/containers/json") {
            let all = path.ends_with("?all=1");
            Some(Value::Array(
                self.containers
                    .iter()
                    .filter(|c| all || c["State"]["Status"] == "running")
                    .map(|c| json!({"Id":c["Id"], "Names":[c["Name"]]}))
                    .collect(),
            ))
        } else if let Some(path) = path.strip_prefix("/containers/") {
            let (id, action) = path.split_once('/').unwrap();
            if let Some(c) = self.containers.iter().find(|c| c["Id"] == id) {
                if action == "json" {
                    Some(c.clone())
                } else if action == "stats?stream=false" {
                    Some(self.stats.clone())
                } else if action.starts_with("logs?") {
                    self.log_reads += 1;
                    return (StatusCode::OK, self.logs.clone());
                } else {
                    None
                }
            } else {
                None
            }
        } else if let Some(id) = path
            .strip_prefix("/images/")
            .and_then(|p| p.strip_suffix("/json"))
        {
            self.images.iter().find(|i| i["Id"] == id).cloned()
        } else {
            None
        };
        value
            .map(|v| (StatusCode::OK, v.to_string().into_bytes()))
            .unwrap_or((
                StatusCode::NOT_FOUND,
                b"{\"message\":\"DAEMON_SECRET\"}".to_vec(),
            ))
    }
}
struct Fixture {
    dir: std::path::PathBuf,
    mock: Arc<Mutex<Mock>>,
    server: tokio::task::JoinHandle<()>,
}
impl Fixture {
    async fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "dash-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("uptime"), "90061.5 1234\n").unwrap();
        let listener = UnixListener::bind(dir.join("docker.sock")).unwrap();
        let mock = Arc::new(Mutex::new(Mock::new()));
        let state = mock.clone();
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let state = state.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                        assert_eq!(request.method(), "GET");
                        let (status, body) =
                            state.lock().unwrap().reply(&request.uri().to_string());
                        async move {
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(status)
                                    .body(Full::new(Bytes::from(body)))
                                    .unwrap(),
                            )
                        }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        Self { dir, mock, server }
    }
    fn state(&self) -> AppState {
        let mut state = AppState::default();
        state.socket_path = self.dir.join("docker.sock").to_string_lossy().into_owned();
        state.uptime_path = self.dir.join("uptime").to_string_lossy().into_owned();
        state
    }
    fn app(&self) -> Router {
        app(self.state())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
async fn html(app: Router, uri: &str) -> (StatusCode, String) {
    let response = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}
async fn get(app: Router, uri: &str) -> (StatusCode, Value) {
    let (status, body) = html(app, uri).await;
    (status, serde_json::from_str(&body).unwrap())
}
fn link(body: &str, label: &str) -> String {
    body.split("href=\"")
        .find_map(|part| {
            let (url, rest) = part.split_once('"')?;
            rest.starts_with(&format!(">{label}</a>"))
                .then(|| url.replace("&amp;", "&"))
        })
        .unwrap()
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
async fn overview_includes_all_states_and_missing_images() {
    let f = Fixture::new().await;
    {
        let mut mock = f.mock.lock().unwrap();
        let mut created = mock.containers[0].clone();
        created["Id"] = json!("bbbbbbbbbbbb");
        created["Name"] = json!("/created");
        created["Image"] = json!("sha256:deadbeef");
        created["State"] = json!({"Status":"created","StartedAt":"0001-01-01T00:00:00Z","FinishedAt":"0001-01-01T00:00:00Z","ExitCode":0});
        let mut exited = created.clone();
        exited["Id"] = json!("cccccccccccc");
        exited["Name"] = json!("/exited");
        exited["State"] = json!({"Status":"exited","StartedAt":"2025-01-01T00:00:00Z","FinishedAt":"2025-01-01T00:00:05Z","ExitCode":2});
        mock.containers.extend([created, exited]);
    }
    let (status, data) = get(f.app(), "/api/overview").await;
    assert_eq!(status, StatusCode::OK, "{data}");
    assert_eq!(data["docker_version"], "29.0.0");
    assert_eq!(data["host_uptime_seconds"], 90061.5);
    let containers = data["containers"].as_array().unwrap();
    assert_eq!(containers.len(), 3);
    let created = &containers[0];
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
    let exited = &containers[1];
    assert_eq!(exited["exit_code"], 2);
    assert!(exited["uptime_seconds"].is_null());
    assert_eq!(exited["finished_at"], "2025-01-01T00:00:05Z");
    assert_eq!(containers[2]["image_created_at"], "2025-12-01T12:30:00Z");
    let (status, body) = html(f.app(), "/").await;
    assert_eq!(status, StatusCode::OK);
    for text in [
        "Containers (3)",
        "Never started",
        "Not running",
        "Exit code</dt><dd>2",
        "Image built (UTC)</dt><dd>2025-12-01T12:30:00Z",
    ] {
        assert!(body.contains(text), "{text}");
    }
    assert!(!body.contains("0001-01-01"));
    assert!(!body.contains("<script"));
    assert!(!body.contains("<input"));
    f.mock.lock().unwrap().containers.clear();
    assert!(html(f.app(), "/").await.1.contains("No containers."));
}
#[tokio::test]
async fn diagnostic_allowlist_excludes_sensitive_fields() {
    let f = Fixture::new().await;
    let (status, data) = get(f.app(), "/api/overview").await;
    assert_eq!(status, StatusCode::OK, "{data}");
    let c = &data["containers"][0];
    assert_eq!(c["restart_count"], 7);
    assert_eq!(c["health_status"], "unhealthy");
    assert_eq!(c["oom_killed"], true);
    assert_eq!(c["health_failing_streak"], 3);
    assert_eq!(c["memory_limit_bytes"], 536870912);
    assert_eq!(c["nano_cpus"], 1500000000u64);
    assert_eq!(c["state_error"], "startup failed [REDACTED]");
    assert_eq!(data["host"]["cpus"], 4);
    assert_eq!(c["ports"][0]["container_port"], "443/tcp");
    assert_eq!(c["ports"][0]["published"], json!([]));
    assert_eq!(c["ports"][2]["published"][0]["host_ip"], "127.0.0.1");
    assert_eq!(c["networks"][0]["ipv4_address"], "172.18.0.2");
    let (_, body) = html(f.app(), "/").await;
    for expected in [
        "unhealthy",
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
        assert!(body.contains(expected), "{expected}");
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
        assert!(!body.contains(secret), "HTML exposed {secret}");
    }
}
#[tokio::test]
async fn multiplexed_logs_are_redacted_before_snapshot_pagination() {
    let f = Fixture::new().await;
    let app = f.app();
    let (status, first) = get(app.clone(), "/api/containers/aaaaaaaaaaaa/logs").await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["lines"].as_array().unwrap().len(), 100);
    assert_eq!(first["has_older"], true);
    assert!(!first.to_string().contains("ENV_SECRET"));
    assert!(first.to_string().contains("[REDACTED]"));
    let until = first["until"].as_str().unwrap();
    let (_, second) = get(
        app.clone(),
        &format!("/api/containers/aaaaaaaaaaaa/logs?page=1&until={until}"),
    )
    .await;
    let (_, third) = get(
        app.clone(),
        &format!("/api/containers/aaaaaaaaaaaa/logs?page=2&until={until}"),
    )
    .await;
    assert_eq!(second["lines"].as_array().unwrap().len(), 100);
    assert_eq!(third["lines"].as_array().unwrap().len(), 51);
    assert_eq!(third["has_older"], false);
    let mut all = third["lines"].as_array().unwrap().clone();
    all.extend(second["lines"].as_array().unwrap().clone());
    all.extend(first["lines"].as_array().unwrap().clone());
    let unique: std::collections::HashSet<_> = all.iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(unique.len(), 251);
    f.mock.lock().unwrap().containers.clear(); // Cached pages don't re-inspect or re-read logs.
    let (status, body) = html(
        app.clone(),
        &format!("/containers/aaaaaaaaaaaa/logs?page=1&until={until}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("[REDACTED]"));
    assert!(!body.contains("ENV_SECRET"));
    assert_eq!(
        get(app.clone(), &link(&body, "This page as JSON")).await.1["page"],
        1
    );
    assert!(link(&body, "Older").contains("page=2"));
    assert!(link(&body, "Newer").contains("page=0"));
    assert_eq!(f.mock.lock().unwrap().log_reads, 1);
    let (status, body) = html(app, "/containers/aaaaaaaaaaaa/logs?until=expired").await;
    assert_eq!(status, StatusCode::GONE);
    assert!(body.contains("Log snapshot expired"));
    assert!(body.contains(">Latest</a>"));
}
#[tokio::test]
async fn tty_and_split_utf8_logs_are_supported_and_html_is_escaped() {
    let f = Fixture::new().await;
    {
        let mut mock = f.mock.lock().unwrap();
        mock.containers[0]["Config"]["Tty"] = json!(true);
        mock.logs = b"2026-01-01T00:00:00Z <script>alert(1)</script> ENV_SECRET\n".to_vec();
    }
    let (status, body) = html(f.app(), "/containers/aaaaaaaaaaaa/logs").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("&lt;script&gt;"));
    assert!(!body.contains("<script>"));
    assert!(!body.contains("ENV_SECRET"));
    {
        let mut mock = f.mock.lock().unwrap();
        mock.containers[0]["Config"]["Tty"] = json!(false);
        mock.logs = frame(1, &[0xe2]);
        mock.logs.extend(frame(1, &[0x82, 0xac, b'\n']));
    }
    assert_eq!(
        get(f.app(), "/api/containers/aaaaaaaaaaaa/logs").await.1["lines"][0],
        "€"
    );
    f.mock.lock().unwrap().logs = vec![1, 0, 0, 0, 0, 0, 0, 9, b'x'];
    assert_eq!(
        get(f.app(), "/api/containers/aaaaaaaaaaaa/logs").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}
#[tokio::test]
async fn logs_can_be_disabled_even_for_cached_pages() {
    let f = Fixture::new().await;
    let mut state = f.state();
    let (_, data) = get(app(state.clone()), "/api/containers/aaaaaaaaaaaa/logs").await;
    state.logs_enabled = false;
    let app = app(state);
    let url = format!(
        "/api/containers/aaaaaaaaaaaa/logs?until={}",
        data["until"].as_str().unwrap()
    );
    assert_eq!(get(app.clone(), &url).await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        html(app.clone(), "/containers/aaaaaaaaaaaa/logs").await.0,
        StatusCode::FORBIDDEN
    );
    let (_, data) = get(app.clone(), "/api/overview").await;
    assert!(data["containers"][0]["state_error"].is_null());
    let (_, body) = html(app, "/").await;
    assert!(body.contains("Log access disabled."));
    assert!(!body.contains("href=\"/containers/aaaaaaaaaaaa/logs\""));
}
#[tokio::test]
async fn usage_calculations_are_bounded_and_allowlisted() {
    let f = Fixture::new().await;
    let (status, data) = get(f.app(), "/api/usage").await;
    assert_eq!(status, StatusCode::OK, "{data}");
    let c = &data["containers"][0];
    assert_eq!(c["cpu_percent_value"], 4.0);
    assert_eq!(c["memory_usage_bytes"], 33554432);
    assert_eq!(c["memory_percent"], "3.12%");
    assert_eq!(c["pids"], "3");
    assert_eq!(c["network_io"], "1.0 KiB / 2.0 KiB");
    assert_eq!(c["block_io"], "4.0 KiB / 8.0 KiB");
    assert!(!data.to_string().contains("STATS_SECRET"));
    assert!(
        html(f.app(), "/usage")
            .await
            .1
            .contains("32.0 MiB / 1.0 GiB")
    );
    f.mock.lock().unwrap().stats = json!({"cpu_stats":{"cpu_usage":{"total_usage":0},"system_cpu_usage":0,"online_cpus":0},"precpu_stats":{"cpu_usage":{"total_usage":1},"system_cpu_usage":1},"memory_stats":{"usage":1,"limit":0,"stats":{"inactive_file":10}}});
    let (_, data) = get(f.app(), "/api/usage").await;
    assert!(data["containers"][0]["cpu_percent_value"].is_null());
    assert!(data["containers"][0]["memory_percent"].is_null());
    f.mock.lock().unwrap().containers.clear();
    assert_eq!(get(f.app(), "/api/usage").await.1["containers"], json!([]));
}
#[tokio::test]
async fn api_errors_limits_and_invalid_queries_do_not_leak_daemon_bodies() {
    let f = Fixture::new().await;
    for uri in [
        "/api/containers/--help/logs",
        "/api/containers/aaaaaaaaaaaa/logs?page=100",
        "/api/containers/aaaaaaaaaaaa/logs?page=1",
    ] {
        assert_eq!(get(f.app(), uri).await.0, StatusCode::BAD_REQUEST);
    }
    f.mock.lock().unwrap().overrides.insert(
        "/info".into(),
        (StatusCode::INTERNAL_SERVER_ERROR, b"DAEMON_SECRET".to_vec()),
    );
    let (status, data) = get(f.app(), "/api/overview").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!data.to_string().contains("DAEMON_SECRET"));
    f.mock.lock().unwrap().overrides.remove("/info");
    f.mock.lock().unwrap().logs = vec![0; 4 * 1024 * 1024 + 1];
    assert_eq!(
        get(f.app(), "/api/containers/aaaaaaaaaaaa/logs").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let mut state = f.state();
    state.socket_path = f.dir.join("missing.sock").to_string_lossy().into_owned();
    assert_eq!(
        get(app(state), "/api/overview").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}
#[tokio::test]
async fn version_negotiation_and_html_headers_work() {
    let f = Fixture::new().await;
    f.mock.lock().unwrap().version["ApiVersion"] = json!("1.43");
    let app = f.app();
    for path in ["/", "/style.css"] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["referrer-policy"], "no-referrer");
        assert!(
            response.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("default-src 'none'")
        );
    }
    {
        let mock = f.mock.lock().unwrap();
        assert!(mock.requests.iter().any(|p| p.starts_with("/v1.43/")));
        assert_eq!(
            mock.requests
                .iter()
                .filter(|p| p.as_str() == "/version")
                .count(),
            1
        );
    }
    f.mock.lock().unwrap().version = json!({"ApiVersion":"1.70","MinAPIVersion":"1.60"});
    assert_eq!(
        get(f.app(), "/api/overview").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}
