//! Read-only Docker Engine HTTP API over a local Unix socket.
use super::{ApiError, AppState, unavailable};
use axum::http::StatusCode;
use http_body_util::{BodyExt, Empty, Limited};
use hyper::{Request, body::Bytes, client::conn::http1};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use std::time::Duration;
use tokio::net::UnixStream;

pub(super) const MAX_BODY: usize = 4 * 1024 * 1024;

async fn raw(state: &AppState, path: &str) -> Result<Bytes, ApiError> {
    let _permit = tokio::time::timeout(Duration::from_secs(5), state.requests.acquire())
        .await
        .map_err(|_| unavailable("Dashboard is busy. Retry the request."))?
        .map_err(|_| unavailable("Dashboard is shutting down"))?;
    tokio::time::timeout(Duration::from_secs(15), async {
        let stream = UnixStream::connect(&state.socket_path)
            .await
            .map_err(|e| unavailable(format!("Cannot connect to Docker socket: {e}")))?;
        let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|_| unavailable("Docker HTTP handshake failed"))?;
        // Connection: close bounds the lifetime of this task, even if a caller
        // cancels while waiting for the response. Abort it on cancellation too.
        let driver = tokio::spawn(async move {
            let _ = connection.await;
        });
        struct Driver(tokio::task::JoinHandle<()>);
        impl Drop for Driver {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _driver = Driver(driver);
        let request = Request::builder()
            .method("GET")
            .uri(path)
            .header("Host", "localhost")
            .header("Connection", "close")
            .body(Empty::<Bytes>::new())
            .map_err(|_| unavailable("Invalid Docker API request"))?;
        let response = sender
            .send_request(request)
            .await
            .map_err(|_| unavailable("Docker HTTP request failed"))?;
        let status = response.status();
        if !status.is_success() {
            // Daemon error bodies may contain command arguments or secrets.
            let exposed = match status {
                StatusCode::NOT_FOUND => StatusCode::NOT_FOUND,
                StatusCode::CONFLICT => StatusCode::CONFLICT,
                _ => StatusCode::SERVICE_UNAVAILABLE,
            };
            return Err(ApiError(
                exposed,
                format!("Docker Engine returned HTTP {status}"),
            ));
        }
        Limited::new(response.into_body(), MAX_BODY)
            .collect()
            .await
            .map(|body| body.to_bytes())
            .map_err(|_| unavailable("Docker response could not be read or exceeds 4 MiB"))
    })
    .await
    .map_err(|_| unavailable("Docker request timed out after 15 seconds"))?
}
fn parse_version(value: &str) -> Option<(u16, u16)> {
    let (major, minor) = value.split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}
async fn negotiated(state: &AppState) -> Result<&(String, Value), ApiError> {
    state
        .api
        .get_or_try_init(|| async {
            let bytes = raw(state, "/version").await?;
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|_| unavailable("Invalid Docker version response"))?;
            let maximum = value["ApiVersion"]
                .as_str()
                .and_then(parse_version)
                .ok_or_else(|| unavailable("Docker did not report a valid API version"))?;
            let minimum = value["MinAPIVersion"]
                .as_str()
                .and_then(parse_version)
                .unwrap_or((1, 24));
            let selected = maximum.min((1, 56));
            if selected < minimum.max((1, 40)) {
                return Err(unavailable(
                    "No compatible Docker API version (supported: 1.40–1.56)",
                ));
            }
            Ok((format!("/v{}.{}", selected.0, selected.1), value))
        })
        .await
}
pub(super) async fn version(state: &AppState) -> Result<Value, ApiError> {
    Ok(negotiated(state).await?.1.clone())
}
pub(super) async fn get(state: &AppState, path: &str) -> Result<Bytes, ApiError> {
    let prefix = &negotiated(state).await?.0;
    raw(state, &format!("{prefix}{path}")).await
}
pub(super) async fn json(state: &AppState, path: &str) -> Result<Value, ApiError> {
    serde_json::from_slice(&get(state, path).await?)
        .map_err(|_| unavailable("Invalid Docker JSON response"))
}
pub(super) fn log_streams(mut bytes: &[u8], tty: bool) -> Result<[Vec<u8>; 2], ApiError> {
    if tty {
        return Ok([bytes.to_vec(), Vec::new()]);
    }
    let mut streams = [Vec::new(), Vec::new()];
    while !bytes.is_empty() {
        if bytes.len() < 8 || !matches!(bytes[0], 1 | 2) || bytes[1..4] != [0, 0, 0] {
            return Err(unavailable("Malformed Docker log frame"));
        }
        let length = u32::from_be_bytes(bytes[4..8].try_into().unwrap()) as usize;
        if length > bytes.len() - 8 {
            return Err(unavailable("Truncated Docker log frame"));
        }
        streams[bytes[0] as usize - 1].extend_from_slice(&bytes[8..8 + length]);
        bytes = &bytes[8 + length..];
    }
    Ok(streams)
}
