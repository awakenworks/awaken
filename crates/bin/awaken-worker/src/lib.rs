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

mod admin;

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

    // The base host: honors a per-run cloud-managed gateway grant (ADR-0004) by
    // dialing the gateway with the run's lease token — the genai implementation of
    // the gateway egress port. This is the durable-path secretless seam: a gateway
    // run needs no local provider credential.
    let mut host = SharedHost::new(Arc::new(NoModelConfiguredExecutor), "worker")
        .with_upstream(upstream)
        .with_gateway_executor_factory(Arc::new(awaken_server::GenaiGatewayExecutorFactory));

    // Gateway-only mode (`AWAKEN_WORKER_GATEWAY_ONLY=1`): a genuinely SECRETLESS
    // worker. It opens no credential vault and needs no seal key — every run must
    // carry a cloud-managed gateway grant; a local-credentialed run has no executor
    // and falls back to the NoModelConfiguredExecutor guidance. Otherwise (the
    // default) the worker also resolves local-grant runs from the shared control
    // plane, which requires the vault + seal key.
    let gateway_only = std::env::var("AWAKEN_WORKER_GATEWAY_ONLY").as_deref() == Ok("1");
    if !gateway_only {
        // The shared control-plane stores this worker resolves local-grant models from
        // — opened the same way the Serve composition opens them (durable under
        // AWAKEN_MGMT_DIR, else in-memory; needs the seal key to unseal credentials).
        let stores = awaken_control::open_shared_config_stores_from_env().await;
        let config_exec_provider = ConfigExecutorProvider::new(
            stores.catalog,
            stores.credentials,
            stores.secrets,
            awaken_control::BOOTSTRAP_WORKSPACE,
        );
        host = host.with_executor_provider(Arc::new(config_exec_provider));
    }

    let host = Arc::new(host);
    host.ensure_dispatch_pool();
    let posture = if gateway_only {
        "secretless (gateway-only; no vault/seal key)"
    } else {
        "per-run model resolution from the config plane"
    };
    eprintln!(
        "awaken-worker draining from {upstream} ({posture})"
    );

    // The cloud-native admin surface on a SEPARATE port from any data path: an
    // orchestrator gates routing on `/readyz` and calls `POST /admin/drain` in a
    // `preStop` hook before SIGTERM. Best-effort — a bind failure is logged but does
    // not stop the worker draining runs (the core job).
    let admin_addr = std::env::var("AWAKEN_WORKER_ADMIN_LISTEN")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "0.0.0.0:9090".to_string());
    match tokio::net::TcpListener::bind(&admin_addr).await {
        Ok(listener) => {
            let router = admin::worker_admin_router(host.clone());
            eprintln!("awaken-worker admin surface on {admin_addr} (/readyz /metrics /admin/drain)");
            tokio::spawn(async move {
                if let Err(err) = axum::serve(listener, router).await {
                    eprintln!("awaken-worker admin server exited: {err}");
                }
            });
        }
        Err(err) => eprintln!("awaken-worker admin surface disabled (bind {admin_addr}: {err})"),
    }

    // Block until asked to stop. SIGINT is a developer's foreground stop (exit
    // promptly after draining); SIGTERM is what an orchestrator sends first before
    // SIGKILL (drain, then let in-flight runs finish within the grace window).
    #[cfg(unix)]
    let graceful = {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => false,
            _ = term.recv() => true,
        }
    };
    #[cfg(not(unix))]
    let graceful = {
        let _ = tokio::signal::ctrl_c().await;
        false
    };

    // Stop claiming immediately so no NEW run is taken; the in-flight ones finish
    // within the grace window before the process exits.
    host.begin_pool_drain().await;
    let grace = drain_grace(graceful);
    if !grace.is_zero() {
        eprintln!("awaken-worker draining: finishing in-flight runs (≤{}s)", grace.as_secs());
        tokio::time::sleep(grace).await;
    }
    Ok(())
}

/// The graceful-drain window: how long to let in-flight runs finish after we stop
/// claiming. Reads `AWAKEN_WORKER_DRAIN_GRACE_SECS` and delegates to the pure
/// [`grace_window`] so the policy is unit-testable without touching the environment.
fn drain_grace(graceful: bool) -> std::time::Duration {
    let configured = std::env::var("AWAKEN_WORKER_DRAIN_GRACE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok());
    grace_window(graceful, configured)
}

/// The grace policy (pure): a SIGTERM (orchestrator scale-in) waits `configured` or
/// the 20s default so in-flight runs finish; a SIGINT (developer ctrl-c) exits
/// promptly (zero) so the foreground stop is snappy.
fn grace_window(graceful: bool, configured_secs: Option<u64>) -> std::time::Duration {
    if !graceful {
        return std::time::Duration::ZERO;
    }
    std::time::Duration::from_secs(configured_secs.unwrap_or(20))
}

#[cfg(test)]
mod grace_tests {
    use super::grace_window;

    #[test]
    fn sigint_exits_promptly_sigterm_waits() {
        assert!(
            grace_window(false, None).is_zero(),
            "ctrl-c drains then exits at once"
        );
        assert!(
            grace_window(false, Some(99)).is_zero(),
            "a configured grace never delays a foreground ctrl-c"
        );
        assert_eq!(
            grace_window(true, None).as_secs(),
            20,
            "SIGTERM default grace is 20s"
        );
        assert_eq!(
            grace_window(true, Some(5)).as_secs(),
            5,
            "the grace window is configurable"
        );
    }
}
