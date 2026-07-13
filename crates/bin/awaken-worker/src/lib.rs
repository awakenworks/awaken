//! `awaken-worker` — the PRODUCTION database-less worker (Stage C).
//!
//! A peer of the control plane (`awaken-control`) and the data plane
//! (`awaken-server`). It holds no store and serves no HTTP: it drains runs from a
//! cell server over the dispatch transport (claim / settle) and pushes committed
//! facts back over the commit ingest (`with_upstream`).
//!
//! **Real per-run model resolution, no mocks.** A drained run arrives as a
//! `RunActivation` carrying its own `ExecutableAgentSnapshot`, whose
//! `resolved_spec.model_binding.model_ref` is the run's model identity. The host's
//! run loop resolves that ref through the injected [`ExecutorProvider`]
//! ([`ConfigExecutorProvider`](awaken_server::config_executor::ConfigExecutorProvider)),
//! which maps `model_ref → offering(provider) → the workspace's Active credential →
//! resolve_inference → a real genai executor` over the SAME shared control-plane
//! stores the console authored (Option A, shared-DB — see
//! [`awaken_control::open_shared_config_stores_from_env`]). Only a run whose model is not yet
//! published falls back to the auxiliary
//! [`NoModelConfiguredExecutor`](awaken_server::no_model::NoModelConfiguredExecutor)
//! — the production placeholder, never a mock echo model.

use std::sync::Arc;

use awaken_server::SharedHost;
use awaken_server::config_executor::ConfigExecutorProvider;
use awaken_server::no_model::NoModelConfiguredExecutor;

/// Run this process as a database-less **worker** of the cell server at `upstream`.
///
/// 1. Route the dispatch pool's claim/settle over HTTP to `upstream`
///    (`worker_dispatch_store`), so the worker drains the server's queue instead of a
///    local one.
/// 2. Open the shared control-plane stores (catalog + credential vault + secret store)
///    the same way the Serve composition does — durable under `AWAKEN_MGMT_DIR`
///    (Option A shared-DB) or in-memory.
/// 3. Build a [`ConfigExecutorProvider`] over those stores, so each drained run's
///    `model_ref` resolves to the real DB-configured provider.
/// 4. Assemble a [`SharedHost`] whose default executor is the production
///    `NoModelConfiguredExecutor` fallback and whose per-run resolution is the
///    config-plane provider, pushing committed facts to `upstream`.
/// 5. Start the dispatch pool and drain in the background until SIGINT / SIGTERM.
///
/// Requires `AWAKEN_INGRESS=durable` (the pool's enable gate); the injected remote
/// store routes the drain over HTTP instead of a local queue.
pub async fn run(upstream: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Route the dispatch pool's claim/settle over HTTP to the cell server.
    awaken_runtime_host::init_shared_dispatch_store(awaken_runtime_host::worker_dispatch_store(
        upstream,
    ));

    // The shared control-plane stores this worker resolves models from — opened the
    // same way the Serve composition opens them (durable under AWAKEN_MGMT_DIR, else
    // in-memory). A worker shares the same per-component databases (Option A).
    let stores = awaken_control::open_shared_config_stores_from_env().await;

    // Resolve each run's model_ref to a real executor from the live catalog + the
    // workspace's credential. This is the seam the host's run loop consults per run;
    // an unpublished/unresolvable model falls back to the NoModelConfiguredExecutor
    // default below (a clear guidance message, never a mock).
    let config_exec_provider = ConfigExecutorProvider::new(
        stores.catalog,
        stores.credentials,
        stores.secrets,
        awaken_control::BOOTSTRAP_WORKSPACE,
    );

    let host = Arc::new(
        SharedHost::new(Arc::new(NoModelConfiguredExecutor), "worker")
            .with_upstream(upstream)
            .with_executor_provider(Arc::new(config_exec_provider)),
    );
    host.ensure_dispatch_pool();
    eprintln!(
        "awaken-worker draining from {upstream} (per-run model resolution from the config plane)"
    );

    // Drain in the background; block until asked to stop. SIGINT is a developer's
    // foreground stop; SIGTERM is what an orchestrator sends first before SIGKILL.
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    Ok(())
}
