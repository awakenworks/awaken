//! `awaken-server` binary: serve the Managed Agents runtime surface on one
//! machine. Binds `AWAKEN_HTTP_ADDR` (default `127.0.0.1:38080`). The model is
//! deterministic so it runs without an API key: `AWAKEN_MODEL_MODE=echo` (default)
//! replies with the user's text; `=probe` writes/reads a file so the HITL
//! (`user.tool_confirmation`) path can be exercised; `=vision` reports the media it
//! received so the multimodal path can be verified. A provider executor is
//! swapped in through [`awaken_server::build_router`].

use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Install the tracing subscriber + optional OTLP / AWAKEN_TRACE_FILE span export
    // and the W3C traceparent propagator before any request is served, so every
    // `#[instrument]` span in the request path is captured on one trace.
    awaken_observability::init();
    let addr = std::env::var("AWAKEN_HTTP_ADDR").unwrap_or_else(|_| "127.0.0.1:38080".to_string());
    // Connect the shared Postgres dispatch pool once, before serving, when durable
    // ingress is backed by Postgres (a multi-node fleet sharing one queue). Doing it
    // here keeps the non-`Send` sqlx connect future out of the per-thread run loop.
    if std::env::var("AWAKEN_INGRESS").as_deref() == Ok("durable")
        && std::env::var("AWAKEN_DISPATCH_BACKEND").as_deref() == Ok("postgres")
    {
        let url = std::env::var("AWAKEN_DATABASE_URL")
            .map_err(|_| "AWAKEN_DISPATCH_BACKEND=postgres requires AWAKEN_DATABASE_URL")?;
        awaken_session_executor::init_shared_postgres_dispatch(&url).await?;
    }
    let app = match std::env::var("AWAKEN_MODEL_MODE").as_deref() {
        Ok("probe") => {
            awaken_server::build_router(Arc::new(awaken_server::ProbeModel), "probe")
        }
        Ok("revise") => {
            awaken_server::build_router(Arc::new(awaken_server::ReviseModel), "revise")
        }
        Ok("custom") => awaken_server::build_custom_router(),
        Ok("remote-hand") => awaken_server::build_remote_hand_router(),
        Ok("delegate") => awaken_server::build_delegation_router(),
        Ok("statemachine") => awaken_server::build_statemachine_router(),
        Ok("statemachine-rich") => awaken_server::build_statemachine_rich_router(),
        Ok("config") => awaken_server::build_config_router(),
        Ok("management") => awaken_server::build_management_router(),
        Ok("real") => awaken_server::build_real_router(),
        Ok("real-gemini") => awaken_server::build_real_gemini_router().await,
        Ok("real-resolved") => awaken_server::build_resolved_real_router().await,
        Ok("schedule") => awaken_server::build_schedule_router(),
        Ok("skills") => awaken_server::build_skills_router(),
        Ok("skills-durable") => awaken_server::build_skills_durable_router(),
        Ok("delegate-remote") => awaken_server::build_remote_delegation_router(),
        Ok("vision") => awaken_server::build_vision_router(),
        Ok("model-route") => awaken_server::build_model_route_router(),
        Ok("acp") => awaken_server::build_acp_router(),
        Ok("acp-jsonrpc") => awaken_server::build_acp_jsonrpc_router(),
        Ok("acp-sandboxed") => awaken_server::build_acp_sandboxed_router(),
        Ok("memory") => awaken_server::build_memory_router(),
        Ok("memory-resource") => awaken_server::build_memory_resource_router(),
        Ok("git-repo") => awaken_server::build_git_repo_router(),
        Ok("compaction") => awaken_server::build_compaction_router(),
        Ok("full-chain") => awaken_server::build_full_chain_router(),
        _ => awaken_server::build_echo_router(),
    };
    // Root every request span in the ingress middleware (extracts the inbound
    // `traceparent`); the whole direct request→inference path nests under it.
    let app = app.layer(axum::middleware::from_fn(awaken_observability::trace_http));
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("awaken-server listening on http://{addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    // Flush any buffered spans (OTLP batch / trace-file) before exit.
    awaken_observability::shutdown();
    Ok(())
}
