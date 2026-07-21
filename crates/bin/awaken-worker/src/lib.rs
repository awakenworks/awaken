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
//! run loop resolves that ref through the injected [`InferenceExecutorMaterializer`]
//! ([`CredentialInferenceMaterializer`](awaken_server::inference_materializer::CredentialInferenceMaterializer)),
//! which consumes the snapshot-pinned endpoint and credential reference and
//! injects the credential from the shared vault — see
//! [`awaken_control::open_inference_materialization_stores_from_env`]). Only a run whose model is not yet
//! published falls back to the auxiliary
//! [`NoModelConfiguredExecutor`](awaken_server::no_model::NoModelConfiguredExecutor)
//! — the production placeholder, never a mock echo model.

use std::sync::Arc;

mod admin;

use awaken_runtime_host::WorkerControlClient;
use awaken_runtime_host::WorkerUpstream;
use awaken_server::inference_materializer::CredentialInferenceMaterializer;
use awaken_server::no_model::NoModelConfiguredExecutor;
use awaken_server::{InferenceExecutorMaterializer, SharedHost};
use awaken_worker_contract::{
    RegistryMutation, VersionRange, WorkerCapacity, WorkerHeartbeat, WorkerIdentity, WorkerManifest,
};

#[derive(Clone)]
struct WorkerLifecycle {
    host: Arc<SharedHost>,
    control: WorkerControlClient,
    identity: WorkerIdentity,
}

impl WorkerLifecycle {
    async fn begin_drain(&self, deadline_ms: Option<u64>) -> Result<(), String> {
        let remote = self.control.begin_drain(&self.identity, deadline_ms).await;
        self.host.begin_pool_drain().await;
        match remote? {
            RegistryMutation::Applied => Ok(()),
            other => Err(format!("worker drain rejected: {other:?}")),
        }
    }
}

/// Run this process as a database-less **worker** of the cell server at `upstream`.
///
/// 1. Route the dispatch pool's claim/settle over HTTP to `upstream`
///    (`worker_dispatch_store`), so the worker drains the server's queue instead of a
///    local one.
/// 2. Open the shared credential vault + secret store
///    the same way the Serve composition does — durable under `AWAKEN_MGMT_DIR`
///    (Option A shared-DB) or in-memory.
/// 3. Build a [`CredentialInferenceMaterializer`] over those stores, so each drained
///    run consumes only its snapshot-pinned inference access.
/// 4. Assemble a [`SharedHost`] whose default executor is the production
///    `NoModelConfiguredExecutor` fallback and whose per-run access is realized
///    by the materializer, pushing committed facts to `upstream`.
/// 5. Start the dispatch pool and drain in the background until SIGINT / SIGTERM.
///
/// Requires `AWAKEN_INGRESS=durable` (the pool's enable gate); the injected remote
/// store routes the drain over HTTP instead of a local queue.
pub async fn run(upstream: &str) -> Result<(), Box<dyn std::error::Error>> {
    let stores = awaken_control::open_inference_materialization_stores_from_env().await;
    let materializer = CredentialInferenceMaterializer::new(stores.credentials, stores.secrets);
    run_configured(WorkerUpstream::new(upstream), Some(Arc::new(materializer))).await
}

/// Run a genuinely secretless worker with a deployment-provided materializer.
/// It receives each durable run's snapshot-pinned inference access and may
/// realize an executor through a remote broker without opening a credential
/// vault or persisting provider keys in this process.
pub async fn run_with_inference_materializer(
    upstream: &str,
    materializer: Arc<dyn InferenceExecutorMaterializer>,
) -> Result<(), Box<dyn std::error::Error>> {
    run_configured(WorkerUpstream::new(upstream), Some(materializer)).await
}

async fn run_configured(
    upstream: WorkerUpstream,
    materializer: Option<Arc<dyn InferenceExecutorMaterializer>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let upstream_url = upstream.base_url().to_string();
    let control = WorkerControlClient::new(upstream.clone());
    let registration = control
        .register(
            new_incarnation_id()?,
            worker_manifest(materializer.as_deref()),
        )
        .await
        .map_err(std::io::Error::other)?;
    let upstream = upstream.with_worker_identity(registration.snapshot.identity.clone());
    // Route the dispatch pool's claim/settle over HTTP to the cell server.
    awaken_runtime_host::init_shared_dispatch_store(
        awaken_runtime_host::worker_dispatch_store_with_upstream(
            &upstream,
            registration.snapshot.identity.clone(),
        ),
    );

    let mut host = SharedHost::new(Arc::new(NoModelConfiguredExecutor), "worker")
        .with_worker_upstream(upstream);
    if let Some(materializer) = materializer {
        host = host.with_inference_materializer(materializer);
    }
    awaken_server::install_platform_memory_data_plane(&host);

    // Serve `acp:*` runs this worker claims on the config-selected CLI, realized in the
    // worker's configured sandbox tier — the SAME env wiring the server root uses, so
    // the two never drift (ADR-0057). No selector set → no ACP backend, native only.
    host = host.with_acp_from_env().await;

    let host = Arc::new(host);
    let lifecycle = Arc::new(WorkerLifecycle {
        host: host.clone(),
        control: control.clone(),
        identity: registration.snapshot.identity,
    });
    // Publish Ready before starting the pull loop. Starting the pool while the
    // directory still says Starting creates a tight claim/reject race; publishing
    // first is safe because any assignment remains queued until this process starts
    // polling immediately below.
    let initial = control
        .heartbeat(
            &lifecycle.identity,
            WorkerHeartbeat {
                sequence: 1,
                ready: true,
                in_flight: 0,
            },
        )
        .await
        .map_err(std::io::Error::other)?;
    if initial != RegistryMutation::Applied {
        return Err(std::io::Error::other(format!(
            "initial worker heartbeat rejected: {initial:?}"
        ))
        .into());
    }
    host.ensure_dispatch_pool();
    let heartbeat = spawn_heartbeat(lifecycle.clone(), 2);
    eprintln!("awaken-worker draining from {upstream_url}");

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
            let router = admin::worker_admin_router_with_lifecycle(lifecycle.clone());
            eprintln!(
                "awaken-worker admin surface on {admin_addr} (/readyz /metrics /admin/drain)"
            );
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
    let grace = drain_grace(graceful);
    let deadline_ms = wall_clock_ms().saturating_add(grace.as_millis() as u64);
    if let Err(error) = lifecycle.begin_drain(Some(deadline_ms)).await {
        eprintln!("awaken-worker drain registration failed closed: {error}");
    }
    if !grace.is_zero() {
        eprintln!(
            "awaken-worker draining: finishing in-flight runs (≤{}s)",
            grace.as_secs()
        );
        wait_for_in_flight(&host, grace).await;
    }
    heartbeat.abort();
    if host.pool_in_flight() == 0 {
        let _ = control.mark_quiesced(&lifecycle.identity).await;
    }
    let _ = control.deregister(&lifecycle.identity).await;
    Ok(())
}

fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn new_incarnation_id() -> Result<String, getrandom::Error> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn worker_manifest(materializer: Option<&dyn InferenceExecutorMaterializer>) -> WorkerManifest {
    use awaken_provisioning_contract::{IsolationClass, SandboxCapabilities};
    let tier = std::env::var("AWAKEN_SANDBOX_TIER").unwrap_or_else(|_| "namespace".to_string());
    let (sandbox, backend) = match tier.as_str() {
        "local" => (WorkerManifest::default().sandbox, "local"),
        "docker" | "podman" | "k8s" => (
            SandboxCapabilities {
                isolation: IsolationClass::Container,
                tool_transparent: true,
                path_fidelity: true,
                enforced_readonly: true,
                network_isolation: true,
                secret_egress_substitution: false,
                resource_limits: true,
                custom_rootfs: true,
            },
            tier.as_str(),
        ),
        _ => (
            SandboxCapabilities {
                isolation: IsolationClass::Namespace,
                tool_transparent: true,
                path_fidelity: true,
                enforced_readonly: true,
                network_isolation: true,
                secret_egress_substitution: false,
                resource_limits: false,
                custom_rootfs: false,
            },
            "namespace",
        ),
    };
    let mut capabilities = std::collections::BTreeSet::from(["native-runtime".to_string()]);
    capabilities.extend(
        materializer
            .into_iter()
            .flat_map(InferenceExecutorMaterializer::supported_access_schemes)
            .map(|capability| (*capability).to_string()),
    );
    capabilities.extend(
        std::env::var("AWAKEN_WORKER_CAPABILITIES")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
    );
    WorkerManifest {
        build_digest: std::env::var("AWAKEN_WORKER_BUILD_DIGEST")
            .unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string()),
        capabilities,
        zone: std::env::var("AWAKEN_WORKER_ZONE").ok(),
        sandbox,
        sandbox_backends: std::collections::BTreeSet::from([backend.to_string()]),
        dispatch_contract: VersionRange::exact(1),
        runtime_protocol: VersionRange::exact(1),
        checkpoint_formats: std::collections::BTreeSet::from(["stream-v1".to_string()]),
        capacity: WorkerCapacity {
            max_concurrent: std::thread::available_parallelism()
                .map(|value| value.get() as u32)
                .unwrap_or(1),
            ..WorkerCapacity::default()
        },
        ..WorkerManifest::default()
    }
}

fn spawn_heartbeat(
    lifecycle: Arc<WorkerLifecycle>,
    mut sequence: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
        interval.tick().await;
        loop {
            interval.tick().await;
            let mutation = lifecycle
                .control
                .heartbeat(
                    &lifecycle.identity,
                    WorkerHeartbeat {
                        sequence,
                        ready: lifecycle.host.pool_accepting_work(),
                        in_flight: lifecycle.host.pool_in_flight(),
                    },
                )
                .await;
            sequence = sequence.saturating_add(1);
            match mutation {
                Ok(RegistryMutation::Applied) => {}
                Ok(other) => {
                    eprintln!("worker heartbeat lost authority: {other:?}; draining locally");
                    lifecycle.host.begin_pool_drain().await;
                    break;
                }
                Err(error) => {
                    eprintln!("worker heartbeat failed: {error}");
                }
            }
        }
    })
}

async fn wait_for_in_flight(host: &SharedHost, grace: std::time::Duration) {
    let deadline = tokio::time::Instant::now() + grace;
    while host.pool_in_flight() > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
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
    use std::sync::Arc;

    use awaken_runtime_contract::{InferenceAccess, llm::LlmExecutor};

    use super::{InferenceExecutorMaterializer, grace_window, worker_manifest};

    struct SchemeMaterializer;

    impl InferenceExecutorMaterializer for SchemeMaterializer {
        fn supported_access_schemes(&self) -> &'static [&'static str] {
            &["test-access/v1"]
        }

        fn materialize_pinned(
            &self,
            _model_ref: &str,
            _access: &InferenceAccess,
        ) -> Option<Arc<dyn LlmExecutor>> {
            None
        }
    }

    #[test]
    fn worker_manifest_derives_materialization_capabilities_from_the_adapter() {
        let materializer = SchemeMaterializer;
        let manifest = worker_manifest(Some(&materializer));

        assert!(manifest.capabilities.contains("native-runtime"));
        assert!(manifest.capabilities.contains("test-access/v1"));
    }

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

    // Boundary: an explicitly configured zero grace collapses SIGTERM to the
    // prompt-exit behavior — the orchestrator asked for no in-flight wait, so a
    // graceful stop must not silently substitute the 20s default.
    #[test]
    fn a_configured_zero_grace_exits_immediately_even_on_sigterm() {
        assert!(
            grace_window(true, Some(0)).is_zero(),
            "grace of 0 means no wait, not the default"
        );
    }
}
