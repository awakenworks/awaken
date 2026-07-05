//! `awaken-server-local` binary: serve the Managed Agents runtime surface on one
//! machine. Binds `AWAKEN_HTTP_ADDR` (default `127.0.0.1:38080`). The model is
//! deterministic so it runs without an API key: `AWAKEN_MODEL_MODE=echo` (default)
//! replies with the user's text; `=probe` writes/reads a file so the HITL
//! (`user.tool_confirmation`) path can be exercised; `=vision` reports the media it
//! received so the multimodal path can be verified. A provider executor is
//! swapped in through [`awaken_server_local::build_router`].

use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::var("AWAKEN_HTTP_ADDR").unwrap_or_else(|_| "127.0.0.1:38080".to_string());
    let app = match std::env::var("AWAKEN_MODEL_MODE").as_deref() {
        Ok("probe") => {
            awaken_server_local::build_router(Arc::new(awaken_server_local::ProbeModel), "probe")
        }
        Ok("revise") => {
            awaken_server_local::build_router(Arc::new(awaken_server_local::ReviseModel), "revise")
        }
        Ok("custom") => awaken_server_local::build_custom_router(),
        Ok("delegate") => awaken_server_local::build_delegation_router(),
        Ok("statemachine") => awaken_server_local::build_statemachine_router(),
        Ok("statemachine-rich") => awaken_server_local::build_statemachine_rich_router(),
        Ok("config") => awaken_server_local::build_config_router(),
        Ok("management") => awaken_server_local::build_management_router(),
        Ok("real") => awaken_server_local::build_real_router(),
        Ok("real-gemini") => awaken_server_local::build_real_gemini_router().await,
        Ok("real-resolved") => awaken_server_local::build_resolved_real_router().await,
        Ok("schedule") => awaken_server_local::build_schedule_router(),
        Ok("skills") => awaken_server_local::build_skills_router(),
        Ok("delegate-remote") => awaken_server_local::build_remote_delegation_router(),
        Ok("vision") => awaken_server_local::build_vision_router(),
        Ok("model-route") => awaken_server_local::build_model_route_router(),
        Ok("acp") => awaken_server_local::build_acp_router(),
        Ok("memory") => awaken_server_local::build_memory_router(),
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
