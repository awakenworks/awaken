// @formal entrypoint session_realization.concurrent_phase_drivers_are_serialized_per_session
// @formal entrypoint worker_authority.independent_evidence_never_extends_expired_proof
// @formal entrypoint worker_credential.heartbeat_observations_are_one_atomic_batch
// @formal entrypoint worker_credential.publication_budget_covers_probe_and_delivery
// @formal entrypoint worker_drain.bounded_cleanup_eventually_exits_without_fabricating_quiescence
// @formal entrypoint worker_drain.closed_generation_is_absorbing
// @formal entrypoint worker_drain.grace_expiry_retains_cancellation_observer
// @formal entrypoint worker_drain.no_claim_after_acknowledgement
// @formal entrypoint worker_drain.no_claim_attempt_after_registry_drain
// @formal entrypoint worker_drain.registry_transition_follows_local_fence
//! Worker registry authority, heartbeat, and local admission lifecycle.

use awaken_sync::LeafRwLock;
use awaken_time::mono::{self, Instant};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use awaken_coordinator_client::WorkerControlClient;
use awaken_lease_contract::authority::{
    AuthorityEnd, AuthorityLeaseTiming, AuthorityProofStarted, RenewalDecision, RenewalReply,
};
use awaken_session_executor::WorkerRoles;
use awaken_worker_contract::{
    RegistryMutation, WorkerHeartbeat, WorkerIdentity, WorkerSourceRetention,
};
use futures_util::FutureExt as _;

use crate::credential_liveness::WorkerObservationCache;

mod environment_evidence;

pub(crate) use environment_evidence::proof_is_fresh as environment_proof_is_fresh;
mod authority;
pub(crate) use authority::WorkerAuthorityProof;
pub(crate) use authority::authority_proof_started;

const SESSION_REALIZATION_LEASE_TTL_MS: u64 = 20_000;
// Session roots queue ordinary terminal release as well as restart recovery.
// Keep that work prompt in the one single-flight reconciliation lane.
const SESSION_TERMINAL_RECONCILIATION_PERIOD: std::time::Duration =
    std::time::Duration::from_secs(1);
pub(crate) const WORKER_REVOCATION_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);

/// Keep a cold cleanup claim inside the already-observed Worker Registry lease.
/// Heartbeats renew at one third of that lease, so half leaves a full renewal
/// interval of slack even when the cleanup cadence runs just before heartbeat.
fn cold_cleanup_lease_ttl_ms(registry_timing: AuthorityLeaseTiming) -> u64 {
    let registry_ttl_ms =
        u64::try_from(registry_timing.lease_ttl().as_millis()).unwrap_or(u64::MAX);
    SESSION_REALIZATION_LEASE_TTL_MS
        .min(registry_ttl_ms / 2)
        .max(1)
}

#[derive(Clone)]
enum WarmCapacityDemand {
    Environment(Box<awaken_session_configuration_contract::EnvironmentSnapshot>),
    Default,
}

/// Cache retirement requires a fresh complete demand observation. The physical
/// pool's fallback receipts can omit retained Environments and are not that
/// observation; a failed Control fetch must never erase their image holders.
async fn prepare_current_environment_images<'a, E, F>(
    fetched: &'a Result<Vec<awaken_session_configuration_contract::EnvironmentSnapshot>, String>,
    prepare: impl FnOnce(&'a [awaken_session_configuration_contract::EnvironmentSnapshot]) -> F,
) -> Result<(), E>
where
    F: std::future::Future<Output = Result<(), E>>,
{
    match fetched {
        Ok(desired) => prepare(desired).await,
        Err(_) => Ok(()),
    }
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
        roles: &WorkerRoles,
        desired: Vec<awaken_session_configuration_contract::EnvironmentSnapshot>,
    ) -> Self {
        let (per_shape, total) = roles.environment_capacity().environment_warmup_limits();
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
            let shape = roles
                .environment_capacity()
                .environment_snapshot_capacity_shape(&snapshot);
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
            let shape = roles
                .environment_capacity()
                .default_environment_capacity_shape();
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerRevocationStage {
    LocalFenceAndInterrupt,
    RuntimeReturn,
    SessionRetirement,
    EnvironmentCapacity,
}

/// Result of the bounded local revocation observer. `Unconfirmed` deliberately
/// carries no quiescence authority: timeout, cancellation delivery, and cleanup
/// failure cannot manufacture an executor-return receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkerRevocationOutcome {
    Completed {
        interrupted: usize,
    },
    Unconfirmed {
        stage: WorkerRevocationStage,
        interrupted: Option<usize>,
        detail: Option<String>,
    },
}

impl WorkerRevocationOutcome {
    pub(crate) fn permits_quiescence_publication(&self, in_flight: u32) -> bool {
        matches!(self, Self::Completed { .. }) && in_flight == 0
    }
}

#[derive(Debug)]
pub(crate) struct WorkerRevocationReport {
    pub(crate) outcome: WorkerRevocationOutcome,
    pub(crate) deadline: tokio::time::Instant,
}

#[derive(Clone)]
pub(crate) struct WorkerSupervisor {
    pub(crate) authority: Arc<WorkerAuthorityProof>,
    pub(crate) roles: WorkerRoles,
    pub(crate) control: WorkerControlClient,
    pub(crate) identity: WorkerIdentity,
    pub(crate) credential_observation_resolver:
        Option<Arc<dyn awaken_credential_contract::WorkerLocalCredentialResolver>>,
    pub(crate) acp_capability_observation_source:
        Option<Arc<dyn awaken_acp_contract::AcpCapabilityObservationSource>>,
    pub(crate) session_environment_provider: Option<
        Arc<
            dyn awaken_provisioning_contract::SandboxProvider<
                    Environment = dyn awaken_provisioning_contract::SessionSandbox,
                >,
        >,
    >,
    /// A local tier's backend probe; present only without a container provider.
    pub(crate) local_readiness: Option<LocalReadinessProbe>,
    pub(crate) observations: Arc<WorkerObservationCache>,
    pub(crate) observation_ttl: std::time::Duration,
    pub(crate) probe_interval: std::time::Duration,
    pub(crate) warm_environments: Arc<
        LeafRwLock<
            BTreeMap<
                awaken_provisioning_contract::SandboxCapacityShapeId,
                awaken_session_configuration_contract::EnvironmentSnapshot,
            >,
        >,
    >,
    /// The original-source retention this draining incarnation last observed;
    /// every heartbeat reports it whole.
    pub(crate) source_retention: Arc<LeafRwLock<BTreeSet<WorkerSourceRetention>>>,
}

/// One fresh readiness observation of a local tier's backend.
pub(crate) type LocalReadinessProbe = Arc<
    dyn Fn() -> futures_util::future::BoxFuture<
            'static,
            Result<(), awaken_provisioning_contract::SandboxError>,
        > + Send
        + Sync,
>;

impl WorkerSupervisor {
    /// Whether an installed Session backend proves its readiness before Ready
    /// and on every heartbeat: the container provider, or a local tier's probe.
    pub(crate) fn environment_readiness_installed(&self) -> bool {
        self.session_environment_provider.is_some() || self.local_readiness.is_some()
    }

    /// The first environment proof, required before Ready.
    pub(crate) async fn prove_environment_before_ready(
        &self,
        timeout: std::time::Duration,
    ) -> std::io::Result<()> {
        if !self.environment_readiness_installed() {
            return Ok(());
        }
        tokio::time::timeout(timeout, self.probe_environment())
            .await
            .map_err(|_| {
                std::io::Error::other("sandbox readiness evidence timed out before Ready")
            })?
            .map_err(|error| {
                std::io::Error::other(format!(
                    "sandbox readiness evidence failed before Ready: {error}"
                ))
            })
    }

    pub(crate) async fn probe_environment(
        &self,
    ) -> Result<(), awaken_provisioning_contract::SandboxError> {
        match (&self.session_environment_provider, &self.local_readiness) {
            (Some(provider), _) => provider.probe_ready().await,
            (None, Some(local)) => local().await,
            (None, None) => Ok(()),
        }
    }

    async fn publish_registry_drain(
        &self,
        _local_fence: awaken_dispatch_execution::PoolDrainFence,
        deadline_ms: Option<u64>,
    ) -> Result<(), String> {
        match self
            .control
            .begin_drain(&self.identity, deadline_ms)
            .await?
        {
            RegistryMutation::Applied => Ok(()),
            other => Err(format!("worker drain rejected: {other:?}")),
        }
    }

    pub(crate) async fn begin_drain(&self, deadline_ms: Option<u64>) -> Result<(), String> {
        // The process-local claim gate is the drain linearization point. Fence it
        // before publishing Draining to the Registry so no still-open claim loop
        // can race the remote state transition and receive a predictable rejection.
        let local_fence = self.roles.dispatch().begin_drain().await.ok_or_else(|| {
            "worker drain rejected: local claim pool is not installed".to_string()
        })?;
        self.publish_registry_drain(local_fence, deadline_ms).await
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
                self.roles.clock().as_ref(),
                self.observation_ttl,
            )
            .await
    }

    pub(crate) fn warm_environment_shapes(
        &self,
    ) -> BTreeSet<awaken_provisioning_contract::SandboxCapacityShapeId> {
        self.warm_environments
            .read()
            .iter()
            .filter(|(_, snapshot)| {
                self.roles
                    .environment_capacity()
                    .ready_environment_snapshot_capacity(snapshot)
                    > 0
            })
            .map(|(shape, _)| shape.clone())
            .collect()
    }

    /// The heartbeat this incarnation sends: one frozen observation batch,
    /// its admitted executions, warm capacity and the source retention it
    /// reports.
    pub(crate) fn heartbeat(&self, sequence: u64, ready: bool) -> WorkerHeartbeat {
        let mut heartbeat = self.observations.snapshot().into_heartbeat(
            sequence,
            ready,
            self.roles.dispatch().in_flight(),
            self.warm_environment_shapes(),
        );
        heartbeat.source_retention = self.source_retention();
        heartbeat
    }

    /// The retention facts the next heartbeat reports.
    pub(crate) fn source_retention(&self) -> BTreeSet<WorkerSourceRetention> {
        self.source_retention.read().clone()
    }

    /// Observe the original Session sources this incarnation holds and keep
    /// the facts its heartbeat reports to the Coordinator, which decides
    /// retirement from them.
    ///
    /// Decision table (rows of [`source_retention_fact`] per subject):
    /// | Rule | Claim pool | Subject read | Observation | Report |
    /// |---|---|---|---|---|
    /// | W1 | can resume work (not draining) | - | - | empty: no retirement is being qualified |
    /// | W2 | draining | fails | - | the previous report stays; the Coordinator refuses any fact whose subject changed |
    /// | W3 | draining | succeeds | retained | retained fact for the subject's digest |
    /// | W4 | draining | succeeds | not retained | not-retained fact |
    /// | W5 | draining | succeeds | fails | not-retained fact: a failed observation stays closed |
    pub(crate) async fn reconcile_source_retention(&self) -> Result<usize, String> {
        let report = if self.roles.dispatch().can_resume_work().await {
            BTreeSet::new()
        } else {
            let subjects = self
                .control
                .source_retention_subjects(&self.identity)
                .await?;
            let mut report = BTreeSet::new();
            for subject in subjects {
                let observed = self
                    .roles
                    .observe_source_retention(
                        &subject.session_id,
                        &subject.projection,
                        subject.source_prepared,
                    )
                    .await;
                report.insert(source_retention_fact(
                    subject.session_id.as_str(),
                    &subject.digest,
                    observed,
                ));
            }
            report
        };
        let facts = report.len();
        *self.source_retention.write() = report;
        Ok(facts)
    }

    /// Reconcile Coordinator-derived Environment demand and the default Session
    /// demand through one global plan. A failed pull reuses the last successful
    /// Environment set, so it neither erases receipts nor bypasses the default
    /// share of the same budget.
    pub(crate) async fn reconcile_environment_warmups(&self) -> Result<usize, String> {
        let previous = self.warm_environments.read().clone();
        let fetched = self
            .control
            .current_environment_warmups(&self.identity)
            .await;
        if let Err(error) = prepare_current_environment_images(&fetched, |desired| {
            self.roles
                .environment_capacity()
                .prewarm_environment_images(desired)
        })
        .await
        {
            eprintln!("Environment image prewarm failed; cold path retained: {error}");
        }
        let (desired, fetch_error) = match fetched {
            Ok(desired) => (desired, None),
            Err(error) => (previous.values().cloned().collect(), Some(error)),
        };
        let plan = WarmCapacityPlan::build(&self.roles, desired);
        let selected_shapes = plan.shapes();

        // Release every plan-external shape before creating replacements. Both
        // default and Environment capacity are compared with the same typed id,
        // so an identical projection cannot discard its own selected capacity.
        for (shape, snapshot) in &previous {
            if !selected_shapes.contains(shape) {
                self.roles
                    .environment_capacity()
                    .discard_environment_snapshot_capacity(snapshot)
                    .await;
            }
        }
        let default_shape = self
            .roles
            .environment_capacity()
            .default_environment_capacity_shape();
        if !selected_shapes.contains(&default_shape) {
            self.roles
                .environment_capacity()
                .discard_default_environment_capacity()
                .await;
        }
        let mut next = previous
            .into_iter()
            .filter(|(shape, _)| selected_shapes.contains(shape))
            .collect::<BTreeMap<_, _>>();
        for target in &plan.targets {
            let result = match &target.demand {
                WarmCapacityDemand::Environment(snapshot) => {
                    self.roles
                        .environment_capacity()
                        .prewarm_environment_snapshot(snapshot, target.target)
                        .await
                }
                WarmCapacityDemand::Default => {
                    self.roles
                        .environment_capacity()
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
        *self.warm_environments.write() = next;
        let ready = self.warm_environment_shapes().len();
        if let Some(error) = fetch_error {
            Err(error)
        } else {
            Ok(ready)
        }
    }
}

pub(crate) fn select_incarnation_id(preselected: Option<String>) -> Result<String, std::io::Error> {
    if let Some(incarnation_id) = preselected {
        return Ok(incarnation_id);
    }
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| std::io::Error::other(format!("generate Worker incarnation: {error}")))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub(crate) fn spawn_heartbeat(
    lifecycle: Arc<WorkerSupervisor>,
    sequence: u64,
    timing: AuthorityLeaseTiming,
    environment_proof_started: awaken_time::mono::Instant,
) -> tokio::task::JoinHandle<WorkerRevocationReport> {
    tokio::spawn(async move {
        // Independent, single-flight lanes under the same incarnation owner.
        // Neither a slow security probe nor a registry retry can extend the
        // other's proof. Losing either proof irreversibly drains both lanes.
        supervise_worker_authority(
            async {
                tokio::select! {
                    biased;
                    () = async {
                        if lifecycle.environment_readiness_installed() {
                            environment_evidence::monitor(
                                environment_proof_started,
                                timing,
                                || lifecycle.probe_environment(),
                            ).await;
                        } else {
                            std::future::pending::<()>().await;
                        }
                    } => {}
                    () = renew_registry_authority(&lifecycle, sequence).boxed() => {}
                }
            }
            .boxed(),
            || async { revoke_worker_session_authority(&lifecycle).await }.boxed(),
        )
        .await
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
) -> Revocation::Output
where
    Revoke: FnOnce() -> Revocation,
    Revocation: std::future::Future,
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
    revoke().await
}

/// The first applied registry renewal of a registered incarnation.
pub(crate) struct InitialRegistryProof {
    /// Request start of the applied heartbeat: the first proof's anchor.
    pub(crate) started: AuthorityProofStarted,
    pub(crate) lease_ttl_ms: u64,
    /// The sequence the regular heartbeat continues from.
    pub(crate) next_sequence: u64,
}

/// Prove the first registry authority inside the registration receipt's own
/// proof window. Registration already leased this incarnation, so the shared
/// renewal law applies: each attempt may run for the remaining registration
/// proof, an unanswered attempt is retried while that proof remains, and only
/// an exhausted proof or an explicit refusal ends startup. Every attempt
/// carries a fresh sequence, so a retry after a lost reply is never stale.
///
/// | Rule | Reply | Registration proof left | Effect |
/// | I1 | Applied (also slow, after the nominal request bound) | > 0 | proof installed |
/// | I2 | transport error or no reply | > 0 | retry, not exit |
/// | I3 | no reply or Applied | 0 | exit: registration proof expired |
/// | I4 | refused mutation | any | exit: heartbeat rejected |
pub(crate) async fn prove_initial_registry_authority<Clock, Send, Reply>(
    registration_started: AuthorityProofStarted,
    registration_timing: AuthorityLeaseTiming,
    mut clock: Clock,
    mut send: Send,
) -> Result<InitialRegistryProof, String>
where
    Clock: FnMut() -> std::io::Result<AuthorityProofStarted>,
    Send: FnMut(u64) -> Reply,
    Reply: std::future::Future<Output = Result<(RegistryMutation, Option<u64>), String>>,
{
    let mut remaining_at = move || {
        clock()
            .map(|now| {
                (
                    now,
                    registration_timing
                        .remaining_proof_at(registration_started, now)
                        .unwrap_or_default(),
                )
            })
            .map_err(|error| error.to_string())
    };
    let mut sequence = 1_u64;
    loop {
        let (attempt, remaining) = remaining_at()?;
        let Some(budget) = registration_timing.renewal_request_budget(remaining) else {
            return Err(
                "initial worker heartbeat timed out: the registration proof expired before an attempt"
                    .to_owned(),
            );
        };
        let reply = tokio::time::timeout(budget, send(sequence)).await;
        let attempted = sequence;
        sequence = sequence
            .checked_add(1)
            .ok_or_else(|| "worker heartbeat sequence exhausted".to_owned())?;
        let (observed, detail, applied_ttl) = match reply {
            Ok(Ok((RegistryMutation::Applied, ttl))) => (RenewalReply::Applied, String::new(), ttl),
            Ok(Ok((mutation, _))) => (RenewalReply::Refused, format!("{mutation:?}"), None),
            Ok(Err(error)) => (RenewalReply::Unanswered, error, None),
            Err(_) => (
                RenewalReply::Unanswered,
                "no reply within the registration proof".to_owned(),
                None,
            ),
        };
        let (_, remaining) = remaining_at()?;
        match registration_timing.decide_renewal(remaining, observed) {
            RenewalDecision::Apply => {
                let lease_ttl_ms = applied_ttl.filter(|ttl| *ttl > 0).ok_or_else(|| {
                    "applied initial worker heartbeat omitted a positive lease TTL".to_owned()
                })?;
                return Ok(InitialRegistryProof {
                    started: attempt,
                    lease_ttl_ms,
                    next_sequence: sequence,
                });
            }
            RenewalDecision::RetryAfter(delay) => {
                tracing::warn!(
                    sequence = attempted,
                    proof_remaining_ms = remaining.as_millis(),
                    error = %detail,
                    "initial worker heartbeat unanswered; retrying within the registration proof"
                );
                tokio::time::sleep(delay).await;
            }
            RenewalDecision::Stop(AuthorityEnd::AuthorityLost) => {
                return Err(format!("initial worker heartbeat rejected: {detail}"));
            }
            RenewalDecision::Stop(AuthorityEnd::ProofExpired) => {
                return Err(format!(
                    "initial worker heartbeat timed out: the registration proof expired ({detail})"
                ));
            }
        }
    }
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
        tokio::time::sleep_until(next_attempt).await;
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
        let ready = lifecycle.roles.dispatch().can_resume_work().await;
        // The request may run for the whole remaining proof: a reply inside it
        // still renews this incarnation. Past the nominal request bound it is
        // slow, not lost, so claim admission pauses while it is awaited.
        let Some(heartbeat_budget) = timing.renewal_request_budget(lifecycle.authority.remaining())
        else {
            awaken_observability::record_worker_authority_loss("proof_expired");
            tracing::error!(
                sequence,
                consecutive_failures,
                "worker registry authority proof expired; draining locally"
            );
            break;
        };
        let heartbeat = lifecycle.heartbeat(sequence, ready);
        let request = lifecycle.control.heartbeat(&lifecycle.identity, heartbeat);
        tokio::pin!(request);
        let nominal = timing.request_timeout().min(heartbeat_budget);
        let mutation = match tokio::time::timeout(nominal, &mut request).await {
            Err(_) if nominal < heartbeat_budget => {
                suspend_authority_admission(lifecycle, &mut admission_suspended).await;
                tracing::warn!(
                    sequence,
                    proof_remaining_ms = lifecycle.authority.remaining().as_millis(),
                    "worker heartbeat reply is slow; claim admission paused while it can still apply"
                );
                tokio::time::timeout(heartbeat_budget - nominal, &mut request).await
            }
            reply => reply,
        };
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
                if let Err(error) = awaken_worker_contract::validate_observation_timing(
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
                    if lifecycle.roles.dispatch().resume_admission().await {
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
                    mono::now().duration_since(attempt_started),
                    scheduling_lag,
                    lifecycle.authority.remaining(),
                );
            }
            Ok(Ok(receipt)) => {
                awaken_observability::record_worker_authority_heartbeat(
                    "rejected",
                    mono::now().duration_since(attempt_started),
                    scheduling_lag,
                    lifecycle.authority.remaining(),
                );
                awaken_observability::record_worker_authority_loss("registry_rejected");
                tracing::error!(mutation = ?receipt.mutation, "worker heartbeat explicitly lost authority; draining locally");
                break;
            }
            Ok(Err(error)) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                let now = mono::now();
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
                let now = mono::now();
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

// A slow or unavailable response is not evidence of authority loss. Reuse the
// same temporary admission gate for slow replies, Registry transport errors and
// unanswered requests. Neither a retry nor a successful probe advances the last
// durable Registry proof.
async fn suspend_authority_admission(lifecycle: &WorkerSupervisor, admission_suspended: &mut bool) {
    if !*admission_suspended {
        lifecycle.roles.dispatch().suspend_admission().await;
        *admission_suspended = true;
    }
}

async fn pause_authority_admission_for_retry(
    lifecycle: &WorkerSupervisor,
    admission_suspended: &mut bool,
    timing: AuthorityLeaseTiming,
) -> Option<Instant> {
    suspend_authority_admission(lifecycle, admission_suspended).await;
    let now = mono::now();
    match timing.decide_renewal(lifecycle.authority.remaining(), RenewalReply::Unanswered) {
        RenewalDecision::RetryAfter(delay) => Some(now + delay),
        RenewalDecision::Apply | RenewalDecision::Stop(_) => {
            // Proof exhausted: stop and abandon local work. This is authority
            // loss, never a durable cancellation of the claimed Runs.
            awaken_observability::record_worker_authority_loss("proof_expired");
            None
        }
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
        let mut interval = tokio::time::interval_at(mono::now() + cold_period, cold_period);
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

fn session_realization_claim_target(
    lifecycle: &WorkerSupervisor,
    lease_ttl_ms: u64,
) -> Option<awaken_session_execution_contract::SessionRealizationTarget> {
    let now_at = lifecycle.roles.clock().now();
    let expiry = awaken_time::Expiry::child_within(
        awaken_time::Expiry::after(now_at, lifecycle.authority.remaining()),
        now_at,
        std::time::Duration::from_millis(lease_ttl_ms),
    )?;
    Some(
        awaken_session_execution_contract::SessionRealizationTarget {
            owner: lifecycle.identity.worker_id.clone(),
            runtime_incarnation: lifecycle.identity.lease_owner(),
            lease_expires_at_unix_ms: expiry.to_epoch_ms(),
            reassign_existing_lease: false,
        },
    )
}

/// Renew resident Session leases promptly while an independent cold lane
/// recovers terminal roots. Control authenticates the current Worker registry
/// lease on every cold claim; the heartbeat task remains the sole
/// registry-liveness owner. The cold Host/contract cleanup driver is
/// single-flight, while unrelated lease renewal stays schedulable during its
/// durable I/O.
pub(crate) fn spawn_session_realization_reconciliation(
    lifecycle: Arc<WorkerSupervisor>,
    registry_timing: AuthorityLeaseTiming,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let timing = AuthorityLeaseTiming::from_ttl_ms(SESSION_REALIZATION_LEASE_TTL_MS);
        let cold_lease_ttl_ms = cold_cleanup_lease_ttl_ms(registry_timing);
        let renewal_lifecycle = lifecycle.clone();
        let cold_lifecycle = lifecycle;
        supervise_session_realization_reconciliation(
            std::time::Duration::from_secs(1),
            SESSION_TERMINAL_RECONCILIATION_PERIOD,
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
                        .roles
                        .run_assembly().renew_due_session_realizations(timing)
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
                    // The cold claim is a child grant of the observed Worker
                    // Registry authority: it never outlives it, and a lapsed
                    // registry proof mints no claim this cycle.
                    let Some(cleanup_target) =
                        session_realization_claim_target(&lifecycle, cold_lease_ttl_ms)
                    else {
                        return;
                    };
                    if let Err(error) = lifecycle
                        .roles
                        .run_assembly().recover_terminal_cleanup_assignments(cleanup_target)
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

/// The complete Session root is the only queue for Run-free preparation. The
/// Worker claims one obligation at a time over its registered Control channel
/// and drives the existing phase protocol without manufacturing a model Run.
pub(crate) fn spawn_session_preparation_reconciliation(
    lifecycle: Arc<WorkerSupervisor>,
    registry_timing: AuthorityLeaseTiming,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            // The same absorbing pool drain that stops Run claims also stops
            // new preparation claims. Temporary heartbeat suspension skips a
            // tick but can resume; it never turns into terminal drain.
            if !lifecycle.roles.dispatch().can_resume_work().await {
                break;
            }
            if !lifecycle.roles.dispatch().accepting_work().await {
                continue;
            }
            let Some(target) = session_realization_claim_target(
                &lifecycle,
                cold_cleanup_lease_ttl_ms(registry_timing),
            ) else {
                continue;
            };
            match lifecycle
                .control
                .claim_next_session_preparation(&lifecycle.identity, target)
                .await
            {
                Ok(Some(assignment)) => {
                    if let Err(error) = lifecycle
                        .roles
                        .run_assembly()
                        .prepare_session_without_run(&assignment)
                        .await
                    {
                        eprintln!(
                            "Worker Session preparation remained pending for {}: {error}",
                            assignment.session_id
                        );
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    eprintln!("Worker Session preparation claim remained pending: {error}");
                }
            }
        }
    })
}

/// One reported fact for one observed subject: only a positive observation
/// is retention.
pub(crate) fn source_retention_fact<E>(
    session_id: &str,
    digest: &str,
    observed: Result<bool, E>,
) -> WorkerSourceRetention {
    WorkerSourceRetention {
        session_id: session_id.to_owned(),
        subject: digest.to_owned(),
        retained: matches!(observed, Ok(true)),
    }
}

/// Keep the retention report current while this incarnation drains.
pub(crate) fn spawn_source_retention_reconciliation(
    lifecycle: Arc<WorkerSupervisor>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(error) = lifecycle.reconcile_source_retention().await {
                eprintln!(
                    "Worker source retention report not refreshed; the previous report stays: {error}"
                );
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

async fn complete_worker_revocation_before<Fence, Observe, ObserveFuture, Retire>(
    deadline: tokio::time::Instant,
    fence_and_interrupt: Fence,
    observe_return: Observe,
    retire_realizations: Retire,
) -> WorkerRevocationOutcome
where
    Fence: std::future::Future<Output = Result<usize, String>>,
    Observe: FnOnce(std::time::Duration) -> ObserveFuture,
    ObserveFuture: std::future::Future<Output = bool>,
    Retire: std::future::Future<Output = Result<(), String>>,
{
    let interrupted = match tokio::time::timeout_at(deadline, fence_and_interrupt).await {
        Ok(Ok(interrupted)) => interrupted,
        Ok(Err(detail)) => {
            return WorkerRevocationOutcome::Unconfirmed {
                stage: WorkerRevocationStage::LocalFenceAndInterrupt,
                interrupted: None,
                detail: Some(detail),
            };
        }
        Err(_) => {
            return WorkerRevocationOutcome::Unconfirmed {
                stage: WorkerRevocationStage::LocalFenceAndInterrupt,
                interrupted: None,
                detail: None,
            };
        }
    };

    let remaining = deadline.saturating_duration_since(mono::now());
    let returned = match tokio::time::timeout_at(deadline, observe_return(remaining)).await {
        Ok(returned) => returned,
        Err(_) => {
            return WorkerRevocationOutcome::Unconfirmed {
                stage: WorkerRevocationStage::RuntimeReturn,
                interrupted: Some(interrupted),
                detail: None,
            };
        }
    };

    let retirement = tokio::time::timeout_at(deadline, retire_realizations).await;
    match (returned, retirement) {
        (true, Ok(Ok(()))) => WorkerRevocationOutcome::Completed { interrupted },
        (false, _) => WorkerRevocationOutcome::Unconfirmed {
            stage: WorkerRevocationStage::RuntimeReturn,
            interrupted: Some(interrupted),
            detail: None,
        },
        (true, Ok(Err(detail))) => WorkerRevocationOutcome::Unconfirmed {
            stage: WorkerRevocationStage::SessionRetirement,
            interrupted: Some(interrupted),
            detail: Some(detail),
        },
        (true, Err(_)) => WorkerRevocationOutcome::Unconfirmed {
            stage: WorkerRevocationStage::SessionRetirement,
            interrupted: Some(interrupted),
            detail: None,
        },
    }
}

async fn complete_worker_capacity_before<Capacity>(
    deadline: tokio::time::Instant,
    revocation: WorkerRevocationOutcome,
    shutdown_capacity: Capacity,
) -> WorkerRevocationOutcome
where
    Capacity: std::future::Future<Output = ()>,
{
    match (
        revocation,
        tokio::time::timeout_at(deadline, shutdown_capacity).await,
    ) {
        (WorkerRevocationOutcome::Completed { interrupted }, Ok(())) => {
            WorkerRevocationOutcome::Completed { interrupted }
        }
        (WorkerRevocationOutcome::Completed { interrupted }, Err(_)) => {
            WorkerRevocationOutcome::Unconfirmed {
                stage: WorkerRevocationStage::EnvironmentCapacity,
                interrupted: Some(interrupted),
                detail: None,
            }
        }
        (unconfirmed, _) => unconfirmed,
    }
}

pub(crate) async fn revoke_worker_session_authority(
    lifecycle: &WorkerSupervisor,
) -> WorkerRevocationReport {
    let deadline = mono::now() + WORKER_REVOCATION_BUDGET;
    WorkerRevocationReport {
        outcome: revoke_worker_session_authority_before(lifecycle, deadline).await,
        deadline,
    }
}

pub(crate) async fn revoke_worker_session_authority_before(
    lifecycle: &WorkerSupervisor,
    deadline: tokio::time::Instant,
) -> WorkerRevocationOutcome {
    // Root guard closes before selecting slots or awaiting any drain/owner lock.
    // A late/new Session cannot install authority outside this selected cohort.
    lifecycle.authority.close();
    awaken_observability::set_worker_authority_proof_remaining(std::time::Duration::ZERO);
    let revocation = lifecycle
        .roles
        .run_assembly()
        .begin_session_realization_revocation();
    // Fail closed without racing terminal sandbox disposal against in-flight
    // native tools. Every revocation await shares one absolute deadline:
    // cancellation and return observation retain the existing owner for the
    // whole budget, while a stalled adapter cannot prevent process exit
    // indefinitely. The report preserves this same deadline for the Worker's
    // following capacity shutdown after background producers are stopped.
    let outcome = complete_worker_revocation_before(
        deadline,
        async {
            let _local_fence = lifecycle
                .roles
                .dispatch()
                .begin_drain()
                .await
                .ok_or_else(|| {
                    "Worker revocation could not install the local claim fence".to_string()
                })?;
            lifecycle
                .roles
                .session_runs()
                .interrupt_all_session_runs()
                .await
                .map_err(|error| error.to_string())
        },
        |remaining| lifecycle.roles.drain(remaining),
        async {
            lifecycle
                .roles
                .run_assembly()
                .finish_session_realization_revocation(
                    revocation.map_err(|error| error.to_string())?,
                )
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        },
    )
    .await;
    if let WorkerRevocationOutcome::Unconfirmed {
        stage,
        interrupted,
        detail,
    } = &outcome
    {
        eprintln!(
            "Worker cancellation cleanup remained unconfirmed at {stage:?} after interrupting {} local Sessions{}; unconfirmed physical attempts remain fenced",
            interrupted.map_or_else(
                || "an unknown number of".to_string(),
                |count| count.to_string()
            ),
            detail
                .as_deref()
                .map_or_else(String::new, |detail| format!(": {detail}")),
        );
    }
    outcome
}

pub(crate) async fn shutdown_worker_capacity_before(
    lifecycle: &WorkerSupervisor,
    deadline: tokio::time::Instant,
    revocation: WorkerRevocationOutcome,
) -> WorkerRevocationOutcome {
    complete_worker_capacity_before(
        deadline,
        revocation,
        lifecycle
            .roles
            .environment_capacity()
            .shutdown_environment_capacity(),
    )
    .await
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

    use super::{
        WorkerRevocationOutcome, WorkerRevocationStage, cold_cleanup_lease_ttl_ms,
        complete_worker_capacity_before, complete_worker_revocation_before,
        supervise_session_realization_reconciliation, supervise_worker_authority,
    };

    type ScriptedReply = (
        u64,
        Result<(awaken_worker_contract::RegistryMutation, Option<u64>), String>,
    );

    /// Drive the initial heartbeat against scripted replies under paused time.
    /// The registration request started at `base`; the authority clock is the
    /// paused scheduler clock, so elapsed time is exact.
    async fn initial_heartbeat_with(
        registration_age_ms: u64,
        script: Vec<ScriptedReply>,
    ) -> (
        Result<super::InitialRegistryProof, String>,
        Vec<u64>,
        std::time::Duration,
    ) {
        use awaken_lease_contract::authority::{AuthorityLeaseTiming, AuthorityProofStarted};
        use std::time::Duration;
        let base = awaken_time::mono::now();
        let registered = AuthorityProofStarted::from_clock_sample(base, Duration::ZERO);
        tokio::time::advance(Duration::from_millis(registration_age_ms)).await;
        let script = Arc::new(std::sync::Mutex::new(
            script
                .into_iter()
                .collect::<std::collections::VecDeque<_>>(),
        ));
        let sequences = Arc::new(std::sync::Mutex::new(Vec::new()));
        let result = super::prove_initial_registry_authority(
            registered,
            // TTL 3s: proof window 2s, nominal request bound 500ms, retry 100ms.
            AuthorityLeaseTiming::from_ttl_ms(3_000),
            move || {
                let now = awaken_time::mono::now();
                Ok(AuthorityProofStarted::from_clock_sample(now, now - base))
            },
            |sequence| {
                sequences.lock().unwrap().push(sequence);
                let (delay_ms, reply) = script
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or((u64::MAX, Err("unscripted".to_owned())));
                async move {
                    if delay_ms == u64::MAX {
                        std::future::pending::<()>().await;
                    }
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    reply
                }
            },
        )
        .await;
        let elapsed = awaken_time::mono::now() - base;
        let sequences = sequences.lock().unwrap().clone();
        (result, sequences, elapsed)
    }

    #[tokio::test(start_paused = true)]
    async fn initial_heartbeat_retries_inside_the_registration_proof() {
        // Rows of `prove_initial_registry_authority` (TTL 3s, proof 2s, nominal
        // request bound 500ms, retry delay 100ms):
        // | Rule | registration age | replies | effect |
        // | I1 | 0 | Applied after 1.2s (slow) | proof anchored at 0, next seq 2 |
        // | I2 | 0 | 503, then Applied after 700ms | retry at 100ms; anchored at 100ms |
        // | I2b | 0 | no reply | one attempt bounded by the 2s proof, then exit |
        // | I3 | 2s | - | exit before any attempt: proof expired |
        // | I3b | 1.99s | Applied after 10ms (at the deadline) | exit: proof expired |
        // | I4 | 0 | StaleIncarnation | exit: rejected, no retry |
        use awaken_worker_contract::RegistryMutation::{Applied, StaleIncarnation};
        use std::time::Duration;

        let (result, sequences, _) =
            initial_heartbeat_with(0, vec![(1_200, Ok((Applied, Some(3_000))))]).await;
        let proof = result.expect("I1 slow reply inside the proof applies");
        assert_eq!(proof.started.elapsed_clock(), Duration::ZERO, "I1 anchor");
        assert_eq!((proof.lease_ttl_ms, proof.next_sequence), (3_000, 2), "I1");
        assert_eq!(sequences, vec![1], "I1 no retry");

        let (result, sequences, _) = initial_heartbeat_with(
            0,
            vec![
                (0, Err("503 unavailable".to_owned())),
                (700, Ok((Applied, Some(3_000)))),
            ],
        )
        .await;
        let proof = result.expect("I2 retry, not exit");
        assert_eq!(
            proof.started.elapsed_clock(),
            Duration::from_millis(100),
            "I2 anchor is the applied attempt's start"
        );
        assert_eq!(proof.next_sequence, 3, "I2");
        assert_eq!(sequences, vec![1, 2], "I2 fresh sequence per attempt");

        let (result, sequences, elapsed) = initial_heartbeat_with(0, Vec::new()).await;
        let error = result.err().expect("I2b exhausted proof exits");
        assert!(error.contains("registration proof expired"), "I2b: {error}");
        assert_eq!(sequences, vec![1], "I2b one attempt for the whole proof");
        assert_eq!(elapsed, Duration::from_secs(2), "I2b bounded by the proof");

        let (result, sequences, _) =
            initial_heartbeat_with(2_000, vec![(0, Ok((Applied, Some(3_000))))]).await;
        assert!(result.is_err(), "I3");
        assert!(sequences.is_empty(), "I3 no attempt without proof");

        let (result, sequences, _) =
            initial_heartbeat_with(1_990, vec![(10, Ok((Applied, Some(3_000))))]).await;
        let error = result.err().expect("I3b reply at the deadline is too late");
        assert!(error.contains("registration proof expired"), "I3b: {error}");
        assert_eq!(sequences, vec![1], "I3b");

        let (result, sequences, _) =
            initial_heartbeat_with(0, vec![(0, Ok((StaleIncarnation, None)))]).await;
        let error = result.err().expect("I4 refusal exits");
        assert!(error.contains("rejected: StaleIncarnation"), "I4: {error}");
        assert_eq!(sequences, vec![1], "I4 no retry after refusal");
    }

    #[test]
    fn a_reported_fact_is_retained_only_for_a_positive_observation() {
        // Rows W3-W5 of `reconcile_source_retention`: the fact names the
        // exact Session and subject digest; only Ok(true) is retention, so a
        // refused or failed observation stays closed.
        for (rule, observed, retained) in [
            ("W3", Ok(true), true),
            ("W4", Ok(false), false),
            ("W5", Err("provider unavailable"), false),
        ] {
            assert_eq!(
                super::source_retention_fact("session-a", "digest-a", observed),
                awaken_worker_contract::WorkerSourceRetention {
                    session_id: "session-a".into(),
                    subject: "digest-a".into(),
                    retained,
                },
                "{rule}"
            );
        }
    }

    #[tokio::test]
    async fn the_retention_report_follows_the_drain_and_survives_a_failed_subject_read() {
        // Rows W1-W2. Without a started claim pool the Worker cannot resume work,
        // so it reports as a draining Worker. An unreachable Coordinator
        // fails the subject read; the previous report stays for the next
        // heartbeat, and the Coordinator refuses it if the subject changed.
        let previous =
            std::collections::BTreeSet::from([awaken_worker_contract::WorkerSourceRetention {
                session_id: "session-a".into(),
                subject: "digest-a".into(),
                retained: true,
            }]);
        let lifecycle = super::WorkerSupervisor {
            authority: Default::default(),
            roles: awaken_session_executor::WorkerRoles::of(
                &*awaken_session_executor_testkit::volatile_host(
                    Arc::new(awaken_session_executor::NoModelConfiguredExecutor),
                    "worker-test",
                )
                .await,
                awaken_session_execution_contract::SessionApplications::Standalone,
            )
            .unwrap(),
            control: awaken_coordinator_client::WorkerControlClient::new(
                awaken_worker_transport_security::WorkerUpstream::new("http://127.0.0.1:1")
                    .with_worker_id("retiring"),
            ),
            identity: awaken_worker_contract::WorkerIdentity::new("retiring", "boot", 1),
            credential_observation_resolver: None,
            acp_capability_observation_source: None,
            session_environment_provider: None,
            local_readiness: None,
            observations: Arc::new(crate::WorkerObservationCache::default()),
            observation_ttl: std::time::Duration::from_secs(30),
            probe_interval: std::time::Duration::from_secs(10),
            warm_environments: Default::default(),
            source_retention: Arc::new(awaken_sync::LeafRwLock::new(previous.clone())),
        };
        assert!(!lifecycle.roles.dispatch().can_resume_work().await);
        assert!(lifecycle.reconcile_source_retention().await.is_err(), "W2");
        assert_eq!(lifecycle.source_retention(), previous, "W2 previous report");
        // Row W1: once the claim pool runs, the Worker is not draining; it
        // reports nothing and never asks the Coordinator for subjects (the
        // unreachable Coordinator would fail the read).
        lifecycle.roles.start_pool().unwrap();
        assert!(lifecycle.roles.dispatch().can_resume_work().await);
        assert_eq!(lifecycle.reconcile_source_retention().await, Ok(0), "W1");
        assert!(lifecycle.source_retention().is_empty(), "W1 empty report");
        assert!(
            lifecycle.heartbeat(1, true).source_retention.is_empty(),
            "W1"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_local_backend_lost_after_start_drains_like_a_lost_container_daemon() {
        // ADR-0092 decision 5. Causes: C1 a container provider is installed;
        // C2 a local tier's readiness probe is installed; C3 the probe answers.
        // Effects: E1 the Worker proves environment evidence before Ready and
        // on every heartbeat; E2 a rejecting local probe closes the evidence
        // monitor (the Worker drains); E3 with neither installed there is no
        // environment evidence to prove.
        // | Rule | C1 | C2 | C3 | effect |
        // | L1 | no | yes | ok | E1, monitor keeps running |
        // | L2 | no | yes | lost | E1 + E2 |
        // | L3 | no | no | - | E3 |
        use std::time::Duration;
        let alive = Arc::new(AtomicBool::new(true));
        let probe_state = alive.clone();
        let probe: super::LocalReadinessProbe = Arc::new(move || {
            let alive = probe_state.load(Ordering::SeqCst);
            Box::pin(async move {
                if alive {
                    Ok(())
                } else {
                    Err(awaken_provisioning_contract::SandboxError::not_ready(
                        "bwrap revoked",
                    ))
                }
            })
        });
        let supervisor = |local_readiness| async move {
            Arc::new(super::WorkerSupervisor {
                authority: Default::default(),
                roles: awaken_session_executor::WorkerRoles::of(
                    &*awaken_session_executor_testkit::volatile_host(
                        Arc::new(awaken_session_executor::NoModelConfiguredExecutor),
                        "worker-test",
                    )
                    .await,
                    awaken_session_execution_contract::SessionApplications::Standalone,
                )
                .unwrap(),
                control: awaken_coordinator_client::WorkerControlClient::new(
                    awaken_worker_transport_security::WorkerUpstream::new("http://127.0.0.1:1")
                        .with_worker_id("local-probe"),
                ),
                identity: awaken_worker_contract::WorkerIdentity::new("local-probe", "boot", 1),
                credential_observation_resolver: None,
                acp_capability_observation_source: None,
                session_environment_provider: None,
                local_readiness,
                observations: Arc::new(crate::WorkerObservationCache::default()),
                observation_ttl: Duration::from_secs(30),
                probe_interval: Duration::from_secs(10),
                warm_environments: Default::default(),
                source_retention: Default::default(),
            })
        };
        let none = supervisor(None).await;
        assert!(!none.environment_readiness_installed(), "L3");
        let local = supervisor(Some(probe)).await;
        assert!(local.environment_readiness_installed(), "L1");
        assert!(local.probe_environment().await.is_ok(), "L1");
        let timing = awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(6000);
        let monitored = local.clone();
        let monitor = tokio::spawn(async move {
            super::environment_evidence::monitor(awaken_time::mono::now(), timing, || {
                monitored.probe_environment()
            })
            .await;
        });
        // awaken-allow(wait): paused clock; three proof windows of healthy renewals
        // must pass without the monitor closing, which is the fact under test.
        tokio::time::sleep(timing.proof_window() * 3).await;
        assert!(!monitor.is_finished(), "L1 healthy evidence keeps renewing");
        alive.store(false, Ordering::SeqCst);
        assert!(local.probe_environment().await.is_err(), "L2");
        tokio::time::timeout(timing.proof_window() * 2, monitor)
            .await
            .expect("L2 a lost local backend closes the evidence monitor")
            .unwrap();
    }

    #[tokio::test]
    async fn image_retirement_never_uses_incomplete_fallback_receipts() {
        // F1 fresh Control demand => provider invoked once, including an empty
        // set; F2 failed fetch => no provider call (existing holders survive);
        // F3 provider fails => propagate error, never fabricate readiness.
        for (fresh, succeeds) in [(true, true), (false, true), (true, false)] {
            let fetched = if fresh {
                Ok(Vec::new())
            } else {
                Err("Control unavailable".into())
            };
            let calls = AtomicUsize::new(0);
            let result = super::prepare_current_environment_images(&fetched, |desired| async {
                calls.fetch_add(1, Ordering::SeqCst);
                assert!(desired.is_empty());
                if succeeds {
                    Ok(())
                } else {
                    Err("provider failed")
                }
            })
            .await;
            assert_eq!(calls.load(Ordering::SeqCst), usize::from(fresh));
            assert_eq!(result.is_ok(), !fresh || succeeds);
        }
    }

    #[test]
    fn cold_cleanup_claim_fits_the_registry_heartbeat_window() {
        // Causes: C1 Registry TTL, C2 heartbeat age at the cold sweep, C3 a
        // stalled heartbeat. Effect E1: the claim fits an ordinary heartbeat
        // window; E2: an overdue Worker is still refused by the root gate.
        // | Rule | TTL | age | claim | Worker expiry | Effect |
        // | R1 | 30s | 10s+1ms | 15s | <20s | E1 |
        // | R2 | 6s | 2s+1ms | 3s | <4s | E1 |
        // | R3 | 30s | 20s | 15s | 10s | E2 |
        // | R4 | 60s | 20s+1ms | 20s | <40s | E1 |
        // The former fixed 20s claim fails R1 when the two 10s cadences align.
        for (registry_ttl_ms, heartbeat_age_ms, expected_claim_ms, fits) in [
            (30_000, 10_001, 15_000, true),
            (6_000, 2_001, 3_000, true),
            (30_000, 20_000, 15_000, false),
            (60_000, 20_001, 20_000, true),
        ] {
            let claim_ms = cold_cleanup_lease_ttl_ms(
                awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(
                    registry_ttl_ms,
                ),
            );
            assert_eq!(claim_ms, expected_claim_ms);
            assert_eq!(
                claim_ms <= registry_ttl_ms - heartbeat_age_ms,
                fits,
                "TTL {registry_ttl_ms}, heartbeat age {heartbeat_age_ms}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_revocation_retains_observation_and_never_converts_timeout_to_completion() {
        // Cause/effect decision table for the one bounded revocation owner:
        // C1 local fence/interruption completes; C2 Runtime return is observed;
        // C3 Session retirement completes; C4 unused Environment capacity
        // shuts down; C5 the absolute deadline expires. E1 Completed is emitted
        // only for C1+C2+C3+C4 before C5; E2 any failed or
        // stalled phase emits typed Unconfirmed; E3 a pending return observer is
        // retained until C5; E4 retirement still runs after an explicit false
        // return observation, but cannot convert it to completion; E5 capacity
        // shutdown is attempted after revocation and shares the same deadline.
        // Constraint: capacity is the final phase and receives only the time
        // remaining after R1-R5; R6 isolates its timeout partition.
        //
        // | Rule | C1 | C2 | C3 | C4 | C5 | Effect |
        // |---|---|---|---|---|---|---|
        // | R1 | yes | yes | yes | yes | no | E1+E5 |
        // | R2 | pending | - | - | - | yes | E2/Fence |
        // | R3 | yes | pending | - | - | yes | E2+E3/Return |
        // | R4 | yes | false | yes | - | no | E2+E4/Return |
        // | R5 | yes | yes | pending | - | yes | E2/Retirement |
        // | R6 | yes | yes | yes | pending | yes | E2/Capacity+E5 |
        let budget = std::time::Duration::from_secs(5);
        let completed_deadline = awaken_time::mono::now() + budget;
        let completed = complete_worker_revocation_before(
            completed_deadline,
            async { Ok(2) },
            |_| async { true },
            async { Ok(()) },
        )
        .await;
        let completed =
            complete_worker_capacity_before(completed_deadline, completed, async {}).await;
        assert_eq!(
            completed,
            WorkerRevocationOutcome::Completed { interrupted: 2 },
            "R1/E1"
        );

        let fence_timeout = complete_worker_revocation_before(
            awaken_time::mono::now() + budget,
            std::future::pending(),
            |_| async { true },
            async { Ok(()) },
        )
        .await;
        assert!(
            matches!(
                fence_timeout,
                WorkerRevocationOutcome::Unconfirmed {
                    stage: WorkerRevocationStage::LocalFenceAndInterrupt,
                    interrupted: None,
                    ..
                }
            ),
            "R2/E2"
        );

        let observer_dropped = Arc::new(AtomicBool::new(false));
        let observer_started = Arc::new(tokio::sync::Notify::new());
        let deadline = awaken_time::mono::now() + budget;
        let task = tokio::spawn(complete_worker_revocation_before(
            deadline,
            async { Ok(3) },
            {
                let observer_dropped = observer_dropped.clone();
                let observer_started = observer_started.clone();
                move |_| async move {
                    struct ObserverDrop(Arc<AtomicBool>);
                    impl Drop for ObserverDrop {
                        fn drop(&mut self) {
                            self.0.store(true, Ordering::SeqCst);
                        }
                    }
                    let _observer = ObserverDrop(observer_dropped);
                    observer_started.notify_one();
                    std::future::pending().await
                }
            },
            async { Ok(()) },
        ));
        observer_started.notified().await;
        tokio::time::advance(budget - std::time::Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert!(
            !task.is_finished(),
            "R3/E3 observer retained before deadline"
        );
        assert!(!observer_dropped.load(Ordering::SeqCst), "R3/E3");
        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        let return_timeout = task.await.unwrap();
        assert!(observer_dropped.load(Ordering::SeqCst), "R3/E3");
        assert!(
            matches!(
                return_timeout,
                WorkerRevocationOutcome::Unconfirmed {
                    stage: WorkerRevocationStage::RuntimeReturn,
                    interrupted: Some(3),
                    ..
                }
            ),
            "R3/E2"
        );

        let retired = Arc::new(AtomicBool::new(false));
        let explicit_unknown = complete_worker_revocation_before(
            awaken_time::mono::now() + budget,
            async { Ok(4) },
            |_| async { false },
            {
                let retired = retired.clone();
                async move {
                    retired.store(true, Ordering::SeqCst);
                    Ok(())
                }
            },
        )
        .await;
        assert!(retired.load(Ordering::SeqCst), "R4/E4");
        assert!(
            matches!(
                explicit_unknown,
                WorkerRevocationOutcome::Unconfirmed {
                    stage: WorkerRevocationStage::RuntimeReturn,
                    interrupted: Some(4),
                    ..
                }
            ),
            "R4/E2"
        );

        let retirement_timeout = complete_worker_revocation_before(
            awaken_time::mono::now() + budget,
            async { Ok(5) },
            |_| async { true },
            std::future::pending(),
        )
        .await;
        assert!(
            matches!(
                retirement_timeout,
                WorkerRevocationOutcome::Unconfirmed {
                    stage: WorkerRevocationStage::SessionRetirement,
                    interrupted: Some(5),
                    ..
                }
            ),
            "R5/E2"
        );

        let capacity_timeout = complete_worker_capacity_before(
            awaken_time::mono::now() + budget,
            WorkerRevocationOutcome::Completed { interrupted: 6 },
            std::future::pending(),
        )
        .await;
        assert!(
            matches!(
                capacity_timeout,
                WorkerRevocationOutcome::Unconfirmed {
                    stage: WorkerRevocationStage::EnvironmentCapacity,
                    interrupted: Some(6),
                    ..
                }
            ),
            "R6/E2+E5"
        );
    }

    #[test]
    fn quiescence_publication_requires_completed_cleanup_and_zero_in_flight() {
        // Causes: C1 the bounded cleanup is Completed or Unconfirmed; C2 the
        // authoritative local in-flight count is zero or nonzero. Effects: E1
        // permit Registry quiescence publication; E2 withhold it. Constraint:
        // interrupted-count metadata does not weaken either prerequisite.
        //
        // | Rule | C1 | C2 | Effect |
        // |---|---|---|---|
        // | Q1 | Completed | zero | E1 |
        // | Q2 | Completed | nonzero | E2 |
        // | Q3 | Unconfirmed | zero | E2 |
        // | Q4 | Unconfirmed | nonzero | E2 |
        let completed = WorkerRevocationOutcome::Completed { interrupted: 2 };
        let unconfirmed = WorkerRevocationOutcome::Unconfirmed {
            stage: WorkerRevocationStage::EnvironmentCapacity,
            interrupted: Some(2),
            detail: None,
        };
        assert!(completed.permits_quiescence_publication(0), "Q1/E1");
        assert!(!completed.permits_quiescence_publication(1), "Q2/E2");
        assert!(!unconfirmed.permits_quiescence_publication(0), "Q3/E2");
        assert!(!unconfirmed.permits_quiescence_publication(1), "Q4/E2");
    }

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
    impl awaken_provisioning_contract::SandboxReadiness for ProbeProvider {
        fn capabilities(&self) -> awaken_provisioning_contract::SandboxCapabilities {
            Default::default()
        }

        fn checkpoint_formats(&self) -> Vec<String> {
            Vec::new()
        }

        async fn probe_ready(&self) -> Result<(), awaken_provisioning_contract::SandboxError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            match (self.mode, call) {
                (1, 0) | (2, _) => std::future::pending().await,
                (3, _) => Err(awaken_provisioning_contract::SandboxError::gone(
                    "evidence lost",
                )),
                _ => Ok(()),
            }
        }
    }

    #[async_trait::async_trait]
    impl awaken_provisioning_contract::SandboxProvider for ProbeProvider {
        type Environment = dyn awaken_provisioning_contract::SessionSandbox;

        async fn prewarm_images(
            &self,
            _specs: &[awaken_provisioning_contract::SandboxSpec],
        ) -> Result<(), awaken_provisioning_contract::SandboxError> {
            Ok(())
        }

        fn adoption_layout(
            &self,
            handle: &awaken_provisioning_contract::SandboxHandle,
        ) -> Result<
            awaken_provisioning_contract::SandboxAdoptionLayout,
            awaken_provisioning_contract::SandboxError,
        > {
            awaken_sandbox_container::container_adoption_layout(handle)
        }

        fn validate_closed_observation(
            &self,
            handle: &awaken_provisioning_contract::SandboxHandle,
            observation: &awaken_provisioning_contract::SandboxObservation,
        ) -> Result<(), awaken_provisioning_contract::SandboxError> {
            awaken_sandbox_container::validate_container_closed_observation(handle, observation)
        }

        fn claim_for_effect<'a>(
            &'a self,
            spec: &'a awaken_provisioning_contract::SandboxSpec,
            effect: awaken_provisioning_contract::SandboxEffect<'a>,
            intent: awaken_provisioning_contract::SandboxRealizationIntent,
        ) -> awaken_provisioning_contract::ClaimedEffect<
            'a,
            std::sync::Arc<dyn awaken_provisioning_contract::SessionSandbox>,
        > {
            let claim = awaken_provisioning_contract::EffectClaim::in_process(
                awaken_provisioning_contract::test_support::retain_abandoned_effect(
                    spec.scope.clone(),
                ),
            );
            awaken_provisioning_contract::ClaimedEffect::new(claim, async move {
                intent.admit_under(effect)?;
                match effect {
                    awaken_provisioning_contract::SandboxEffect::Unattached => {
                        crate::tests::UnusedContainerProvider
                            .claim_for_effect(
                                spec,
                                awaken_provisioning_contract::SandboxEffect::Unattached,
                                awaken_provisioning_contract::SandboxRealizationIntent::Create,
                            )
                            .await
                    }
                    awaken_provisioning_contract::SandboxEffect::Fenced { fence, authority } => {
                        let _spec = spec;
                        let _effect_fence = fence;
                        let _authority = authority;
                        let _intent = intent;

                        Err(awaken_provisioning_contract::SandboxError::unsupported(
                            "test double does not implement fenced Environment creation",
                        ))
                    }
                }
            })
        }

        async fn observe(
            &self,
            _adoption: awaken_provisioning_contract::SandboxAdoption<'_>,
            _basis: awaken_provisioning_contract::SandboxObservationBasis<'_>,
        ) -> Result<
            awaken_provisioning_contract::SandboxObservation,
            awaken_provisioning_contract::SandboxError,
        > {
            Err(awaken_provisioning_contract::SandboxError::unsupported(
                "test double does not observe Environments",
            ))
        }

        async fn adopt_for_effect(
            &self,
            adoption: awaken_provisioning_contract::SandboxAdoption<'_>,
            effect: awaken_provisioning_contract::SandboxEffect<'_>,
        ) -> Result<
            std::sync::Arc<dyn awaken_provisioning_contract::SessionSandbox>,
            awaken_provisioning_contract::SandboxError,
        > {
            match effect {
                awaken_provisioning_contract::SandboxEffect::Unattached => {
                    crate::tests::UnusedContainerProvider
                        .adopt_for_effect(
                            adoption,
                            awaken_provisioning_contract::SandboxEffect::Unattached,
                        )
                        .await
                }
                awaken_provisioning_contract::SandboxEffect::Fenced { fence, authority } => {
                    let _adoption = adoption;
                    let _effect_fence = fence;
                    let _authority = authority;

                    Err(awaken_provisioning_contract::SandboxError::unsupported(
                        "test double does not implement fenced Environment adoption",
                    ))
                }
            }
        }

        async fn prepare_terminal_for_effect(
            &self,
            _spec: &awaken_provisioning_contract::SandboxSpec,
            _handle: Option<&awaken_provisioning_contract::SandboxHandle>,
            _expected_effect_fence: Option<&awaken_provisioning_contract::SandboxEffectFence>,
            _terminal_effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
            _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
        ) -> Result<
            Option<std::sync::Arc<dyn awaken_provisioning_contract::SessionSandbox>>,
            awaken_provisioning_contract::SandboxError,
        > {
            Err(awaken_provisioning_contract::SandboxError::unsupported(
                "test double does not implement fenced terminal preparation",
            ))
        }

        fn restoration(
            &self,
        ) -> awaken_provisioning_contract::SandboxRestoration<
            '_,
            std::sync::Arc<dyn awaken_provisioning_contract::SessionSandbox>,
        > {
            awaken_provisioning_contract::SandboxRestoration::Unsupported
        }
    }

    impl awaken_provisioning_contract::SandboxInstallation for ProbeProvider {
        fn install_memory_mounter(
            &self,
            _mounter: std::sync::Arc<dyn awaken_provisioning_contract::MemoryMounter>,
        ) {
        }

        fn install_secret_broker(
            &self,
            _broker: std::sync::Arc<dyn awaken_provisioning_contract::SecretBroker>,
        ) {
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
                roles: awaken_session_executor::WorkerRoles::of(
                    &*awaken_session_executor_testkit::volatile_host(
                        Arc::new(awaken_session_executor::NoModelConfiguredExecutor),
                        "none",
                    )
                    .await,
                    awaken_session_execution_contract::SessionApplications::Standalone,
                )
                .unwrap(),
                control: WorkerControlClient::new(
                    awaken_worker_transport_security::WorkerUpstream::new(format!(
                        "http://{address}"
                    )),
                ),
                identity: identity.clone(),
                credential_observation_resolver: None,
                acp_capability_observation_source: None,
                session_environment_provider: Some(provider.clone()),
                local_readiness: None,
                observations: Arc::new(WorkerObservationCache::default()),
                observation_ttl: Duration::from_secs(6),
                probe_interval: Duration::from_secs(1),
                warm_environments: Default::default(),
                source_retention: Default::default(),
            });
            let task = spawn_heartbeat(
                lifecycle,
                1,
                AuthorityLeaseTiming::from_ttl_ms(6000),
                awaken_time::mono::now(),
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
        // T6 timeout vs deadline-clipped expiry emits exact elapsed/remaining
        // diagnostics from that same timing owner, without changing either
        // deadline. A returned timeout must never look like new evidence.
        use tracing::instrument::WithSubscriber;
        let captured = awaken_reliability_testkit::events::CapturedEvents::default();
        let timing = awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(600);
        let started = awaken_time::mono::now();
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
        .with_subscriber(captured.clone())
        .await;
        let events = captured.snapshot();
        let timeouts = events
            .iter()
            .filter(|fields| fields.contains_key("probe_elapsed_ms"))
            .collect::<Vec<_>>();
        assert_eq!(timeouts.len(), 2, "T6 timeout then expiry");
        for (event, expected) in timeouts
            .iter()
            .zip([("100", "300", "100"), ("100", "400", "0")])
        {
            assert_eq!(event["probe_elapsed_ms"], expected.0);
            assert_eq!(event["proof_age_ms"], expected.1);
            assert_eq!(event["remaining_proof_ms"], expected.2);
            assert_eq!(event["request_budget_ms"], "100");
        }
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
        let timing = awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(600);
        let calls = AtomicUsize::new(0);
        super::environment_evidence::monitor(
            awaken_time::mono::now() - timing.proof_window(),
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
        let timing = awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(600);
        let started = awaken_time::mono::now();
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
        const PERIOD: std::time::Duration = super::SESSION_TERMINAL_RECONCILIATION_PERIOD;

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
        // C5 a normal terminal command queues a root between polls: the same
        // production period must discover it without waiting a former ten-second
        // recovery delay. R1-R3 also apply at this prompt cadence: no overlap,
        // cancellation, burst replay or shortened lease proof window is allowed.
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
