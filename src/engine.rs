//! Read-only Docker Engine HTTP API over a Unix socket or plain TCP.
use super::{ApiError, AppState, docker, unavailable};
use axum::http::StatusCode;
use http_body_util::{BodyExt, Empty};
use hyper::{Request, body::Bytes, client::conn::http1};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use std::{path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpStream, UnixStream},
};

const MAX_BODY: usize = 4 * 1024 * 1024;

/// Where the Docker Engine listens, as given by `DOCKER_HOST`.
#[derive(Clone, Debug, PartialEq)]
pub enum Endpoint {
    Unix(PathBuf),
    Tcp(String),
}
impl Default for Endpoint {
    fn default() -> Self {
        Self::Unix("/var/run/docker.sock".into())
    }
}
impl Endpoint {
    /// Parses `unix://` and `tcp://` addresses. TLS and SSH are not supported.
    pub fn parse(docker_host: &str) -> Result<Self, String> {
        if let Some(path) = docker_host.strip_prefix("unix://") {
            Ok(Self::Unix(path.into()))
        } else if let Some(address) = docker_host.strip_prefix("tcp://") {
            let address = address.trim_end_matches('/');
            if address.is_empty() || address.contains('/') {
                return Err(format!("Invalid DOCKER_HOST: {docker_host}"));
            }
            Ok(Self::Tcp(address.into()))
        } else {
            Err(format!(
                "Unsupported DOCKER_HOST {docker_host}: use unix:// or tcp://"
            ))
        }
    }
}

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

async fn connect(endpoint: &Endpoint) -> std::io::Result<Box<dyn Stream>> {
    Ok(match endpoint {
        Endpoint::Unix(path) => Box::new(UnixStream::connect(path).await?),
        Endpoint::Tcp(address) => Box::new(TcpStream::connect(address).await?),
    })
}

/// Sends a GET request and passes each body chunk to `sink` as it arrives.
async fn read(
    state: &AppState,
    path: &str,
    mut sink: impl FnMut(Bytes) -> Result<(), ApiError> + Send,
) -> Result<(), ApiError> {
    let _permit = tokio::time::timeout(Duration::from_secs(5), state.requests.acquire())
        .await
        .map_err(|_| unavailable("Dashboard is busy. Retry the request."))?
        .map_err(|_| unavailable("Dashboard is shutting down"))?;
    tokio::time::timeout(Duration::from_secs(15), async {
        let stream = connect(&state.docker)
            .await
            .map_err(|e| unavailable(format!("Cannot connect to Docker: {e}")))?;
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
        let mut body = response.into_body();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|_| unavailable("Docker response could not be read"))?;
            if let Ok(data) = frame.into_data() {
                sink(data)?;
            }
        }
        Ok(())
    })
    .await
    .map_err(|_| unavailable("Docker request timed out after 15 seconds"))?
}
async fn raw(state: &AppState, path: &str) -> Result<Vec<u8>, ApiError> {
    let mut body = Vec::new();
    read(state, path, |chunk| {
        if body.len() + chunk.len() > MAX_BODY {
            return Err(unavailable("Docker response exceeds 4 MiB"));
        }
        body.extend_from_slice(&chunk);
        Ok(())
    })
    .await?;
    Ok(body)
}
fn parse_version(value: &str) -> Option<(u16, u16)> {
    let (major, minor) = value.split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}
async fn negotiated(state: &AppState) -> Result<&(String, Option<String>), ApiError> {
    state
        .api
        .get_or_try_init(|| async {
            let bytes = raw(state, "/version").await?;
            let value: docker::Version = serde_json::from_slice(&bytes)
                .map_err(|_| unavailable("Invalid Docker version response"))?;
            let maximum = parse_version(&value.api_version)
                .ok_or_else(|| unavailable("Docker did not report a valid API version"))?;
            let minimum = value
                .min_api_version
                .as_deref()
                .and_then(parse_version)
                .unwrap_or((1, 24));
            let selected = maximum.min((1, 56));
            if selected < minimum.max((1, 40)) {
                return Err(unavailable(
                    "No compatible Docker API version (supported: 1.40–1.56)",
                ));
            }
            Ok((format!("/v{}.{}", selected.0, selected.1), value.version))
        })
        .await
}
pub(super) async fn version(state: &AppState) -> Result<Option<String>, ApiError> {
    Ok(negotiated(state).await?.1.clone())
}
pub(super) async fn json<T: DeserializeOwned>(state: &AppState, path: &str) -> Result<T, ApiError> {
    let prefix = &negotiated(state).await?.0;
    serde_json::from_slice(&raw(state, &format!("{prefix}{path}")).await?)
        .map_err(|_| unavailable("Invalid Docker JSON response"))
}
/// Streams a response body without a size limit; `sink` must bound memory.
pub(super) async fn stream(
    state: &AppState,
    path: &str,
    sink: impl FnMut(Bytes) -> Result<(), ApiError> + Send,
) -> Result<(), ApiError> {
    let prefix = &negotiated(state).await?.0;
    read(state, &format!("{prefix}{path}"), sink).await
}

#[cfg(test)]
mod tests {
    use super::Endpoint;
    #[test]
    fn docker_host_supports_unix_and_tcp() {
        assert_eq!(
            Endpoint::parse("unix:///run/user/1000/docker.sock"),
            Ok(Endpoint::Unix("/run/user/1000/docker.sock".into()))
        );
        assert_eq!(
            Endpoint::parse("tcp://127.0.0.1:2375"),
            Ok(Endpoint::Tcp("127.0.0.1:2375".into()))
        );
        assert!(Endpoint::parse("ssh://user@host").is_err());
        assert!(Endpoint::parse("tcp://").is_err());
    }
}
