//! `awaken-server-local` binary: serve the Managed Agents runtime surface on one
//! machine. Binds `AWAKEN_HTTP_ADDR` (default `127.0.0.1:38080`) and uses the
//! deterministic echo model so it runs without an API key; a provider executor is
//! swapped in through [`awaken_server_local::build_router`].

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::var("AWAKEN_HTTP_ADDR").unwrap_or_else(|_| "127.0.0.1:38080".to_string());
    let app = awaken_server_local::build_echo_router();
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("awaken-server-local listening on http://{addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
