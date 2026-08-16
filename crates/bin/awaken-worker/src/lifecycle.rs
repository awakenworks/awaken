//! Worker registry authority, heartbeat, and local admission lifecycle.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use awaken_runtime_host::SharedHost;
use awaken_worker_contract::{RegistryMutation, WorkerHeartbeat, WorkerIdentity};
use awaken_worker_runtime::WorkerControlClient;

use crate::credential_liveness::WorkerObservationCache;

#[derive(Clone)]
enum WarmCapacityDemand {
    Environment(Box<awaken_session_contract::EnvironmentSnapshot>),
    Default,
}

#[derive(Clone)]
struct WarmCapacityTarget {
    shape: awaken_provisioning_contract::SandboxCapacityShapeId,
    target: usize,
    demand: WarmCapacityDemand,
}

/// The one Worker-owned allocation of the process-global warm-container budget.
/// Exact current Environment shapes have priority; the generic default shape uses
/// only the remaining capacity and is deduplicated when it is already selected.
struct WarmCapacityPlan {
    targets: Vec<WarmCapacityTarget>,
}

impl WarmCapacityPlan {
    fn build(
        host: &SharedHost,
        desired: Vec<awaken_session_contract::EnvironmentSnapshot>,
    ) -> Self {
        let (per_shape, total) = host.environment_warmup_limits();
        if per_shape == 0 || total == 0 {
            return Self {
                targets: Vec::new(),
            };
        }
        let mut remaining = total;
        let mut selected = BTreeSet::new();
        let mut targets = Vec::new();
        for snapshot in desired {
            if snapshot.self_hosted || remaining == 0 {
                continue;
            }
            let shape = host.environment_snapshot_capacity_shape(&snapshot);
            if !selected.insert(shape.clone()) {
                continue;
            }
            let target = per_shape.min(remaining);
            remaining -= target;
            targets.push(WarmCapacityTarget {
                shape,
                target,
                demand: WarmCapacityDemand::Environment(Box::new(snapshot)),
            });
        }
        if remaining > 0 {
            let shape = host.default_environment_capacity_shape();
            if selected.insert(shape.clone()) {
                targets.push(WarmCapacityTarget {
                    shape,
                    target: per_shape.min(remaining),
                    demand: WarmCapacityDemand::Default,
                });
            }
        }
        Self { targets }
    }

    fn shapes(&self) -> BTreeSet<awaken_provisioning_contract::SandboxCapacityShapeId> {
        self.targets
            .iter()
            .map(|target| target.shape.clone())
            .collect()
    }
}

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
pub(crate) struct WorkerSupervisor {
    pub(crate) host: Arc<SharedHost>,
    pub(crate) control: WorkerControlClient,
    pub(crate) identity: WorkerIdentity,
    pub(crate) credential_observation_resolver:
        Option<Arc<dyn awaken_runtime_contract::WorkerLocalCredentialResolver>>,
    pub(crate) acp_capability_observation_source:
        Option<Arc<dyn awaken_acp_contract::AcpCapabilityObservationSource>>,
    pub(crate) observations: Arc<WorkerObservationCache>,
    pub(crate) observation_ttl: std::time::Duration,
    pub(crate) warm_environments: Arc<
        RwLock<
            BTreeMap<
                awaken_provisioning_contract::SandboxCapacityShapeId,
                awaken_session_contract::EnvironmentSnapshot,
            >,
        >,
    >,
}

impl WorkerSupervisor {
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

    pub(crate) fn warm_environment_shapes(
        &self,
    ) -> BTreeSet<awaken_provisioning_contract::SandboxCapacityShapeId> {
        self.warm_environments
            .read()
            .expect("warm Environment receipt lock")
            .iter()
            .filter(|(_, snapshot)| self.host.ready_environment_snapshot_capacity(snapshot) > 0)
            .map(|(shape, _)| shape.clone())
            .collect()
    }

    /// Reconcile Coordinator-derived Environment demand and the default Session
    /// demand through one global plan. A failed pull reuses the last successful
    /// Environment set, so it neither erases receipts nor bypasses the default
    /// share of the same budget.
    pub(crate) async fn reconcile_environment_warmups(&self) -> Result<usize, String> {
        let previous = self
            .warm_environments
            .read()
            .expect("warm Environment receipt lock")
            .clone();
        let fetched = self
            .control
            .current_environment_warmups(&self.identity)
            .await;
        let (desired, fetch_error) = match fetched {
            Ok(desired) => (desired, None),
            Err(error) => (previous.values().cloned().collect(), Some(error)),
        };
        let plan = WarmCapacityPlan::build(&self.host, desired);
        let selected_shapes = plan.shapes();

        // Release every plan-external shape before creating replacements. Both
        // default and Environment capacity are compared with the same typed id,
        // so an identical projection cannot discard its own selected capacity.
        for (shape, snapshot) in &previous {
            if !selected_shapes.contains(shape) {
                self.host
                    .discard_environment_snapshot_capacity(snapshot)
                    .await;
            }
        }
        let default_shape = self.host.default_environment_capacity_shape();
        if !selected_shapes.contains(&default_shape) {
            self.host.discard_default_environment_capacity().await;
        }
        let mut next = previous
            .into_iter()
            .filter(|(shape, _)| selected_shapes.contains(shape))
            .collect::<BTreeMap<_, _>>();
        for target in &plan.targets {
            let result = match &target.demand {
                WarmCapacityDemand::Environment(snapshot) => {
                    self.host
                        .prewarm_environment_snapshot(snapshot, target.target)
                        .await
                }
                WarmCapacityDemand::Default => {
                    self.host
                        .prewarm_default_environment_capacity(target.target)
                        .await
                }
            };
            match (result, &target.demand) {
                (Ok(ready), WarmCapacityDemand::Environment(snapshot)) if ready > 0 => {
                    next.insert(target.shape.clone(), snapshot.as_ref().clone());
                }
                (Ok(_), WarmCapacityDemand::Environment(_)) => {
                    next.remove(&target.shape);
                }
                (Ok(_), WarmCapacityDemand::Default) => {}
                (Err(error), WarmCapacityDemand::Environment(snapshot)) => eprintln!(
                    "Environment shape prewarm failed for {}@{}; cold path retained: {error}",
                    snapshot.environment_id, snapshot.revision.0
                ),
                (Err(error), WarmCapacityDemand::Default) => {
                    eprintln!("default Session shape prewarm failed; cold path retained: {error}")
                }
            }
        }
        *self
            .warm_environments
            .write()
            .expect("warm Environment receipt lock") = next;
        let ready = self.warm_environment_shapes().len();
        if let Some(error) = fetch_error {
            Err(error)
        } else {
            Ok(ready)
        }
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
    lifecycle: Arc<WorkerSupervisor>,
    mut sequence: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
        // A suspended laptop or paused VM can miss many ticks. Replaying them as
        // a burst contends with the co-located Coordinator just when it is also
        // recovering. One fresh heartbeat is authoritative; stale catch-up ticks
        // add load without extending the lease further.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            let mutation = tokio::time::timeout(
                std::time::Duration::from_secs(8),
                lifecycle.control.heartbeat(
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
                ),
            )
            .await;
            sequence = sequence.saturating_add(1);
            match mutation {
                Ok(Ok(RegistryMutation::Applied)) => {
                    // Session projection leases are shorter than Worker
                    // authority and renew through the canonical realization
                    // protocol. The Control endpoint caps the requested expiry
                    // by the freshly-heartbeated registry lease.
                    let now = wall_clock_ms();
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(3),
                        lifecycle.host.renew_due_session_realizations(
                            now.saturating_add(15_000),
                            now.saturating_add(20_000),
                        ),
                    )
                    .await
                    {
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => eprintln!(
                            "Session realization renewal failed closed; Worker heartbeat continues: {error}"
                        ),
                        Err(_) => eprintln!(
                            "Session realization renewal exceeded 3s; projections retain their existing deadlines and Worker heartbeat continues"
                        ),
                    }
                }
                Ok(Ok(other)) => {
                    eprintln!("worker heartbeat lost authority: {other:?}; draining locally");
                    revoke_worker_session_authority(&lifecycle).await;
                    break;
                }
                Ok(Err(error)) => {
                    eprintln!(
                        "worker heartbeat cannot prove continuing authority: {error}; draining locally"
                    );
                    revoke_worker_session_authority(&lifecycle).await;
                    break;
                }
                Err(_) => {
                    eprintln!(
                        "worker heartbeat exceeded 8s and cannot prove continuing authority; draining locally"
                    );
                    revoke_worker_session_authority(&lifecycle).await;
                    break;
                }
            }
        }
    })
}

pub(crate) fn spawn_environment_warmup_reconciliation(
    lifecycle: Arc<WorkerSupervisor>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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

async fn revoke_worker_session_authority(lifecycle: &WorkerSupervisor) {
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
