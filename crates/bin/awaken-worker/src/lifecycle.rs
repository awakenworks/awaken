//! Worker registry authority, heartbeat, and local admission lifecycle.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use awaken_runtime_contract::authority_lease::AuthorityLeaseTiming;
use awaken_runtime_host::SharedHost;
use awaken_worker_contract::{RegistryMutation, WorkerHeartbeat, WorkerIdentity};
use awaken_worker_runtime::WorkerControlClient;

use crate::credential_liveness::WorkerObservationCache;

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
    mut last_proof: Instant,
    mut timing: AuthorityLeaseTiming,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        awaken_observability::set_worker_authority_proof_remaining(
            timing.remaining_proof_after(Instant::now().saturating_duration_since(last_proof)),
        );
        let mut next_attempt = last_proof + timing.renew_interval();
        let mut next_regular = next_attempt;
        let mut admission_suspended = false;
        let mut consecutive_failures = 0_u64;
        loop {
            tokio::time::sleep_until(tokio::time::Instant::from_std(next_attempt)).await;
            let attempt_started = Instant::now();
            let scheduling_lag = attempt_started.saturating_duration_since(next_attempt);
            let proof_deadline = last_proof + timing.proof_window();
            if attempt_started >= proof_deadline {
                awaken_observability::record_worker_authority_loss("proof_expired");
                tracing::error!(
                    sequence,
                    consecutive_failures,
                    "worker registry authority proof expired; draining locally"
                );
                revoke_worker_session_authority(&lifecycle).await;
                break;
            }
            if let Some(provider) = &lifecycle.session_environment_provider {
                let probe_timeout = timing
                    .request_timeout()
                    .min(proof_deadline.saturating_duration_since(Instant::now()));
                match tokio::time::timeout(probe_timeout, provider.probe_ready()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        awaken_observability::record_worker_authority_loss(
                            "environment_evidence_lost",
                        );
                        tracing::error!(error = %error, "worker sandbox evidence no longer holds; draining locally");
                        revoke_worker_session_authority(&lifecycle).await;
                        break;
                    }
                    Err(_) => {
                        awaken_observability::record_worker_authority_loss(
                            "environment_probe_timeout",
                        );
                        tracing::error!(
                            timeout_ms = probe_timeout.as_millis(),
                            "worker sandbox evidence probe timed out; draining locally"
                        );
                        revoke_worker_session_authority(&lifecycle).await;
                        break;
                    }
                }
            }
            let ready = lifecycle.host.pool_can_resume_work().await;
            let heartbeat_timeout = timing
                .request_timeout()
                .min(proof_deadline.saturating_duration_since(Instant::now()));
            let mutation = tokio::time::timeout(
                heartbeat_timeout,
                lifecycle.control.heartbeat(
                    &lifecycle.identity,
                    WorkerHeartbeat {
                        sequence,
                        ready,
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
            sequence = match sequence.checked_add(1) {
                Some(next) => next,
                None => {
                    awaken_observability::record_worker_authority_loss("sequence_exhausted");
                    tracing::error!("worker heartbeat sequence exhausted; draining locally");
                    revoke_worker_session_authority(&lifecycle).await;
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
                    timing = AuthorityLeaseTiming::from_ttl_ms(receipt_ttl);
                    // The Coordinator applied the renewal at an unknown instant
                    // between request start and response receipt. Request start is
                    // the conservative local lower bound; response latency must
                    // never mint extra local authority.
                    last_proof = attempt_started;
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
                        while next_regular <= last_proof {
                            next_regular += timing.renew_interval();
                        }
                    } else {
                        next_regular = last_proof + timing.renew_interval();
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
                        timing.remaining_proof_after(
                            Instant::now().saturating_duration_since(last_proof),
                        ),
                    );
                    // Cold terminal cleanup recovery remains coupled to the
                    // registry heartbeat: claiming an orphaned assignment is a
                    // Worker-authority operation and does not need a hot-path
                    // polling cadence.
                    let now = wall_clock_ms();
                    let cleanup_target = awaken_session_contract::SessionRealizationTarget {
                        owner: lifecycle.identity.worker_id.clone(),
                        runtime_incarnation: lifecycle.identity.lease_owner(),
                        lease_expires_at_unix_ms: now
                            .saturating_add(SESSION_REALIZATION_LEASE_TTL_MS),
                        reassign_existing_lease: false,
                    };
                    match tokio::time::timeout(
                        timing.request_timeout(),
                        lifecycle
                            .host
                            .recover_terminal_cleanup_assignments(cleanup_target),
                    )
                    .await
                    {
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => eprintln!(
                            "cold Session terminal cleanup recovery remained pending; Worker heartbeat continues: {error}"
                        ),
                        Err(_) => eprintln!(
                            "cold Session terminal cleanup recovery exceeded its bounded request window; durable assignments remain retryable and Worker heartbeat continues"
                        ),
                    }
                }
                Ok(Ok(receipt)) => {
                    awaken_observability::record_worker_authority_heartbeat(
                        "rejected",
                        Instant::now().duration_since(attempt_started),
                        scheduling_lag,
                        proof_deadline.saturating_duration_since(Instant::now()),
                    );
                    awaken_observability::record_worker_authority_loss("registry_rejected");
                    tracing::error!(mutation = ?receipt.mutation, "worker heartbeat explicitly lost authority; draining locally");
                    revoke_worker_session_authority(&lifecycle).await;
                    break;
                }
                Ok(Err(error)) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    if !admission_suspended {
                        lifecycle.host.suspend_pool_admission().await;
                        admission_suspended = true;
                    }
                    let now = Instant::now();
                    let remaining = proof_deadline.saturating_duration_since(now);
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
                    if remaining.is_zero() {
                        awaken_observability::record_worker_authority_loss("proof_expired");
                        revoke_worker_session_authority(&lifecycle).await;
                        break;
                    }
                    next_attempt = now + timing.retry_delay().min(remaining);
                }
                Err(_) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    if !admission_suspended {
                        lifecycle.host.suspend_pool_admission().await;
                        admission_suspended = true;
                    }
                    let now = Instant::now();
                    let remaining = proof_deadline.saturating_duration_since(now);
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
                    if remaining.is_zero() {
                        awaken_observability::record_worker_authority_loss("proof_expired");
                        revoke_worker_session_authority(&lifecycle).await;
                        break;
                    }
                    next_attempt = now + timing.retry_delay().min(remaining);
                }
            }
        }
    })
}

/// Reconcile due resident Session realization leases independently from the
/// Worker registry heartbeat. This lane performs no terminal discovery: the
/// heartbeat-paced global claim-next command is the sole cleanup selector, so
/// an idle fleet creates no per-Session Control traffic. Ordinary terminal
/// settlement remains on the initiating command path; this supervisor owns only
/// recovery of cold/orphaned cleanup assignments.
pub(crate) fn spawn_session_realization_reconciliation(
    lifecycle: Arc<WorkerSupervisor>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let timing = AuthorityLeaseTiming::from_ttl_ms(SESSION_REALIZATION_LEASE_TTL_MS);
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let now = wall_clock_ms();
            // Cause/effect decision table: C1 this sweep owns only due Session
            // lease renewals; C2 the Host bounds independent calls; C3 the
            // Worker heartbeat/terminal claim is a separate task and authority
            // lane; C4 one Control renewal is slow. Effects: E1 await the
            // bounded sweep without starting an overlapping sweep; E2 never
            // cancel every other Session at one arbitrary batch deadline; E3
            // heartbeat proof remains independently schedulable; E4 non-due
            // Sessions make no Control call. Durable fences own failure/recovery.
            //
            // | Rule | bounded Host | slow call | Effect |
            // |---|---|---|---|
            // | R1 | yes | no | E1 + E3 + E4 |
            // | R2 | yes | yes | E1 + E2 + E3 + E4 |
            match lifecycle
                .host
                .renew_due_session_realizations(now, timing)
                .await
            {
                Ok(_) => {}
                Err(error) => eprintln!(
                    "Session realization reconciliation failed closed; projections retain their existing deadlines: {error}"
                ),
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

async fn revoke_worker_session_authority(lifecycle: &WorkerSupervisor) {
    awaken_observability::set_worker_authority_proof_remaining(std::time::Duration::ZERO);
    lifecycle.host.begin_pool_drain().await;
    // Fail closed without racing terminal sandbox disposal against in-flight
    // native tools. Cancellation stops each process group (including descendant
    // containers), and the bounded drain lets those futures release their
    // workspace handles before the directories are reaped below.
    lifecycle.host.interrupt_all_session_runs().await;
    let _ = lifecycle
        .host
        .drain_runtime(std::time::Duration::from_secs(20))
        .await;
    if let Err(error) = lifecycle.host.revoke_all_session_realizations().await {
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
