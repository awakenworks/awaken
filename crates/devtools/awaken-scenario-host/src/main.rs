//! `awaken-server` binary: serve the Managed Agents runtime surface on one
//! machine. Binds `AWAKEN_HTTP_ADDR` (default `127.0.0.1:38080`). The model is
//! deterministic so it runs without an API key: `AWAKEN_MODEL_MODE=echo` (default)
//! replies with the user's text; `=probe` writes/reads a file so the HITL
//! (`user.tool_confirmation`) path can be exercised; `=vision` reports the media it
//! received so the multimodal path can be verified. A provider executor is
//! swapped in through [`awaken_scenario_host::build_router`].

use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Install the tracing subscriber + optional OTLP / AWAKEN_TRACE_FILE span export
    // and the W3C traceparent propagator before any request is served, so every
    // `#[instrument]` span in the request path is captured on one trace.
    awaken_observability::init();
    // The single role axis (`AWAKEN_ROLE`, with backward-compatible inference from
    // the historic `AWAKEN_UPSTREAM_URL`). Worker is an execution endpoint that never
    // starts the HTTP surface; Serve is the default — single-machine all-in-one, or a
    // coordinator when the local pool is disabled. (The hand is now the separate
    // `awaken-sandbox hand` execution-plane binary, not a server role.)
    match awaken_server::deployment_role() {
        awaken_server::Role::Worker => {
            let upstream = std::env::var("AWAKEN_UPSTREAM_URL").unwrap_or_default();
            // Test-only echo-draining worker (the worker-pool e2e). The production
            // worker with real per-run model resolution lives in `awaken-worker`.
            return awaken_scenario_host::run_echo_worker(&upstream).await;
        }
        awaken_server::Role::Serve => {}
    }
    let addr = std::env::var("AWAKEN_HTTP_ADDR").unwrap_or_else(|_| "127.0.0.1:38080".to_string());
    // Durability guard: refuse to boot a `AWAKEN_INGRESS=durable` ingress that would
    // resolve to a volatile in-memory queue (durable + default sqlite backend + no
    // AWAKEN_STORAGE_DIR) — such a queue silently drops every queued/crashed/scheduled
    // run on restart, defeating the whole point of durable ingress (no-data-loss).
    awaken_runtime_host::ensure_durable_backend()?;
    // Connect the shared Postgres dispatch pool once, before serving, when durable
    // ingress is backed by Postgres (a multi-node fleet sharing one queue). Doing it
    // here keeps the non-`Send` sqlx connect future out of the per-thread run loop.
    if std::env::var("AWAKEN_INGRESS").as_deref() == Ok("durable")
        && std::env::var("AWAKEN_DISPATCH_BACKEND").as_deref() == Ok("postgres")
    {
        let url = std::env::var("AWAKEN_DATABASE_URL")
            .map_err(|_| "AWAKEN_DISPATCH_BACKEND=postgres requires AWAKEN_DATABASE_URL")?;
        awaken_runtime_host::init_shared_postgres_dispatch(&url).await?;
    }
    // Shared Postgres commit backend (ADR-0022 D6): thread history on one DB so any
    // node warm-reloads any thread. Connected once here (non-Send sqlx out of the run
    // loop), independent of the dispatch backend.
    if std::env::var("AWAKEN_STORE").as_deref() == Ok("postgres") {
        let url = std::env::var("AWAKEN_DATABASE_URL")
            .map_err(|_| "AWAKEN_STORE=postgres requires AWAKEN_DATABASE_URL")?;
        awaken_runtime_host::init_shared_postgres_commit(&url).await?;
    }
    let app = match std::env::var("AWAKEN_MODEL_MODE").as_deref() {
        Ok("probe") => {
            awaken_scenario_host::build_router(Arc::new(awaken_scenario_host::ProbeModel), "probe")
        }
        Ok("revise") => awaken_scenario_host::build_router(
            Arc::new(awaken_scenario_host::ReviseModel),
            "revise",
        ),
        Ok("custom") => awaken_scenario_host::build_custom_router(),
        Ok("remote-hand") => awaken_scenario_host::build_remote_hand_router(),
        Ok("delegate") => awaken_scenario_host::build_delegation_router(),
        Ok("statemachine") => awaken_scenario_host::build_statemachine_router(),
        Ok("statemachine-rich") => awaken_scenario_host::build_statemachine_rich_router(),
        Ok("config") => awaken_scenario_host::build_config_router().await,
        // The management scenario drives the REAL management router but with the
        // deterministic MCP scenario model as the pre-published fallback (production
        // uses the provider-free NoModelConfiguredExecutor); env-driven store
        // selection + IAM are identical to production.
        Ok("management") => {
            awaken_cli::build_management_router_with_fallback(
                std::sync::Arc::new(awaken_scenario_host::McpToolModel),
                "management".to_string(),
            )
            .await
        }
        Ok("real") => awaken_scenario_host::build_real_router(),
        Ok("real-gemini") => awaken_scenario_host::build_real_gemini_router().await,
        Ok("real-resolved") => awaken_scenario_host::build_resolved_real_router().await,
        Ok("oauth-resolved") => awaken_scenario_host::build_oauth_resolved_router().await,
        Ok("schedule") => awaken_scenario_host::build_schedule_router(),
        Ok("skills") => awaken_scenario_host::build_skills_router(),
        Ok("skills-durable") => awaken_scenario_host::build_skills_durable_router(),
        Ok("pool-failover") => awaken_scenario_host::build_pool_failover_router(),
        Ok("delegate-remote") => awaken_scenario_host::build_remote_delegation_router(),
        Ok("vision") => awaken_scenario_host::build_vision_router(),
        Ok("model-route") => awaken_scenario_host::build_model_route_router(),
        Ok("acp") => awaken_scenario_host::build_acp_router(),
        Ok("acp-jsonrpc") => awaken_scenario_host::build_acp_jsonrpc_router(),
        Ok("acp-sandboxed") => awaken_scenario_host::build_acp_sandboxed_router(),
        Ok("acp-container") => awaken_scenario_host::build_acp_container_router().await,
        Ok("acp-gateway") => awaken_scenario_host::build_acp_gateway_router(),
        Ok("acp-managed-mcp") => awaken_scenario_host::build_acp_managed_mcp_router().await,
        Ok("acp-real-mcp") => awaken_scenario_host::build_acp_real_mcp_router().await,
        Ok("memory") => awaken_scenario_host::build_memory_router(),
        Ok("memory-resource") => awaken_scenario_host::build_memory_resource_router(),
        Ok("git-repo") => awaken_scenario_host::build_git_repo_router(),
        Ok("compaction") => awaken_scenario_host::build_compaction_router(),
        Ok("error") => awaken_scenario_host::build_error_router(),
        Ok("worker") => awaken_scenario_host::build_worker_router(),
        Ok("full-chain") => awaken_scenario_host::build_full_chain_router(),
        _ => awaken_scenario_host::build_echo_router(),
    };
    // Brain admin surface (ADR-0022 D7): the connection-count metric that KEDA
    // autoscales on, plus /admin/drain + /readyz for graceful, stream-preserving
    // scale-in. Wraps the served router so the in-flight counter sees every request.
    let app = awaken_cli::with_brain_admin(app, awaken_cli::DrainController::new());
    // Root every request span in the ingress middleware (extracts the inbound
    // `traceparent`); the whole direct request→inference path nests under it.
    let app = app.layer(axum::middleware::from_fn(awaken_observability::trace_http));
    // Enforce the managed-agents beta opt-in on session creation, exactly as the
    // real API (the bare `router()` unit tests build carries no such layer).
    let app = app.layer(axum::middleware::from_fn(
        awaken_protocol_managed::enforce_managed_beta,
    ));
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("awaken-server listening on http://{addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    // Flush any buffered spans (OTLP batch / trace-file) before exit.
    awaken_observability::shutdown();
    Ok(())
}

/// Resolve when the process is asked to stop, so `axum::serve` stops accepting new
/// connections and drains in-flight requests before returning. We wait on BOTH
/// SIGINT (Ctrl-C, a developer's foreground stop) AND SIGTERM (the signal an
/// orchestrator — Kubernetes, systemd, `docker stop` — sends first, before the
/// hard SIGKILL). Handling SIGTERM is what makes the drain fire under a real
/// deployment: a durable foreground run is held open by its still-pending HTTP
/// handler (which awaits the dispatch pool's settle event), so graceful shutdown
/// waits for that request to finish — the run settles and commits instead of being
/// abandoned mid-drive (stranded claimed-but-not-settled until its lease expires).
/// Without the SIGTERM arm the process took the default disposition — an immediate
/// kill (exit 143) that dropped in-flight foreground work and skipped the span
/// flush below. Background/queued pool work not tied to a live request is not
/// covered here; it relies on lease-expiry recovery on the next start.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        // If SIGTERM can't be registered, fall back to Ctrl-C only rather than
        // refuse to shut down.
        let mut term = match signal(SignalKind::terminate()) {
            Ok(term) => term,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
