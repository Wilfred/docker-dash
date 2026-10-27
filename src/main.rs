use docker_dash::{AppState, app};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let address = std::env::var("DOCKER_DASH_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let mut state = AppState::default();
    if let Ok(value) = std::env::var("DOCKER_DASH_LOGS") {
        state.logs_enabled = value
            .parse::<bool>()
            .map_err(|_| "DOCKER_DASH_LOGS must be true or false")?;
    }
    let listener = tokio::net::TcpListener::bind(&address).await?;
    println!("docker-dash listening on http://{}", listener.local_addr()?);
    axum::serve(listener, app(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
