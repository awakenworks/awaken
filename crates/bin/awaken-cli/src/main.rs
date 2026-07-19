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
//!
//! (The remote ACP/hand executor is now the separate `awaken-sandbox`
//! execution-plane binary, not a role of this control-plane command.)
//!
//! The Serve role reuses the production management assembly from
//! `awaken-server` (the single-machine composition root); the Worker role
//! reuses its role helper. This subsumes the separate `awaken-server`
//! binary, which remains only as the e2e-scenario host.

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Install tracing + optional OTLP / trace-file export before anything is served.
    awaken_observability::init();

    // Validate the unified deployment configuration up front, for every role: a
    // role-aware, fail-closed check that refuses a contradictory deployment at boot
    // (rather than a confusing runtime behaviour later), and surfaces any legacy
    // env-name usage as a deprecation warning.
    let deployment = awaken_cli::config::AwakenConfig::from_env();
    for warning in &deployment.deprecations {
        eprintln!("awaken config: {warning}");
    }
    if let Err(errors) = deployment.validate() {
        for error in &errors {
            eprintln!("awaken config error: {error}");
        }
        return Err(format!("refusing to start: {} configuration error(s)", errors.len()).into());
    }
    eprintln!("awaken config: {}", deployment.summary());

    // The single role axis. Worker is an execution endpoint that never serves HTTP;
    // Serve is the default single-machine / coordinator command. (The hand is now the
    // separate `awaken-sandbox hand` execution-plane binary, not a server role.)
    match awaken_server::deployment_role() {
        awaken_server::Role::Worker => {
            let upstream = std::env::var("AWAKEN_UPSTREAM_URL").unwrap_or_default();
            // The PRODUCTION worker (Stage C): drains runs and resolves EACH run's
            // model from the DB-configured catalog + vault via ConfigExecutorProvider.
            return awaken_worker::run(&upstream).await;
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
        awaken_server::init_postgres_worker_registry(&url).await?;
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
    // graceful, stream-preserving scale-in works. When AWAKEN_SERVER_ADMIN_LISTEN is
    // set, serve the admin routes on that SEPARATE port (cloud-native: probes/metrics/
    // drain reach a management port, never the business Ingress); the business router
    // keeps only the connection metric. Otherwise the admin routes merge onto the one
    // business port (backward-compatible single-port default).
    let ctrl = awaken_cli::DrainController::new();
    // Register the connection-load gauge on the global OTel meter (installed by
    // `init()` above), so `/metrics` exposes `awaken_brain_active_streams` alongside
    // the business metrics. Held for the process lifetime so the callback stays live.
    let _active_streams_gauge = awaken_cli::register_active_streams_gauge(ctrl.clone());
    let admin_addr = std::env::var("AWAKEN_SERVER_ADMIN_LISTEN")
        .ok()
        .filter(|v| !v.is_empty());
    let app = match &admin_addr {
        Some(_) => awaken_cli::with_connection_metric(app, ctrl.clone()),
        None => awaken_cli::with_brain_admin(app, ctrl.clone()),
    };
    // Root every request span in the ingress middleware (extracts the inbound
    // traceparent); the whole request→inference path nests under it.
    let app = app.layer(axum::middleware::from_fn(awaken_observability::trace_http));

    // Serve the split admin surface on its own port, if configured.
    if let Some(admin_addr) = admin_addr {
        match tokio::net::TcpListener::bind(&admin_addr).await {
            Ok(listener) => {
                let admin = awaken_cli::brain_admin_router(ctrl);
                eprintln!(
                    "awaken admin surface on http://{admin_addr} (/readyz /metrics /admin/drain)"
                );
                tokio::spawn(async move {
                    if let Err(err) = axum::serve(listener, admin).await {
                        eprintln!("awaken admin server exited: {err}");
                    }
                });
            }
            Err(err) => {
                eprintln!("awaken admin surface disabled (bind {admin_addr}: {err})");
            }
        }
    }

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
