//! `awaken` — the single aggregated command.
//!
//! One binary; configuration decides the deployment. The role axis (`AWAKEN_ROLE`,
//! with backward-compatible inference) selects what this process is:
//!
//!   - **Serve** (default) — the single-machine all-in-one, or a coordinator when
//!     `AWAKEN_DISABLE_LOCAL_POOL=1`. Mounts the full management + data plane and
//!     resolves each session's model from the **database-configured** catalog +
//!     credential vault (the console authors providers/models/credentials via
//!     `/v1/config/*` + `/v1/vaults/*`; a session then runs the real provider the
//!     management plane bound). Durable under `AWAKEN_MGMT_DIR` (secrets sealed with
//!     `AWAKEN_MGMT_SEAL_KEY`); the embedded IAM guard is enabled with
//!     `AWAKEN_MGMT_IAM=embedded`.
//!   - **Worker** (`AWAKEN_UPSTREAM_URL`) — a database-less worker of a cell server:
//!     claims runs and commits facts over HTTP, holds no store, serves no HTTP.
//!   - **Hand** (`AWAKEN_HAND_*`) — a remote ACP executor endpoint.
//!
//! The Serve role reuses the production management assembly from
//! `awaken-server` (the single-machine composition root); the Worker / Hand
//! roles reuse its role helpers. This subsumes the separate `awaken-server`
//! binary, which remains only as the e2e-scenario host.

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Install tracing + optional OTLP / trace-file export before anything is served.
    awaken_observability::init();

    // The single role axis. Hand and Worker are execution endpoints that never serve
    // HTTP; Serve is the default single-machine / coordinator command.
    match awaken_server::deployment_role() {
        awaken_server::Role::Hand => return awaken_server::run_hand_role().await,
        awaken_server::Role::Worker => {
            let upstream = std::env::var("AWAKEN_UPSTREAM_URL").unwrap_or_default();
            return awaken_server::run_worker(&upstream).await;
        }
        awaken_server::Role::Serve => {}
    }

    // Serve role. Refuse a durable ingress on a volatile queue (no-data-loss guard).
    awaken_runtime_host::ensure_durable_backend()?;
    // Shared Postgres data-plane backends (a multi-node fleet), connected once before
    // serving; the console's own config/catalog/credential stores are SQLite under
    // AWAKEN_MGMT_DIR (or in-memory), assembled inside the management router.
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

    // The production management assembly (this crate's own composition-root library):
    // the full protocol surface over a host whose ExecutorProvider resolves each
    // session's model from the DB-configured catalog + credential vault. Configure a
    // provider/model/credential through /v1/config/* + /v1/vaults/* and sessions run
    // that real model.
    let app = awaken_cli::build_management_router().await;
    // The brain admin surface (connection-count metric + /admin/drain + /readyz) so a
    // graceful, stream-preserving scale-in works; wraps the served router.
    let app = awaken_cli::with_brain_admin(app, awaken_cli::DrainController::new());
    // Root every request span in the ingress middleware (extracts the inbound
    // traceparent); the whole request→inference path nests under it.
    let app = app.layer(axum::middleware::from_fn(awaken_observability::trace_http));

    let addr = std::env::var("AWAKEN_HTTP_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let iam = std::env::var("AWAKEN_MGMT_IAM").as_deref() == Ok("embedded");
    let durable = std::env::var("AWAKEN_MGMT_DIR").is_ok();
    eprintln!(
        "awaken serving on http://{addr} (management plane; models resolved from the \
         database-configured catalog + vault; storage: {}; auth: {})",
        if durable {
            "durable (AWAKEN_MGMT_DIR)"
        } else {
            "in-memory (set AWAKEN_MGMT_DIR to persist)"
        },
        if iam {
            "embedded IAM"
        } else {
            "open (key-resolved tenancy)"
        },
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    awaken_observability::shutdown();
    Ok(())
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
