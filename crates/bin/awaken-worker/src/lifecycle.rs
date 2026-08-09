//! Worker registry authority, heartbeat, and local admission lifecycle.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

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
    pub(crate) warm_environments:
        Arc<RwLock<BTreeMap<String, awaken_session_contract::EnvironmentSnapshot>>>,
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

    pub(crate) fn warm_environment_shapes(&self) -> BTreeSet<String> {
        self.warm_environments
            .read()
            .expect("warm Environment receipt lock")
            .iter()
            .filter(|(_, snapshot)| self.host.ready_environment_snapshot_capacity(snapshot) > 0)
            .map(|(shape, _)| shape.clone())
            .collect()
    }

    /// Reconcile derived desired state from Coordinator into the canonical Host
    /// capacity path. A failed refresh leaves the previous receipts untouched;
    /// a successful refresh drops shapes no longer present in the current catalog.
    pub(crate) async fn reconcile_environment_warmups(&self) -> Result<usize, String> {
        let desired = self
            .control
            .current_environment_warmups(&self.identity)
            .await?;
        let (per_shape, total) = self.host.environment_warmup_limits();
        let mut remaining = total;
        let mut desired_with_targets = BTreeMap::new();
        // Coordinator returns a stable Environment-id/revision order. Preserve it
        // while selecting a bounded subset so a catalog larger than local capacity
        // does not rotate LRU entries and recreate containers every reconciliation.
        for snapshot in desired {
            let shape = snapshot.runtime_shape_fingerprint().0;
            if desired_with_targets.contains_key(&shape) {
                continue;
            }
            let target = per_shape.min(remaining);
            if target == 0 {
                break;
            }
            remaining -= target;
            desired_with_targets.insert(shape, (snapshot, target));
        }
        let previous = self
            .warm_environments
            .read()
            .expect("warm Environment receipt lock")
            .clone();
        // Free shapes outside the deterministic selected set before adding their
        // replacements. This lets the pool remain bounded without eviction churn.
        for (shape, snapshot) in &previous {
            if !desired_with_targets.contains_key(shape) {
                self.host
                    .discard_environment_snapshot_capacity(snapshot)
                    .await;
            }
        }
        let mut next = previous
            .into_iter()
            .filter(|(shape, _)| desired_with_targets.contains_key(shape))
            .collect::<BTreeMap<_, _>>();
        for (shape, (snapshot, target)) in &desired_with_targets {
            match self
                .host
                .prewarm_environment_snapshot(snapshot, *target)
                .await
            {
                Ok(ready) if ready > 0 => {
                    next.insert(shape.clone(), snapshot.clone());
                }
                Ok(_) => {
                    next.remove(shape);
                }
                Err(error) => {
                    eprintln!(
                        "Environment shape prewarm failed for {}@{}; cold path retained: {error}",
                        snapshot.environment_id, snapshot.revision.0
                    );
                }
            }
        }
        *self
            .warm_environments
            .write()
            .expect("warm Environment receipt lock") = next;
        Ok(self.warm_environment_shapes().len())
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
                        warm_environment_shapes: lifecycle.warm_environment_shapes(),
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

pub(crate) fn spawn_environment_warmup_reconciliation(
    lifecycle: Arc<WorkerLifecycle>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
        interval.tick().await;
        loop {
            interval.tick().await;
            if let Err(error) = lifecycle.reconcile_environment_warmups().await {
                eprintln!(
                    "Environment warmup reconciliation failed; prior receipts retained: {error}"
                );
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
