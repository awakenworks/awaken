//! Worker registry authority, heartbeat, and local admission lifecycle.

use std::sync::Arc;

use awaken_runtime_host::SharedHost;
use awaken_worker_contract::{RegistryMutation, WorkerHeartbeat, WorkerIdentity};
use awaken_worker_runtime::WorkerControlClient;

use crate::credential_liveness::WorkerObservationCache;

/// Why the Worker lifecycle is stopping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerShutdown {
    /// Developer/foreground stop: close admission and exit without an in-flight
    /// grace window.
    Prompt,
    /// Orchestrator scale-in: close admission and honor the configured grace.
    Graceful,
}

#[derive(Clone)]
pub(crate) struct WorkerLifecycle {
    pub(crate) host: Arc<SharedHost>,
    pub(crate) control: WorkerControlClient,
    pub(crate) identity: WorkerIdentity,
    pub(crate) credential_observation_resolver:
        Option<Arc<dyn awaken_runtime_contract::WorkerLocalCredentialResolver>>,
    pub(crate) acp_capability_observation_source:
        Option<Arc<dyn awaken_acp_contract::AcpCapabilityObservationSource>>,
    pub(crate) observations: Arc<WorkerObservationCache>,
    pub(crate) observation_ttl: std::time::Duration,
}

impl WorkerLifecycle {
    pub(crate) async fn begin_drain(&self, deadline_ms: Option<u64>) -> Result<(), String> {
        // The process-local claim gate is the drain linearization point. Fence it
        // before publishing Draining to the Registry so no still-open claim loop
        // can race the remote state transition and receive a predictable rejection.
        self.host.begin_pool_drain().await;
        let remote = self.control.begin_drain(&self.identity, deadline_ms).await;
        match remote? {
            RegistryMutation::Applied => Ok(()),
            other => Err(format!("worker drain rejected: {other:?}")),
        }
    }

    /// Trigger the canonical atomic observation operation immediately.
    ///
    /// The periodic loop, startup and this event entrypoint share the cache's
    /// coalescing guard, so remediation never creates a parallel discovery path.
    pub(crate) async fn refresh_observations(&self) -> Result<(), String> {
        self.observations
            .refresh(
                self.credential_observation_resolver.as_deref(),
                self.acp_capability_observation_source.as_deref(),
                wall_clock_ms(),
                self.observation_ttl,
            )
            .await
    }
}

pub(crate) fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(crate) fn new_incarnation_id() -> Result<String, getrandom::Error> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub(crate) fn spawn_heartbeat(
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
                        credential_observations: lifecycle.observations.credential_snapshot(),
                        acp_capability_observations: lifecycle
                            .observations
                            .acp_capability_snapshot(),
                    },
                )
                .await;
            sequence = sequence.saturating_add(1);
            match mutation {
                Ok(RegistryMutation::Applied) => {
                    // Session projection leases are shorter than Worker
                    // authority and renew through the canonical realization
                    // protocol. The Control endpoint caps the requested expiry
                    // by the freshly-heartbeated registry lease.
                    let now = wall_clock_ms();
                    if let Err(error) = lifecycle
                        .host
                        .renew_due_session_realizations(
                            now.saturating_add(15_000),
                            now.saturating_add(20_000),
                        )
                        .await
                    {
                        eprintln!("Session realization renewal failed closed: {error}");
                        revoke_worker_session_authority(&lifecycle).await;
                        break;
                    }
                }
                Ok(other) => {
                    eprintln!("worker heartbeat lost authority: {other:?}; draining locally");
                    revoke_worker_session_authority(&lifecycle).await;
                    break;
                }
                Err(error) => {
                    eprintln!(
                        "worker heartbeat cannot prove continuing authority: {error}; draining locally"
                    );
                    revoke_worker_session_authority(&lifecycle).await;
                    break;
                }
            }
        }
    })
}

async fn revoke_worker_session_authority(lifecycle: &WorkerLifecycle) {
    lifecycle.host.begin_pool_drain().await;
    // Fail closed without racing terminal sandbox disposal against in-flight
    // native tools. Cancellation stops each process group (including descendant
    // containers), and the bounded drain lets those futures release their
    // workspace handles before the directories are reaped below.
    lifecycle.host.interrupt_all_session_runs().await;
    wait_for_in_flight(&lifecycle.host, std::time::Duration::from_secs(20)).await;
    if let Err(error) = lifecycle.host.revoke_all_session_realizations().await {
        eprintln!("Session realization revocation remains incomplete: {error}");
    }
}

pub(crate) async fn wait_for_in_flight(host: &SharedHost, grace: std::time::Duration) {
    let deadline = tokio::time::Instant::now() + grace;
    while host.pool_in_flight() > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// The grace policy (pure): a SIGTERM (orchestrator scale-in) waits `configured` or
/// the 20s default so in-flight runs finish; a SIGINT (developer ctrl-c) exits
/// promptly (zero) so the foreground stop is snappy.
pub(crate) fn grace_window(graceful: bool, configured_secs: Option<u64>) -> std::time::Duration {
    if !graceful {
        return std::time::Duration::ZERO;
    }
    std::time::Duration::from_secs(configured_secs.unwrap_or(20))
}
