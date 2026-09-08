//! Worker registry authority, heartbeat, and local admission lifecycle.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use awaken_runtime_contract::authority_lease::AuthorityLeaseTiming;
use awaken_session_executor::SharedHost;
use awaken_worker_contract::{RegistryMutation, WorkerIdentity};
use awaken_coordinator_client::WorkerControlClient;
use futures_util::FutureExt as _;

use crate::credential_liveness::WorkerObservationCache;

mod environment_evidence;

pub(crate) use environment_evidence::proof_is_fresh as environment_proof_is_fresh;
mod authority;
pub(crate) use authority::WorkerAuthorityProof;
pub(crate) use authority::authority_proof_started;

const SESSION_REALIZATION_LEASE_TTL_MS: u64 = 20_000;

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
    pub(crate) authority: Arc<WorkerAuthorityProof>,
    pub(crate) host: Arc<SharedHost>,
    pub(crate) control: WorkerControlClient,
    pub(crate) identity: WorkerIdentity,
    pub(crate) credential_observation_resolver:
        Option<Arc<dyn awaken_runtime_contract::WorkerLocalCredentialResolver>>,
    pub(crate) acp_capability_observation_source:
        Option<Arc<dyn awaken_acp_contract::AcpCapabilityObservationSource>>,
    pub(crate) session_environment_provider:
        Option<Arc<dyn awaken_sandbox_container::ContainerEnvironmentProvider>>,
    pub(crate) observations: Arc<WorkerObservationCache>,
    pub(crate) observation_ttl: std::time::Duration,
    pub(crate) probe_interval: std::time::Duration,
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
    sequence: u64,
    timing: AuthorityLeaseTiming,
    environment_proof_started: Instant,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Independent, single-flight lanes under the same incarnation owner.
        // Neither a slow security probe nor a registry retry can extend the
        // other's proof. Losing either proof irreversibly drains both lanes.
        supervise_worker_authority(
            async {
                tokio::select! {
                    biased;
                    () = async {
                        if let Some(provider) = &lifecycle.session_environment_provider {
                            environment_evidence::monitor(
                                tokio::time::Instant::from_std(environment_proof_started),
                                timing,
                                || provider.probe_ready(),
                            ).await;
                        } else {
                            std::future::pending::<()>().await;
                        }
                    } => {}
                    () = renew_registry_authority(&lifecycle, sequence).boxed() => {}
                }
            }
            .boxed(),
            || revoke_worker_session_authority(&lifecycle).boxed(),
        )
        .await;
    })
}

/// One exit edge for normal proof loss and unwinding dependency failures. This
/// stays in the heartbeat task, so it also supervises authority during graceful
/// drain. It never restarts the failed loop or creates a second monitor task.
/// Explicit owner abort/process death is outside unwind handling; the physical
/// execution cutoff and final shutdown owner retain those responsibilities.
async fn supervise_worker_authority<Revoke, Revocation>(
    heartbeat: impl std::future::Future<Output = ()>,
    revoke: Revoke,
) where
    Revoke: FnOnce() -> Revocation,
    Revocation: std::future::Future<Output = ()>,
{
    if std::panic::AssertUnwindSafe(heartbeat)
        .catch_unwind()
        .await
        .is_err()
    {
        awaken_observability::record_worker_authority_loss("heartbeat_panicked");
        tracing::error!("worker heartbeat panicked; revoking Session authority before task exit");
    }
    // The heartbeat future has been dropped before revocation. Borrowed
    // provider/transport state is never reused after an unwind.
    revoke().await;
}

async fn renew_registry_authority(lifecycle: &WorkerSupervisor, mut sequence: u64) {
    let Ok((receipt, remaining)) = lifecycle.authority.snapshot() else {
        return;
    };
    awaken_observability::set_worker_authority_proof_remaining(remaining);
    let mut next_attempt = receipt.started.scheduled() + receipt.timing.renew_interval();
    let mut next_regular = next_attempt;
    let mut admission_suspended = false;
    let mut consecutive_failures = 0_u64;
    loop {
        tokio::time::sleep_until(tokio::time::Instant::from_std(next_attempt)).await;
        let Ok(attempt) = authority_proof_started() else {
            break;
        };
        let attempt_started = attempt.scheduled();
        let scheduling_lag = attempt_started.saturating_duration_since(next_attempt);
        let Ok((receipt, _)) = lifecycle.authority.snapshot() else {
            awaken_observability::record_worker_authority_loss("proof_expired");
            tracing::error!(
                sequence,
                consecutive_failures,
                "worker registry authority proof expired; draining locally"
            );
            break;
        };
        let timing = receipt.timing;
        let ready = lifecycle.host.pool_can_resume_work().await;
        let heartbeat_timeout = timing
            .request_timeout()
            .min(lifecycle.authority.remaining());
        let heartbeat = lifecycle.observations.snapshot().into_heartbeat(
            sequence,
            ready,
            lifecycle.host.pool_in_flight(),
            lifecycle.warm_environment_shapes(),
        );
        let mutation = tokio::time::timeout(
            heartbeat_timeout,
            lifecycle.control.heartbeat(&lifecycle.identity, heartbeat),
        )
        .await;
        sequence = match sequence.checked_add(1) {
            Some(next) => next,
            None => {
                awaken_observability::record_worker_authority_loss("sequence_exhausted");
                tracing::error!("worker heartbeat sequence exhausted; draining locally");
                break;
            }
        };
        match mutation {
            Ok(Ok(receipt)) if receipt.mutation == RegistryMutation::Applied => {
                let recovered = admission_suspended;
                let receipt_ttl = receipt
                    .lease_ttl_ms
                    .expect("WorkerControlClient validates applied heartbeat TTL");
                let previous_ttl = timing.lease_ttl().as_millis() as u64;
                let timing = AuthorityLeaseTiming::from_ttl_ms(receipt_ttl);
                if let Err(error) = crate::credential_liveness::validate_observation_timing(
                    lifecycle.probe_interval,
                    lifecycle.observation_ttl,
                    Some(timing),
                ) {
                    awaken_observability::record_worker_authority_loss(
                        "observation_timing_incompatible",
                    );
                    tracing::error!(%error, "Worker observation publication budget lost; draining locally");
                    break;
                }
                // The Coordinator applied the renewal at an unknown instant
                // between request start and response receipt. Request start is
                // the conservative local lower bound; response latency must
                // never mint extra local authority.
                if lifecycle.authority.apply(attempt, timing).is_err() {
                    awaken_observability::record_worker_authority_loss("proof_expired");
                    break;
                }
                consecutive_failures = 0;
                if admission_suspended {
                    if lifecycle.host.resume_pool_admission().await {
                        tracing::info!(
                            "worker registry authority re-proven; claim admission resumed"
                        );
                    } else {
                        tracing::info!(
                            "worker registry authority re-proven while admission remains permanently draining"
                        );
                    }
                    admission_suspended = false;
                }
                if receipt_ttl == previous_ttl {
                    while next_regular <= attempt_started {
                        next_regular += timing.renew_interval();
                    }
                } else {
                    next_regular = attempt_started + timing.renew_interval();
                }
                next_attempt = next_regular;
                awaken_observability::record_worker_authority_heartbeat(
                    if recovered {
                        "applied_after_retry"
                    } else {
                        "applied"
                    },
                    Instant::now().duration_since(attempt_started),
                    scheduling_lag,
                    lifecycle.authority.remaining(),
                );
            }
            Ok(Ok(receipt)) => {
                awaken_observability::record_worker_authority_heartbeat(
                    "rejected",
                    Instant::now().duration_since(attempt_started),
                    scheduling_lag,
                    lifecycle.authority.remaining(),
                );
                awaken_observability::record_worker_authority_loss("registry_rejected");
                tracing::error!(mutation = ?receipt.mutation, "worker heartbeat explicitly lost authority; draining locally");
                break;
            }
            Ok(Err(error)) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                let now = Instant::now();
                let remaining = lifecycle.authority.remaining();
                tracing::warn!(
                    error = %error,
                    consecutive_failures,
                    proof_remaining_ms = remaining.as_millis(),
                    attempt_duration_ms = now.duration_since(attempt_started).as_millis(),
                    "worker heartbeat transport failed; claim admission paused and retrying"
                );
                awaken_observability::record_worker_authority_heartbeat(
                    "transport_error",
                    now.duration_since(attempt_started),
                    scheduling_lag,
                    remaining,
                );
                let Some(retry) = pause_authority_admission_for_retry(
                    lifecycle,
                    &mut admission_suspended,
                    timing,
                )
                .await
                else {
                    break;
                };
                next_attempt = retry;
            }
            Err(_) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                let now = Instant::now();
                let remaining = lifecycle.authority.remaining();
                tracing::warn!(
                    consecutive_failures,
                    proof_remaining_ms = remaining.as_millis(),
                    attempt_duration_ms = now.duration_since(attempt_started).as_millis(),
                    "worker heartbeat request timed out; claim admission paused and retrying"
                );
                awaken_observability::record_worker_authority_heartbeat(
                    "timeout",
                    now.duration_since(attempt_started),
                    scheduling_lag,
                    remaining,
                );
                let Some(retry) = pause_authority_admission_for_retry(
                    lifecycle,
                    &mut admission_suspended,
                    timing,
                )
                .await
                else {
                    break;
                };
                next_attempt = retry;
            }
        }
    }
}

// An unavailable response is not evidence of authority loss. Reuse the same
// temporary admission gate for Registry transport errors and timeouts. Neither
// a retry nor a successful probe advances the last durable Registry proof.
async fn pause_authority_admission_for_retry(
    lifecycle: &WorkerSupervisor,
    admission_suspended: &mut bool,
    timing: AuthorityLeaseTiming,
) -> Option<Instant> {
    if !*admission_suspended {
        lifecycle.host.suspend_pool_admission().await;
        *admission_suspended = true;
    }
    let now = Instant::now();
    let remaining = lifecycle.authority.remaining();
    if remaining.is_zero() {
        awaken_observability::record_worker_authority_loss("proof_expired");
        None
    } else {
        Some(now + timing.retry_delay().min(remaining))
    }
}

/// Keep due-only lease renewal and cold terminal recovery under one
/// process-owned supervisor without making either cadence cancel the other's
/// future. The cold claim-next lane is the sole recovery scheduler and awaits
/// each Artifact, Memory, Repository, or provider effect to its durable
/// boundary. Ordinary terminal settlement remains on the initiating command
/// path; renewal runs beside recovery because one Session's physical cleanup
/// cannot consume another Session's finite realization lease.
async fn supervise_session_realization_reconciliation<Renewal, RenewalFuture, Cold, ColdFuture>(
    renewal_period: std::time::Duration,
    cold_period: std::time::Duration,
    mut renewal: Renewal,
    mut cold: Cold,
) where
    Renewal: FnMut() -> RenewalFuture,
    RenewalFuture: std::future::Future<Output = ()>,
    Cold: FnMut() -> ColdFuture,
    ColdFuture: std::future::Future<Output = ()>,
{
    let renewals = async move {
        let mut interval = tokio::time::interval(renewal_period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            renewal().await;
        }
    };
    let cold_recovery = async move {
        let mut interval =
            tokio::time::interval_at(tokio::time::Instant::now() + cold_period, cold_period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            // Await the complete durable recovery effect. A missed poll is
            // skipped instead of cancelling this future or overlapping it with
            // another claim-next drive.
            cold().await;
        }
    };
    tokio::join!(renewals, cold_recovery);
}

/// Renew resident Session leases promptly while an independent cold lane
/// recovers terminal roots. Control authenticates the current Worker registry
/// lease on every cold claim; the heartbeat task remains the sole
/// registry-liveness owner. The cold Host/contract cleanup driver is
/// single-flight, while unrelated lease renewal stays schedulable during its
/// durable I/O.
pub(crate) fn spawn_session_realization_reconciliation(
    lifecycle: Arc<WorkerSupervisor>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let timing = AuthorityLeaseTiming::from_ttl_ms(SESSION_REALIZATION_LEASE_TTL_MS);
        let renewal_lifecycle = lifecycle.clone();
        let cold_lifecycle = lifecycle;
        supervise_session_realization_reconciliation(
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(10),
            move || {
                let lifecycle = renewal_lifecycle.clone();
                async move {
                    // Cause/effect decision table: C1 this cadence owns only
                    // lease renewal; C2 the Host bounds independent due
                    // Sessions; C3 each Control call has an authority-derived
                    // deadline; C4 cold recovery may still be running on the
                    // independent recovery lane. Effects: E1 no overlapping
                    // renewal sweep; E2 no batch-wide cancellation; E3 cleanup
                    // cannot consume another Session's proof window; E4 a
                    // non-due Session causes no Control call.
                    //
                    // | Rule | due | slow renewal | recovery running | Effect |
                    // | R1 | yes | no | any | E1 + E2 + E3 |
                    // | R2 | yes | yes | any | E1 + E2 + E3 |
                    // | R3 | no | any | any | E4 |
                    if let Err(error) = lifecycle
                        .host
                        .renew_due_session_realizations(timing)
                        .boxed()
                        .await
                    {
                        eprintln!(
                            "Session realization renewal failed closed; affected projections retain only their existing authority: {error}"
                        );
                    }
                }
            },
            move || {
                let lifecycle = cold_lifecycle.clone();
                async move {
                    let now = wall_clock_ms();
                    let cleanup_target = awaken_session_contract::SessionRealizationTarget {
                        owner: lifecycle.identity.worker_id.clone(),
                        runtime_incarnation: lifecycle.identity.lease_owner(),
                        lease_expires_at_unix_ms: now
                            .saturating_add(SESSION_REALIZATION_LEASE_TTL_MS),
                        reassign_existing_lease: false,
                    };
                    if let Err(error) = lifecycle
                        .host
                        .recover_terminal_cleanup_assignments(cleanup_target)
                        .boxed()
                        .await
                    {
                        eprintln!(
                            "cold Session terminal cleanup recovery remained pending; Worker reconciliation continues: {error}"
                        );
                    }
                }
            },
        )
        .await;
    })
}

pub(crate) fn spawn_environment_warmup_reconciliation(
    lifecycle: Arc<WorkerSupervisor>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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

pub(crate) async fn revoke_worker_session_authority(lifecycle: &WorkerSupervisor) {
    // Root guard closes before selecting slots or awaiting any drain/owner lock.
    // A late/new Session cannot install authority outside this selected cohort.
    lifecycle.authority.close();
    awaken_observability::set_worker_authority_proof_remaining(std::time::Duration::ZERO);
    let revocation = lifecycle.host.begin_session_realization_revocation();
    lifecycle.host.begin_pool_drain().await;
    // Fail closed without racing terminal sandbox disposal against in-flight
    // native tools. Request cancellation and observe bounded executor return
    // before detaching local projections. Unknown external effects retain their
    // physical Dispatch slot; durable Session environments are not disposed.
    let interrupted = lifecycle.host.interrupt_all_session_runs().await;
    let returned = lifecycle
        .host
        .drain_runtime(std::time::Duration::from_secs(20))
        .await;
    if !returned {
        eprintln!(
            "Worker cancellation drain expired after interrupting {interrupted} local Sessions; unconfirmed physical attempts remain fenced"
        );
    }
    if let Err(error) = lifecycle
        .host
        .finish_session_realization_revocation(revocation)
        .await
    {
        eprintln!("Session realization revocation remains incomplete: {error}");
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::{supervise_session_realization_reconciliation, supervise_worker_authority};

    #[tokio::test]
    async fn heartbeat_exit_owns_one_revocation_before_completion_even_after_unwind() {
        // Cause/effect graph: C1 heartbeat returns normally or unwinds after a
        // dependency await; C2 cleanup finishes or the owner cancels its wait;
        // C3 explicit owner abort occurs before heartbeat exit instead.
        // E1 release heartbeat-owned temporaries before revocation; E2 invoke
        // the SAME revocation exactly once; E3 never complete while revocation
        // is pending; E4 cancellation cannot manufacture a completed receipt.
        // | rule | heartbeat | owner action | effect |
        // | H1-H2 | return/panic | finish cleanup | E1+E2+E3, then completion |
        // | H3-H4 | return/panic | cancel cleanup | E1+E2+E4, closure retained |
        // | H5-H6 | still pending | abort heartbeat | no invented revoke/receipt |
        // H5-H6 explicitly bound this unwind guarantee: process death/abort
        // requires physical cutoff and the outer shutdown owner, not catch_unwind.
        struct DropMark(Arc<AtomicBool>);
        impl Drop for DropMark {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        for panics in [false, true] {
            for action in ["finish", "cancel-cleanup", "abort-heartbeat"] {
                let dropped = Arc::new(AtomicBool::new(false));
                let calls = Arc::new(AtomicUsize::new(0));
                let closed = Arc::new(AtomicBool::new(false));
                let completed = Arc::new(AtomicBool::new(false));
                let (started, observed) = tokio::sync::oneshot::channel();
                let (exit, exit_wait) = tokio::sync::oneshot::channel();
                let (closing, close_observed) = tokio::sync::oneshot::channel();
                let (finish, finish_wait) = tokio::sync::oneshot::channel();
                let heartbeat_guard = DropMark(dropped.clone());
                let task = tokio::spawn(supervise_worker_authority(
                    async move {
                        let _guard = heartbeat_guard;
                        started.send(()).unwrap();
                        exit_wait.await.unwrap();
                        assert!(!panics, "injected heartbeat dependency panic");
                    },
                    {
                        let dropped = dropped.clone();
                        let calls = calls.clone();
                        let closed = closed.clone();
                        let completed = completed.clone();
                        move || async move {
                            assert!(dropped.load(Ordering::SeqCst), "E1");
                            assert_eq!(calls.fetch_add(1, Ordering::SeqCst), 0, "E2");
                            closed.store(true, Ordering::SeqCst);
                            closing.send(()).unwrap();
                            finish_wait.await.unwrap();
                            completed.store(true, Ordering::SeqCst);
                        }
                    },
                ));
                observed.await.unwrap();
                assert_eq!(calls.load(Ordering::SeqCst), 0);
                if action == "abort-heartbeat" {
                    task.abort();
                    assert!(task.await.unwrap_err().is_cancelled(), "H5-H6");
                    assert!(dropped.load(Ordering::SeqCst));
                    assert!(!closed.load(Ordering::SeqCst));
                    assert!(!completed.load(Ordering::SeqCst));
                    assert_eq!(calls.load(Ordering::SeqCst), 0);
                    continue;
                }
                exit.send(()).unwrap();
                close_observed.await.unwrap();
                assert!(!task.is_finished(), "E3");
                assert!(closed.load(Ordering::SeqCst));
                assert!(!completed.load(Ordering::SeqCst));
                if action == "cancel-cleanup" {
                    task.abort();
                    assert!(task.await.unwrap_err().is_cancelled(), "E4");
                    assert!(!completed.load(Ordering::SeqCst));
                    assert!(closed.load(Ordering::SeqCst));
                } else {
                    finish.send(()).unwrap();
                    task.await.unwrap();
                    assert!(completed.load(Ordering::SeqCst));
                }
                assert_eq!(calls.load(Ordering::SeqCst), 1, "E2");
            }
        }
    }

    #[tokio::test]
    async fn revocation_unwind_is_not_retried_or_reported_as_completed() {
        // H7: revocation itself unwinds after closing admission (cause).
        // It must escape to the outer Worker owner, not restart heartbeat,
        // retry destructive cleanup, or turn the panic into normal completion.
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let task = tokio::spawn(supervise_worker_authority(async {}, move || async move {
            observed.fetch_add(1, Ordering::SeqCst);
            panic!("injected revocation panic");
        }));
        assert!(task.await.unwrap_err().is_panic());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    struct ProbeProvider {
        mode: u8,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl awaken_sandbox_container::ContainerEnvironmentProvider for ProbeProvider {
        async fn probe_ready(&self) -> Result<(), awaken_provisioning_contract::SandboxError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            match (self.mode, call) {
                (1, 0) | (2, _) => std::future::pending().await,
                (3, _) => Err(awaken_provisioning_contract::SandboxError(
                    "evidence lost".into(),
                )),
                _ => Ok(()),
            }
        }

        async fn create_environment(
            &self,
            spec: &awaken_provisioning_contract::SandboxSpec,
        ) -> Result<
            Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
            awaken_provisioning_contract::SandboxError,
        > {
            crate::tests::UnusedContainerProvider
                .create_environment(spec)
                .await
        }

        async fn adopt_environment(
            &self,
            adoption: awaken_sandbox_container::ContainerEnvironmentAdoption<'_>,
        ) -> Result<
            Arc<dyn awaken_sandbox_container::ContainerEnvironment>,
            awaken_provisioning_contract::SandboxError,
        > {
            crate::tests::UnusedContainerProvider
                .adopt_environment(adoption)
                .await
        }
    }

    #[tokio::test]
    async fn sandbox_probe_timeout_retries_only_inside_the_existing_authority_window() {
        // Causes: P0 ready; P1 first probe times out then recovers; P2 all probes
        // time out; P3 provider explicitly rejects its evidence. Effects: P0/P1
        // heartbeat with the SAME incarnation and first sequence, no supervisor
        // exit; P2 stops at the ORIGINAL environment proof deadline even while
        // independent registry heartbeats succeed;
        // P3 stops immediately without retry. Timeouts must neither mint a new
        // proof nor become evidence of physical termination. Pool suspension's
        // in-flight preservation and absorbing drain are owned by its own tests.
        // Independent lanes can deliver their first heartbeat before P3's
        // rejecting probe returns. That earlier delivery is valid; require
        // terminal authority closure, not a scheduler-dependent empty queue.
        use super::*;
        use axum::{Json, Router, routing::post};
        use std::time::Duration;
        for mode in [0, 1, 2, 3] {
            let (sender, mut received) = tokio::sync::mpsc::channel(4);
            let router = Router::new().route(
                "/v1/worker/heartbeat",
                post(move |Json(body): Json<serde_json::Value>| {
                    let sender = sender.clone();
                    async move {
                        sender.send(body).await.unwrap();
                        Json(serde_json::json!({"mutation":"applied", "lease_ttl_ms":6000}))
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            });
            let provider = Arc::new(ProbeProvider {
                mode,
                calls: AtomicUsize::new(0),
            });
            let identity = WorkerIdentity::new("probe-worker", "same-incarnation", 1);
            let authority = Arc::new(WorkerAuthorityProof::default());
            authority
                .apply(
                    authority_proof_started().unwrap(),
                    AuthorityLeaseTiming::from_ttl_ms(6000),
                )
                .unwrap();
            let lifecycle = Arc::new(WorkerSupervisor {
                authority: authority.clone(),
                host: Arc::new(SharedHost::new(
                    Arc::new(awaken_session_executor::NoModelConfiguredExecutor),
                    "none",
                )),
                control: WorkerControlClient::new(
                    awaken_worker_transport_security::WorkerUpstream::new(format!(
                        "http://{address}"
                    )),
                ),
                identity: identity.clone(),
                credential_observation_resolver: None,
                acp_capability_observation_source: None,
                session_environment_provider: Some(provider.clone()),
                observations: Arc::new(WorkerObservationCache::default()),
                observation_ttl: Duration::from_secs(6),
                probe_interval: Duration::from_secs(1),
                warm_environments: Default::default(),
            });
            let task = spawn_heartbeat(
                lifecycle,
                1,
                AuthorityLeaseTiming::from_ttl_ms(6000),
                Instant::now(),
            );
            if mode < 2 {
                let heartbeat = tokio::time::timeout(Duration::from_secs(5), received.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    heartbeat["identity"],
                    serde_json::to_value(identity).unwrap()
                );
                assert_eq!(heartbeat["heartbeat"]["sequence"], 1);
                tokio::time::timeout(Duration::from_secs(4), async {
                    while provider.calls.load(Ordering::SeqCst) < if mode == 1 { 2 } else { 1 } {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .unwrap();
                assert!(!task.is_finished(), "P0/P1 preserve the live supervisor");
                task.abort();
                let _ = task.await;
            } else {
                tokio::time::timeout(Duration::from_secs(5), task)
                    .await
                    .unwrap()
                    .unwrap();
                let mut heartbeats = 0;
                while received.try_recv().is_ok() {
                    heartbeats += 1;
                }
                if mode == 2 {
                    assert!(heartbeats >= 1, "P2 registry progressed during slow probes");
                } else {
                    assert!(
                        heartbeats <= 1,
                        "P3 permits only the concurrent first delivery"
                    );
                }
                assert!(
                    authority.remaining().is_zero(),
                    "P2/P3 terminal authority closure"
                );
                assert_eq!(
                    provider.calls.load(Ordering::SeqCst),
                    if mode == 2 { 2 } else { 1 }
                );
            }
            server.abort();
            let _ = server.await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn environment_timeouts_do_not_extend_evidence_or_overlap_probes() {
        // Cause/effect decision table: T1 valid initial evidence + all pending
        // probes => retry only before the ORIGINAL deadline, then stop; T2 each
        // cancelled request drops before the next probe starts => max flight=1.
        // Exact virtual-time equality detects timeout-anchored freshness bugs.
        let timing =
            awaken_runtime_contract::authority_lease::AuthorityLeaseTiming::from_ttl_ms(600);
        let started = tokio::time::Instant::now();
        let calls = AtomicUsize::new(0);
        let active = AtomicUsize::new(0);
        struct Flight<'a>(&'a AtomicUsize);
        impl Drop for Flight<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        super::environment_evidence::monitor(started, timing, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(active.fetch_add(1, Ordering::SeqCst), 0, "T2 single flight");
            let _flight = Flight(&active);
            std::future::pending::<Result<(), &'static str>>().await
        })
        .await;
        assert_eq!(
            started.elapsed(),
            timing.proof_window(),
            "T1 original deadline"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2, "T1 bounded retry");
        assert_eq!(
            active.load(Ordering::SeqCst),
            0,
            "T2 cancelled probe dropped"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn already_expired_environment_evidence_cannot_be_revived() {
        // T3 an executor resumes after the proof deadline, with a healthy
        // provider available => irreversible expiry, never start a new probe.
        let timing =
            awaken_runtime_contract::authority_lease::AuthorityLeaseTiming::from_ttl_ms(600);
        let calls = AtomicUsize::new(0);
        super::environment_evidence::monitor(
            tokio::time::Instant::now() - timing.proof_window(),
            timing,
            || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok::<(), &'static str>(())
            },
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 0, "T3");
    }

    #[tokio::test(start_paused = true)]
    async fn slow_success_uses_request_start_not_response_time_for_evidence() {
        // T4 timely success with nonzero latency + subsequent unknown probes
        // => expiry anchored at the successful REQUEST start, not its response.
        // T5 explicit error is independently covered by the real Worker test P3.
        let timing =
            awaken_runtime_contract::authority_lease::AuthorityLeaseTiming::from_ttl_ms(600);
        let started = tokio::time::Instant::now();
        let calls = AtomicUsize::new(0);
        super::environment_evidence::monitor(started, timing, || async {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                Ok(())
            } else {
                std::future::pending::<Result<(), &'static str>>().await
            }
        })
        .await;
        assert_eq!(
            started.elapsed(),
            timing.renew_interval() + timing.proof_window(),
            "T4"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn effectful_reconciliation_outlives_poll_budget_and_remains_single_flight() {
        const PERIOD: std::time::Duration = std::time::Duration::from_secs(10);

        let renewal_initial = Arc::new(tokio::sync::Notify::new());
        let renewal_during_cold = Arc::new(tokio::sync::Notify::new());
        let renewal_slow_started = Arc::new(tokio::sync::Notify::new());
        let renewal_slow_release = Arc::new(tokio::sync::Notify::new());
        let renewal_caught_up = Arc::new(tokio::sync::Notify::new());
        let cold_started = Arc::new(tokio::sync::Notify::new());
        let cold_release = Arc::new(tokio::sync::Notify::new());
        let cold_caught_up = Arc::new(tokio::sync::Notify::new());
        let renewal_calls = Arc::new(AtomicUsize::new(0));
        let cold_calls = Arc::new(AtomicUsize::new(0));
        let renewal_completed = Arc::new(AtomicBool::new(false));
        let cold_completed = Arc::new(AtomicBool::new(false));

        // Cause/effect graph: C1 cold recovery remains pending across several
        // periods; C2 renewal becomes due while C1 is pending; C3 that renewal
        // also remains pending across several periods; C4 both effects reach
        // their durable boundary with several ticks overdue. Effects: E1 the
        // independent renewal lane still starts while cold recovery is pending;
        // E2 neither lane overlaps or cancels its pending effect; E3 each lane
        // executes exactly one overdue tick after completion instead of a Burst
        // replay; E4 no successor starts before its predecessor completes.
        //
        // | Rule | cold pending | renewal due | renewal pending | released | Effect |
        // |---|---|---|---|---|---|
        // | R1 | yes | yes | no | no | E1 |
        // | R2 | yes | yes | yes | no | E2 + E4 |
        // | R3 | yes | yes | yes | yes | E3 + E4 |
        //
        // Error reporting belongs to the production closures; this test owns
        // only the shared supervisor cadence and adds no second scheduler.
        let task = tokio::spawn(supervise_session_realization_reconciliation(
            PERIOD,
            PERIOD,
            {
                let initial = renewal_initial.clone();
                let during_cold = renewal_during_cold.clone();
                let slow_started = renewal_slow_started.clone();
                let slow_release = renewal_slow_release.clone();
                let caught_up = renewal_caught_up.clone();
                let calls = renewal_calls.clone();
                let completed = renewal_completed.clone();
                move || {
                    let initial = initial.clone();
                    let during_cold = during_cold.clone();
                    let slow_started = slow_started.clone();
                    let slow_release = slow_release.clone();
                    let caught_up = caught_up.clone();
                    let calls = calls.clone();
                    let completed = completed.clone();
                    async move {
                        match calls.fetch_add(1, Ordering::SeqCst) + 1 {
                            1 => initial.notify_one(),
                            2 => during_cold.notify_one(),
                            3 => {
                                slow_started.notify_one();
                                slow_release.notified().await;
                                completed.store(true, Ordering::SeqCst);
                            }
                            4 => caught_up.notify_one(),
                            _ => {}
                        }
                    }
                }
            },
            {
                let started = cold_started.clone();
                let release = cold_release.clone();
                let caught_up = cold_caught_up.clone();
                let calls = cold_calls.clone();
                let completed = cold_completed.clone();
                move || {
                    let started = started.clone();
                    let release = release.clone();
                    let caught_up = caught_up.clone();
                    let calls = calls.clone();
                    let completed = completed.clone();
                    async move {
                        match calls.fetch_add(1, Ordering::SeqCst) + 1 {
                            1 => {
                                started.notify_one();
                                release.notified().await;
                                completed.store(true, Ordering::SeqCst);
                            }
                            2 => caught_up.notify_one(),
                            _ => {}
                        }
                    }
                }
            },
        ));

        renewal_initial.notified().await;
        tokio::time::advance(PERIOD).await;
        cold_started.notified().await;
        renewal_during_cold.notified().await;
        assert_eq!(renewal_calls.load(Ordering::SeqCst), 2, "R1/E1");
        assert_eq!(cold_calls.load(Ordering::SeqCst), 1, "R1");

        tokio::time::advance(PERIOD).await;
        renewal_slow_started.notified().await;
        tokio::time::advance(PERIOD * 2 + PERIOD / 2).await;
        assert_eq!(renewal_calls.load(Ordering::SeqCst), 3, "R2/E2");
        assert_eq!(cold_calls.load(Ordering::SeqCst), 1, "R2/E2");
        assert!(!renewal_completed.load(Ordering::SeqCst), "R2/E4");
        assert!(!cold_completed.load(Ordering::SeqCst), "R2/E4");

        renewal_slow_release.notify_one();
        cold_release.notify_one();
        renewal_caught_up.notified().await;
        cold_caught_up.notified().await;
        assert!(renewal_completed.load(Ordering::SeqCst), "R3/E4");
        assert!(cold_completed.load(Ordering::SeqCst), "R3/E4");
        assert_eq!(renewal_calls.load(Ordering::SeqCst), 4, "R3/E3");
        assert_eq!(cold_calls.load(Ordering::SeqCst), 2, "R3/E3");
        task.abort();
    }
}
