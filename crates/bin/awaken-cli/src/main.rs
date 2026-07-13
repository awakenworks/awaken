//! `awaken` — the single aggregated command.
//!
//! One binary; configuration decides the deployment. The role axis (`AWAKEN_ROLE`,
//! with backward-compatible inference) selects what this process is:
//!
//!   - **Serve** (default) — the single-machine all-in-one, or a coordinator when
//!     `AWAKEN_DISABLE_LOCAL_POOL=1`. Owns the store (SQLite under
//!     `AWAKEN_STORAGE_DIR`, or shared Postgres) and serves the full protocol
//!     surface with the co-located dispatch pool.
//!   - **Worker** (`AWAKEN_UPSTREAM_URL`) — a database-less worker of a cell server:
//!     claims runs and commits facts over HTTP, holds no store, serves no HTTP.
//!   - **Hand** (`AWAKEN_HAND_*`) — a remote ACP executor endpoint.
//!
//! The Serve role reuses the clean full-surface assembly (`awaken_standalone::build`)
//! and the deployment axes flow through the typed `DeploymentConfig` the runtime
//! reads. This subsumes the separate `awaken-server-local` (production path) and
//! `awaken-standalone` binaries — those remain only as the e2e-scenario host and a
//! thin compatibility shim.

use std::sync::Arc;

use awaken_runtime_contract::llm::LlmExecutor;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Install tracing + optional OTLP / trace-file export before anything is served.
    awaken_observability::init();

    // The single role axis. Hand and Worker are execution endpoints that never serve
    // HTTP; Serve is the default single-machine / coordinator command.
    match awaken_server_local::deployment_role() {
        awaken_server_local::Role::Hand => return awaken_server_local::run_hand_role().await,
        awaken_server_local::Role::Worker => {
            let upstream = std::env::var("AWAKEN_UPSTREAM_URL").unwrap_or_default();
            return awaken_server_local::run_worker(&upstream).await;
        }
        awaken_server_local::Role::Serve => {}
    }

    // Serve role. Refuse a durable ingress on a volatile queue (no-data-loss guard).
    awaken_runtime_host::ensure_durable_backend()?;
    // Shared Postgres backends (a multi-node fleet), connected once before serving.
    if std::env::var("AWAKEN_DISPATCH_BACKEND").as_deref() == Ok("postgres") {
        let url = std::env::var("AWAKEN_DATABASE_URL")
            .map_err(|_| "AWAKEN_DISPATCH_BACKEND=postgres requires AWAKEN_DATABASE_URL")?;
        awaken_runtime_host::init_shared_postgres_dispatch(&url).await?;
    }
    if std::env::var("AWAKEN_STORE").as_deref() == Ok("postgres") {
        let url = std::env::var("AWAKEN_DATABASE_URL")
            .map_err(|_| "AWAKEN_STORE=postgres requires AWAKEN_DATABASE_URL")?;
        awaken_runtime_host::init_shared_postgres_commit(&url).await?;
    }

    let standalone = awaken_standalone::build(select_model());
    let addr = std::env::var("AWAKEN_HTTP_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let local = listener.local_addr()?;
    eprintln!("{}", awaken_standalone::banner(&standalone, &local.to_string()));
    let router = standalone
        .router
        .layer(axum::middleware::from_fn(awaken_observability::trace_http));
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    awaken_observability::shutdown();
    Ok(())
}

/// The Serve-role model. A real provider is wired via config-plane/vault (a
/// follow-up exposes `awaken_server_local::executor_from_resolved` over env); the
/// zero-config demo greeter is the default so `awaken` boots with no credentials.
fn select_model() -> Arc<dyn LlmExecutor> {
    Arc::new(awaken_standalone::HelloModel)
}

/// Resolve on SIGINT (developer stop) or SIGTERM (orchestrator stop) so the server
/// drains in-flight requests before returning.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
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
