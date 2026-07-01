//! `awaken-server-local` binary: serve the Managed Agents runtime surface on one
//! machine. Binds `AWAKEN_HTTP_ADDR` (default `127.0.0.1:38080`). The model is
//! deterministic so it runs without an API key: `AWAKEN_MODEL_MODE=echo` (default)
//! replies with the user's text; `=probe` writes/reads a file so the HITL
//! (`user.tool_confirmation`) path can be exercised. A provider executor is
//! swapped in through [`awaken_server_local::build_router`].

use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::var("AWAKEN_HTTP_ADDR").unwrap_or_else(|_| "127.0.0.1:38080".to_string());
    let app = match std::env::var("AWAKEN_MODEL_MODE").as_deref() {
        Ok("probe") => {
            awaken_server_local::build_router(Arc::new(awaken_server_local::ProbeModel), "probe")
        }
        _ => awaken_server_local::build_echo_router(),
    };
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("awaken-server-local listening on http://{addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
