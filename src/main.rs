use docker_dash::{AppState, Endpoint, app};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let address = std::env::var("DOCKER_DASH_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let mut state = AppState::default();
    if let Ok(host) = std::env::var("DOCKER_HOST").map(|host| host.trim().to_owned())
        && !host.is_empty()
    {
        if std::env::var("DOCKER_TLS_VERIFY").is_ok_and(|v| !v.is_empty()) {
            return Err("DOCKER_TLS_VERIFY is set, but TLS connections are not supported".into());
        }
        state.docker = Endpoint::parse(&host)?;
    }
    state.allowed_hosts = std::env::var("DOCKER_DASH_ALLOWED_HOSTS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .map(str::to_owned)
        .collect();
    let listener = tokio::net::TcpListener::bind(&address).await?;
    println!("docker-dash listening on http://{}", listener.local_addr()?);
    axum::serve(listener, app(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
