// @formal entrypoint session.internal_create_accepts_only_a_complete_durable_root_and_recovers_without_a_job_store
// @formal entrypoint session_execution.terminal_state_never_reopens
// @formal entrypoint session_realization.publication_excludes_staged_retired_generation
// @formal entrypoint session_realization.recovery_backoff_is_bounded_and_dependency_wakes_never_postpone_the_existing_deadline
//! Canonical durable Session realization phase commands.

use awaken_time::WallClockExt as _;
use std::collections::BTreeSet;

use awaken_session_mcp_contract::{McpAttachmentState, McpGenerationRef, StageMcpAttachment};
use awaken_session_realization_contract::{
    AcknowledgeSessionRealization, ActivateSessionRealization, BeginSessionRealization,
    FailSessionRealization, RenewSessionRealization, SessionRealizationAction,
    SessionRealizationControl, SessionRealizationControlFailure, SessionRealizationDirective,
    SessionTerminalCleanupAssignment,
};
use awaken_session_resource_contract::SessionRealizationLease;
use awaken_session_execution_contract::RealizationRuntime;
use awaken_session_state_contract::{
    ManagedLifecycleFact, PersistedSession, SessionEnvironmentState, SessionExecutionState,
    SessionRepositoryError,
};
use awaken_session_types::RunError;

/// The same Environment phase gate owns scan and direct realization entry.
/// Continuation has closed process admission until restore commits Resident.
pub(super) fn environment_admits_realization_effects(
    environment: &SessionEnvironmentState,
) -> bool {
    matches!(
        environment,
        SessionEnvironmentState::Unmaterialized | SessionEnvironmentState::Resident { .. }
    )
}

use super::{SessionApplication, SessionMutationError};
use crate::{activity::runtime_interval_fact, creation::DurableSessionRoot, projection};

mod authority;
mod control;
mod local_lock;
mod publication;
mod recovery_backoff;

use publication::{draining_generations, publication_action, publication_generations};

pub(super) use authority::terminal_cleanup_worker_requirements;
use authority::{
    TerminalCleanupClaimConflict, TerminalCleanupClaimScope, TerminalCleanupRootClaim,
    exact_generation_key, generation_set_authorizes, verify_lease,
};
use control::RefreshedSessionRealizationControl;
pub(crate) use local_lock::LocalRealizationLocks;
use recovery_backoff::{
    SessionRecoveryTrigger, external_wake_coalescing_delay, next_external_wake_cooldown,
    next_session_recovery_delay,
};

/// One timing policy for every realization owned by the co-located Runtime.
/// Terminal cleanup uses the same lease horizon but renews at one third of it
/// while its canonical drive is still pending.
pub(crate) use awaken_session_realization_driver::LOCAL_SESSION_REALIZATION_LEASE_MS;

fn initial_idle_fact(owner_scope: &str, session_id: &str, now_ms: u64) -> ManagedLifecycleFact {
    ManagedLifecycleFact {
        id: format!("session:{session_id}:created"),
        object_id: session_id.to_string(),
        workspace_id: Some(owner_scope.to_string()),
        event_type: "session.status_idled".to_string(),
        timestamp: i64::try_from(now_ms / 1_000).unwrap_or(i64::MAX),
        runtime_interval: None,
    }
}

fn unavailable(error: impl std::fmt::Display) -> SessionRealizationControlFailure {
    SessionRealizationControlFailure::Unavailable(error.to_string())
}

pub(crate) fn repository_control(
    error: SessionRepositoryError,
) -> SessionRealizationControlFailure {
    match error {
        SessionRepositoryError::NotFound => SessionRealizationControlFailure::NotFound,
        error => unavailable(error),
    }
}

pub(crate) fn mutation_control(error: SessionMutationError) -> SessionRealizationControlFailure {
    match error {
        SessionMutationError::NotFound => SessionRealizationControlFailure::NotFound,
        error => unavailable(error),
    }
}

fn validate_realization_target(
    target: &awaken_session_realization_contract::SessionRealizationTarget,
    now_ms: u64,
) -> Result<(), SessionRealizationControlFailure> {
    if target.owner.trim().is_empty()
        || target.runtime_incarnation.trim().is_empty()
        || !awaken_session_realization_contract::realization_lease_is_live_at(
            target.lease_expires_at_unix_ms,
            now_ms,
        )
    {
        return Err(SessionRealizationControlFailure::Invalid(
            "Runtime owner/incarnation and a future lease expiry are required".into(),
        ));
    }
    Ok(())
}

fn validate_target(
    command: &BeginSessionRealization,
    now_ms: u64,
) -> Result<(), SessionRealizationControlFailure> {
    if command.session_id.as_str().trim().is_empty() {
        return Err(SessionRealizationControlFailure::Invalid(
            "Session id is required".into(),
        ));
    }
    validate_realization_target(&command.target, now_ms)
}

/// Failure while the Session application drives durable realization effects.
#[derive(Debug, thiserror::Error)]
pub enum SessionRealizationError {
    #[error("Session realization control failed: {0}")]
    Control(#[source] SessionRealizationControlFailure),
    #[error("Session realization effect failed: {0}")]
    Effect(#[source] RunError),
    #[error("Session realization did not converge")]
    DidNotConverge,
}

/// One Session recovery failure retained for a later reconciliation pass.
#[derive(Debug)]
pub struct SessionReconciliationFailure {
    pub session_id: String,
    pub message: String,
}

/// Protocol-neutral result of one durable Session recovery scan.
#[derive(Debug, Default)]
pub struct SessionReconciliation {
    pub settled: Vec<PersistedSession>,
    pub failures: Vec<SessionReconciliationFailure>,
    pub quarantined: Vec<awaken_session_state_contract::SessionRecoveryQuarantine>,
    pub pending: usize,
}

struct LocalProjectionSynchronizer<'a> {
    runtime: &'a dyn RealizationRuntime,
}

#[async_trait::async_trait]
impl awaken_session_realization_contract::SessionProjectionSynchronizer
    for LocalProjectionSynchronizer<'_>
{
    fn observe_realization_request(
        &self,
    ) -> Result<awaken_lease_contract::authority::AuthorityProofStarted, RunError> {
        self.runtime.observe_realization_request()
    }

    async fn synchronize_session_projection(
        &self,
        session_id: &awaken_session_types::SessionId,
        projection: &awaken_session_realization_contract::FrozenSessionProjection,
        lease: &awaken_session_realization_contract::ObservedSessionRealizationLease,
        prepare_session: bool,
    ) -> Result<(), RunError> {
        self.runtime
            .install_session_projection(
                session_id.as_str(),
                projection.clone(),
                awaken_session_realization_contract::SessionProjectionInstallMode::Realization {
                    lease: lease.clone(),
                    prepare_session,
                },
            )
            .await
    }

    async fn invalidate_session_realization_lease(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &SessionRealizationLease,
    ) -> Result<(), RunError> {
        self.runtime
            .invalidate_session_realization_lease(session_id.as_str(), lease)
            .await
    }
}

impl SessionApplication {
    /// Reproject one already-committed local Resident Environment through the
    /// same complete-projection port used by the canonical realization driver.
    /// Resident durable truth is deliberately the retry fact: Runtime owns no
    /// projection receipt or parallel restore-completion state.
    pub(super) async fn synchronize_resident_environment_projection(
        &self,
        owner_scope: &str,
        session: &PersistedSession,
    ) -> Result<(), RunError> {
        let realization_runtime = self.runtime().realization();

        if !matches!(
            session.environment,
            SessionEnvironmentState::Resident { .. }
        ) {
            return Err(RunError::internal(
                "only a committed Resident Environment may be reprojected",
            ));
        }
        if self.requires_external_realization(session) {
            return Err(RunError::internal(
                "a Worker-owned Resident Environment cannot be projected by the local Runtime",
            ));
        }
        let lease = session.realization.as_ref().ok_or_else(|| {
            RunError::unavailable_classified(
                "session_environment_realization_missing",
                "Resident Environment has no exact realization lease",
            )
        })?;
        let request_started = realization_runtime.observe_realization_request()?;
        let projection = self
            .frozen_session_projection(owner_scope.to_string(), session, true)
            .await?;
        awaken_session_realization_contract::SessionProjectionSynchronizer::synchronize_session_projection(
            &LocalProjectionSynchronizer {
                runtime: realization_runtime,
            },
            &session.session_id,
            &projection,
            &awaken_session_realization_contract::SessionRealizationLeaseReceipt::issued_at(
                lease.clone(),
                self.clock.now_ms(),
            )
            .map_err(|error| RunError::unavailable(error.to_string()))?
            .observed(request_started),
            true,
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn reconcile_pending_session_state(
        self: std::sync::Arc<Self>,
    ) -> SessionRecoveryCycle {
        self.reconcile_pending_session_state_for(SessionRecoveryTrigger::Scheduled)
            .await
    }

    async fn reconcile_pending_session_state_for(
        self: std::sync::Arc<Self>,
        trigger: SessionRecoveryTrigger,
    ) -> SessionRecoveryCycle {
        let cycle_started = awaken_time::mono::Stopwatch::start();
        let candidates = match super::scan_recovery_candidates(
            self.session_repository(),
            trigger == SessionRecoveryTrigger::Scheduled,
        )
        .await
        {
            Ok(candidates) => candidates,
            Err(error) => {
                tracing::warn!(error = ?error, "Session recovery candidate scan failed");
                return SessionRecoveryCycle {
                    retryable_failures: 1,
                    settled: 0,
                    elapsed: cycle_started.elapsed(),
                };
            }
        };
        let scan_elapsed = cycle_started.elapsed();
        let resources = self.reconcile_resource_activations_from(&candidates).await;
        let resources_elapsed = cycle_started.elapsed();
        let resource_failure_count = resources.failures.len();
        let pending = resources.pending;
        let quarantined = resources.quarantined.len();
        let settled_resources = resources.settled.len();
        for failure in resources.failures {
            tracing::warn!(
                session = %failure.session_id,
                error = %failure.message,
                "Session Resource reconciliation remains pending"
            );
        }
        if !resources.settled.is_empty() {
            tracing::info!(
                reconciled_resources = resources.settled.len(),
                "reconciled durable Session Resource activations"
            );
        }
        for isolation in resources.quarantined {
            tracing::error!(
                session = %isolation.session_id,
                reason = %isolation.reason,
                "corrupt durable Session remains quarantined"
            );
        }
        let continuation_failure_count = self
            .reconcile_environment_continuations_from(&candidates, self.clock.now_ms())
            .await;
        let continuations_elapsed = cycle_started.elapsed();
        let realizations = self.reconcile_session_realizations_from(&candidates).await;
        let realizations_elapsed = cycle_started.elapsed();
        let realization_failure_count = realizations.failures.len();
        let settled_realizations = realizations.settled.len();
        for failure in realizations.failures {
            tracing::warn!(
                session = %failure.session_id,
                error = %failure.message,
                "Session realization reconciliation remains pending"
            );
        }
        if !realizations.settled.is_empty() {
            tracing::info!(
                reconciled_realizations = realizations.settled.len(),
                "reconciled durable Session Runtime projections"
            );
        }
        // Event reconciliation may enter the complete Runtime Run state machine.
        // Start that phase at a scheduler boundary instead of nesting it below
        // Resource, continuation, and realization recovery. The `JoinSet` keeps
        // the child structurally owned and aborts it if this recovery cycle is
        // cancelled; only its polling stack changes.
        let application = std::sync::Arc::clone(&self);
        let mut event_task = tokio::task::JoinSet::new();
        let event_candidates = candidates.clone();
        event_task.spawn(async move {
            application
                .reconcile_event_batches_from(&event_candidates)
                .await
        });
        let event_batches = match event_task.join_next().await {
            Some(Ok(report)) => report,
            Some(Err(error)) => {
                let mut report = crate::event_batches::EventBatchReconciliation::default();
                report.failures.push((
                    "<supervisor>".to_string(),
                    format!("Session Event recovery task failed: {error}"),
                ));
                report
            }
            None => {
                let mut report = crate::event_batches::EventBatchReconciliation::default();
                report.failures.push((
                    "<supervisor>".to_string(),
                    "Session Event recovery task disappeared".to_string(),
                ));
                report
            }
        };
        let event_batches_elapsed = cycle_started.elapsed();
        let event_batch_failure_count = event_batches.failures.len();
        let event_batches_settled = event_batches.settled;
        for (session_id, error) in event_batches.failures {
            tracing::warn!(
                session = %session_id,
                %error,
                "Session Event reconciliation remains pending"
            );
        }
        if event_batches.settled != 0 {
            tracing::info!(
                reconciled_event_batches = event_batches.settled,
                "reconciled durable Session Event batches"
            );
        }
        let outcomes = self.reconcile_outcome_continuations_from(&candidates).await;
        let outcomes_elapsed = cycle_started.elapsed();
        let outcome_failure_count = outcomes.failures.len();
        let outcomes_settled = outcomes.settled;
        for (session_id, error) in outcomes.failures {
            tracing::warn!(
                session = %session_id,
                %error,
                "Thread Outcome reconciliation remains pending"
            );
        }
        if outcomes.settled != 0 {
            tracing::info!(
                reconciled_outcomes = outcomes.settled,
                "reconciled durable Thread Outcomes"
            );
        }
        let final_scan_failure_count = match self
            .refresh_event_batch_cutover_validation(event_batch_failure_count)
            .await
        {
            Ok(snapshot) => {
                tracing::info!(
                    validation_generation = snapshot.generation,
                    terminal_with_incomplete_event_batches =
                        snapshot.terminal_with_incomplete_event_batches,
                    event_batch_failures = snapshot.event_batch_failures,
                    quarantined_sessions = snapshot.quarantined,
                    "Session Event-batch cutover validation scan completed"
                );
                0
            }
            Err(error) => {
                tracing::warn!(
                    error = ?error,
                    "Session Event-batch cutover validation scan remains pending"
                );
                1
            }
        };
        let retryable_failures = resource_failure_count
            + continuation_failure_count
            + realization_failure_count
            + event_batch_failure_count
            + outcome_failure_count
            + final_scan_failure_count;
        tracing::info!(
            pending_sessions = pending
                .max(realizations.pending)
                .max(event_batches.pending)
                .max(outcomes.pending),
            quarantined_sessions = quarantined
                .max(realizations.quarantined.len())
                .max(event_batches.quarantined)
                .max(outcomes.quarantined),
            retryable_failures,
            scan_ms = scan_elapsed.as_millis(),
            resources_ms = (resources_elapsed - scan_elapsed).as_millis(),
            continuations_ms = (continuations_elapsed - resources_elapsed).as_millis(),
            realizations_ms = (realizations_elapsed - continuations_elapsed).as_millis(),
            event_batches_ms = (event_batches_elapsed - realizations_elapsed).as_millis(),
            outcomes_ms = (outcomes_elapsed - event_batches_elapsed).as_millis(),
            validation_ms = (cycle_started.elapsed() - outcomes_elapsed).as_millis(),
            elapsed_ms = cycle_started.elapsed().as_millis(),
            "Session recovery scan completed"
        );
        SessionRecoveryCycle {
            retryable_failures,
            settled: settled_resources
                + settled_realizations
                + event_batches_settled
                + outcomes_settled,
            elapsed: cycle_started.elapsed(),
        }
    }

    /// Run the sole Session lifecycle supervisor for this application instance.
    /// The process startup owns spawning, cancellation, readiness, and join;
    /// this application owns only the convergence sequence and one-shot fence.
    pub async fn run_lifecycle_supervisor(
        self: std::sync::Arc<Self>,
        cancellation: awaken_runtime_contract::CancellationToken,
    ) -> Result<(), String> {
        if !self.claim_lifecycle_supervisor() {
            return Err("Session lifecycle supervisor was already claimed".into());
        }
        // Observe cancellation during initialization and a whole recovery pass,
        // not only between passes. Dropping the loop also drops its owned
        // JoinSets; durable lease/physical-quiescence authority is unchanged.
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Ok(()),
            result = self.run_lifecycle_loop(cancellation.clone()) => result,
        }
    }

    async fn run_lifecycle_maintenance(&self) {
        if let Err(error) = self.renew_due_session_realizations().await {
            tracing::warn!(
                error = ?error,
                "Session realization lease renewal remains pending"
            );
        }
        let dispatch = self.reconcile_work_dispatches().await;
        for failure in dispatch.failures {
            tracing::warn!(
                session = %failure.session_id,
                environment = %failure.environment_id,
                error = %failure.message,
                "Session WorkQueue dispatch remains pending"
            );
        }
    }

    async fn run_lifecycle_loop(
        self: std::sync::Arc<Self>,
        cancellation: awaken_runtime_contract::CancellationToken,
    ) -> Result<(), String> {
        let mut recovery_failure_streak = 0_u32;
        let mut next_recovery = awaken_time::mono::now();
        let mut external_wake_cooldown = awaken_time::mono::now();
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        let initial_dispatch = self.reconcile_work_dispatches().await;
        for failure in initial_dispatch.failures {
            tracing::warn!(
                session = %failure.session_id,
                environment = %failure.environment_id,
                error = %failure.message,
                "Session WorkQueue dispatch remains pending"
            );
        }
        loop {
            let recovery_trigger = tokio::select! {
                () = cancellation.cancelled() => break,
                _ = self.lifecycle_wakeup.notified() => {
                    let delay = external_wake_coalescing_delay(
                        next_recovery.saturating_duration_since(awaken_time::mono::now()),
                        external_wake_cooldown
                            .saturating_duration_since(awaken_time::mono::now()),
                    );
                    tokio::select! {
                        biased;
                        () = cancellation.cancelled() => break,
                        () = tokio::time::sleep(delay) => {}
                    }
                    Some(SessionRecoveryTrigger::ExternalWake)
                },
                _ = tokio::time::sleep_until(next_recovery) => Some(SessionRecoveryTrigger::Scheduled),
                _ = interval.tick() => {
                    self.run_lifecycle_maintenance().await;
                    None
                }
            };
            let Some(recovery_trigger) = recovery_trigger else {
                continue;
            };
            // The sole recovery cycle composes Resource, realization,
            // Event-batch, and Outcome reconciliation. Await it as a distinct
            // Tokio task so its first poll begins at the scheduler boundary;
            // nesting the complete state machine in this supervisor can exceed
            // a default worker stack before the first User batch is admitted.
            let application = std::sync::Arc::clone(&self);
            let mut recovery_task = tokio::task::JoinSet::new();
            recovery_task.spawn(async move {
                application
                    .reconcile_pending_session_state_for(recovery_trigger)
                    .await
            });
            let cycle = loop {
                tokio::select! {
                    biased;
                    result = recovery_task.join_next() => {
                        break result
                            .ok_or_else(|| "Session recovery task disappeared".to_string())?
                            .map_err(|error| format!("Session recovery task failed: {error}"))?;
                    }
                    _ = interval.tick() => self.run_lifecycle_maintenance().await,
                }
            };
            recovery_failure_streak = if cycle.retryable_failures == 0 {
                0
            } else {
                recovery_failure_streak.saturating_add(1)
            };
            let now = awaken_time::mono::now();
            external_wake_cooldown =
                now + next_external_wake_cooldown(recovery_trigger, cycle.settled, cycle.elapsed);
            let existing_remaining = next_recovery.saturating_duration_since(now);
            next_recovery = now
                + next_session_recovery_delay(
                    recovery_trigger,
                    existing_remaining,
                    recovery_failure_streak,
                );
        }
        Ok(())
    }

    /// Install frozen dispatch facts without acquiring a local realization
    /// lease. Worker-owned placement crosses this seam before enqueue only; the
    /// claimed Worker remains the sole owner of physical realization effects.
    pub async fn install_dispatch_projection(
        &self,
        owner_scope: &str,
        session: &PersistedSession,
    ) -> Result<(), SessionRealizationError> {
        let realization_runtime = self.runtime().realization();

        let projection = self
            .frozen_session_projection(owner_scope.to_string(), session, true)
            .await
            .map_err(|error| {
                SessionRealizationError::Effect(RunError::internal(error.to_string()))
            })?;
        realization_runtime
            .install_session_projection(
                session.session_id.as_str(),
                projection,
                awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch,
            )
            .await
            .map_err(SessionRealizationError::Effect)
    }

    /// Drive one local Session through the canonical durable realization phase
    /// protocol. Protocol adapters may project the committed result but cannot
    /// perform or reorder these effects.
    pub async fn realize_session(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<PersistedSession, SessionRealizationError> {
        let admission = self
            .refresh_executable_projections()
            .await
            .map_err(|error| SessionRealizationError::Control(unavailable(error)))?;
        admission
            .enter(|| self.realize_session_after_refresh(session_id))
            .await
    }

    /// Continue realization after the owning application operation refreshed
    /// every executable projection before its first catalog read or mutation.
    pub(super) async fn realize_session_after_refresh(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<PersistedSession, SessionRealizationError> {
        let realization_runtime = self.runtime().realization();

        // One co-located phase driver per Session. The request path and the
        // recovery sweep share this process; letting both drive interleaves
        // their phases and repeats the same external MCP effect. The permit is
        // the only way to reach `drive_session_realization` below.
        let permit = self
            .local_realization_locks()
            .acquire(session_id.as_str())
            .await;
        let request_started = realization_runtime
            .observe_realization_request()
            .map_err(SessionRealizationError::Effect)?;
        let lease_expires_at_unix_ms = self
            .clock
            .now_ms()
            .checked_add(LOCAL_SESSION_REALIZATION_LEASE_MS)
            .ok_or_else(|| {
                SessionRealizationError::Effect(RunError::internal("lease expiry overflow"))
            })?;
        let directive = self
            .begin_session_realization_after_refresh(BeginSessionRealization {
                expected_resources: None,
                session_id: session_id.clone(),
                target: awaken_session_realization_contract::SessionRealizationTarget {
                    owner: self.local_realization_owner().to_string(),
                    runtime_incarnation: self.runtime_incarnation().to_string(),
                    lease_expires_at_unix_ms,
                    reassign_existing_lease: false,
                },
            })
            .await
            .map_err(SessionRealizationError::Control)?;
        self.drive_local_realization(&permit, directive, request_started)
            .await?;
        self.session_repository()
            .get(session_id)
            .await
            .map_err(repository_control)
            .map_err(SessionRealizationError::Control)
    }

    async fn drive_local_realization(
        &self,
        permit: &awaken_session_realization_contract::SessionRealizationPermit,
        directive: SessionRealizationDirective,
        request_started: awaken_lease_contract::authority::AuthorityProofStarted,
    ) -> Result<(), SessionRealizationError> {
        let control = RefreshedSessionRealizationControl(self);
        crate::drive_session_realization(
            permit,
            None,
            &control,
            &LocalProjectionSynchronizer {
                runtime: self.runtime().realization(),
            },
            self.mcp_realizer(),
            directive,
            request_started,
        )
        .await
        .map_err(|error| match error {
            awaken_session_realization_contract::SessionRealizationDriveError::Effect(error) => {
                SessionRealizationError::Effect(error)
            }
            awaken_session_realization_contract::SessionRealizationDriveError::Control(error) => {
                SessionRealizationError::Control(error)
            }
            awaken_session_realization_contract::SessionRealizationDriveError::DidNotConverge => {
                SessionRealizationError::DidNotConverge
            }
        })
    }

    /// Renew due local projections through the lease-only root-CAS command.
    pub async fn renew_due_session_realizations(&self) -> Result<usize, SessionRealizationError> {
        let realization_runtime = self.runtime().realization();

        realization_runtime
            .renew_session_realization_projections(
                &RefreshedSessionRealizationControl(self),
                self.local_realization_owner(),
                self.runtime_incarnation(),
                awaken_lease_contract::authority::AuthorityLeaseTiming::from_ttl_ms(
                    LOCAL_SESSION_REALIZATION_LEASE_MS,
                ),
            )
            .await
            .map_err(SessionRealizationError::Effect)
    }

    /// Recover the one local Runtime projection for every Session that lost its
    /// process incarnation or has unfinished MCP work. This is deliberately the
    /// same canonical realization driver used by create/update, not a restart-only
    /// environment or MCP path. Terminal and Worker-owned Sessions remain untouched.
    pub async fn reconcile_session_realizations(&self) -> SessionReconciliation {
        let candidates =
            match super::scan_recovery_candidates(self.session_repository(), true).await {
                Ok(candidates) => candidates,
                Err(error) => {
                    let mut report = SessionReconciliation::default();
                    report.failures.push(SessionReconciliationFailure {
                        session_id: "<repository>".to_string(),
                        message: error.to_string(),
                    });
                    return report;
                }
            };
        self.reconcile_session_realizations_from(&candidates).await
    }

    pub(super) async fn reconcile_session_realizations_from(
        &self,
        candidates: &super::SessionRecoveryCandidates,
    ) -> SessionReconciliation {
        let mut report = SessionReconciliation {
            pending: candidates.sessions.len(),
            quarantined: candidates.quarantined.clone(),
            ..Default::default()
        };
        let realization_candidates = candidates
            .sessions
            .iter()
            .filter(|candidate| {
                candidate.interests.local_realization || candidate.interests.dispatch_projection
            })
            .collect::<Vec<_>>();
        if realization_candidates.is_empty() {
            return report;
        }
        let admission = match self.refresh_executable_projections().await {
            Ok(admission) => admission,
            Err(error) => {
                report.failures.push(SessionReconciliationFailure {
                    session_id: "<executable-projections>".to_string(),
                    message: error,
                });
                return report;
            }
        };
        for candidate in realization_candidates {
            let session = match admission
                .enter(|| self.session_repository().get(&candidate.session_id))
                .await
            {
                Ok(session) => match DurableSessionRoot::from_recovery(session) {
                    Ok(root) => root,
                    Err(message) => {
                        report.failures.push(SessionReconciliationFailure {
                            session_id: candidate.session_id.to_string(),
                            message: message.into(),
                        });
                        continue;
                    }
                },
                Err(awaken_session_state_contract::SessionRepositoryError::NotFound) => continue,
                Err(error) => {
                    report.failures.push(SessionReconciliationFailure {
                        session_id: candidate.session_id.to_string(),
                        message: error.to_string(),
                    });
                    continue;
                }
            };
            let session = session.as_session();
            let now = self.clock.now_ms();
            let projection_is_current = session.realization.as_ref().is_some_and(|lease| {
                lease.owner == self.local_realization_owner()
                    && lease.runtime_incarnation == self.runtime_incarnation()
                    && awaken_session_realization_contract::realization_lease_is_live_at(
                        lease.expires_at_unix_ms,
                        now,
                    )
            });
            if session.is_terminal() {
                continue;
            }
            if self.requires_external_realization(session) {
                // Worker placement keeps the durable Session as authority and
                // the Coordinator projection as a disposable cache. Creation
                // can commit the root before projection I/O fails or its reply
                // is lost; rebuild that same projection while initial
                // realization remains unfinished. Resident/idle Sessions do
                // not need periodic reinstall, and live effects remain solely
                // Worker-owned.
                if session.needs_initial_realization_recovery() {
                    match self
                        .install_dispatch_projection(&candidate.workspace_id, session)
                        .await
                    {
                        Ok(()) => report.settled.push(session.clone()),
                        Err(error) => report.failures.push(SessionReconciliationFailure {
                            session_id: candidate.session_id.to_string(),
                            message: error.to_string(),
                        }),
                    }
                }
                continue;
            }
            if !environment_admits_realization_effects(&session.environment) {
                continue;
            }
            // Recovery cause/effect decision table:
            // C1 local nonterminal Session; C2 durable lease names this process;
            // C3 lease is live; C4 MCP has unfinished work. E1 a missing/stale
            // process projection is reassigned and rebuilt once; E2 pending MCP
            // is driven by that same phase protocol; E3 an already-current idle
            // projection is a no-op; E4 terminal/remote placement is untouched.
            //
            // | Rule | C1 | C2+C3 | C4 | Effect |
            // | R1 | yes | no | any | E1 (+E2 when pending) |
            // | R2 | yes | yes | yes | E2 |
            // | R3 | yes | yes | no | E3 |
            // | R4 | no | any | any | E4 |
            if projection_is_current && !session.mcp.needs_reconciliation() {
                continue;
            }
            let session_id = session.session_id.clone();
            match self.realize_session_after_refresh(&session_id).await {
                Ok(session) => report.settled.push(session),
                Err(error) => report.failures.push(SessionReconciliationFailure {
                    session_id: session_id.into_string(),
                    message: error.to_string(),
                }),
            }
        }
        report
    }

    fn realization_stage_requests(
        owner_scope: &str,
        session: &PersistedSession,
    ) -> Result<Vec<StageMcpAttachment>, SessionRealizationControlFailure> {
        session
            .mcp
            .attachments
            .iter()
            .filter(|attachment| {
                attachment.state == McpAttachmentState::Realizing
                    || (attachment.state == McpAttachmentState::Active
                        && !attachment.publication_acknowledged)
            })
            .map(|attachment| {
                projection::stage_mcp_request(owner_scope, &session.session_id, attachment)
                    .map_err(unavailable)
            })
            .collect()
    }

    async fn realization_directive(
        &self,
        owner_scope: String,
        session: &PersistedSession,
        action: SessionRealizationAction,
        activate_pending_resources: bool,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        // Context is a lightweight rebuildable projection and must refresh even
        // when an existing realization lease needs no Environment/Resource
        // Stage. Otherwise a durable System command accepted between Runs would
        // remain invisible on a warm local or remote Worker.
        let materialize_request_context = true;
        let projection = if activate_pending_resources {
            self.frozen_session_projection(owner_scope, session, materialize_request_context)
                .await
        } else {
            self.active_frozen_session_projection(owner_scope, session, materialize_request_context)
                .await
        }
        .map_err(|error| unavailable(error.to_string()))?;
        Ok(SessionRealizationDirective {
            projection,
            authority:
                awaken_session_realization_contract::SessionRealizationLeaseReceipt::issued_at(
                    session
                        .realization
                        .clone()
                        .ok_or(SessionRealizationControlFailure::NotReady)?,
                    self.clock.now_ms(),
                )?,
            action,
        })
    }

    async fn next_action(
        &self,
        owner_scope: String,
        session: &PersistedSession,
        prepare_projection: bool,
        activate_pending_resources: bool,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        let stages = Self::realization_stage_requests(&owner_scope, session)?;
        // A new runtime incarnation has no process-local baseline even when the
        // Session has zero Resources/MCP. Synchronize the complete frozen
        // projection on assignment; pending Resources independently require the
        // same idempotent synchronization before their external effects.
        let prepare_session = prepare_projection
            || (activate_pending_resources && session.resources.pending.is_some());
        if prepare_session || !stages.is_empty() {
            return self
                .realization_directive(
                    owner_scope,
                    session,
                    SessionRealizationAction::Stage {
                        prepare_session,
                        mcp_stages: stages,
                    },
                    activate_pending_resources,
                )
                .await;
        }
        let publish = publication_generations(&session.session_id, &session.mcp.attachments)?;
        let drain = draining_generations(&session.session_id, &session.mcp.attachments)?;
        let action = if publish.is_empty() && drain.is_empty() {
            SessionRealizationAction::Complete
        } else {
            SessionRealizationAction::Publish { publish, drain }
        };
        self.realization_directive(owner_scope, session, action, activate_pending_resources)
            .await
    }

    async fn session_for_realization_without_credential_migration(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<(String, PersistedSession), SessionRealizationControlFailure> {
        let owner_scope = self.owner(session_id).await.map_err(mutation_control)?;
        let session = self
            .session_repository()
            .get(session_id)
            .await
            .map_err(repository_control)?;
        // A failed initial realization is terminal. Treating its now-empty
        // Stage/Publish sets as `Complete` would allow a retried Run claim to
        // execute after silently dropping the failed MCP/Resource projection.
        if session.is_terminal() {
            return Err(SessionRealizationControlFailure::Terminal);
        }
        if session.frozen_baseline().is_none() {
            return Err(SessionRealizationControlFailure::NotReady);
        }
        if !environment_admits_realization_effects(&session.environment) {
            return Err(SessionRealizationControlFailure::NotReady);
        }
        Ok((owner_scope, session))
    }

    async fn session_for_realization(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<(String, PersistedSession), SessionRealizationControlFailure> {
        let (owner_scope, session) = self
            .session_for_realization_without_credential_migration(session_id)
            .await?;
        let session = self
            .ensure_repository_credentials_pinned(&owner_scope, session)
            .await
            .map_err(unavailable)?;
        Ok((owner_scope, session))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionRecoveryCycle {
    pub(crate) retryable_failures: usize,
    /// How many durable items this scan actually moved forward. A scan that
    /// settled nothing is the only one whose successor hint is plausibly
    /// redundant, so this is what arms the external-wake coalescing window.
    pub(crate) settled: usize,
    /// What this scan cost. An empty hint-driven scan is charged its own
    /// duration as the coalescing window, so scanning cannot exceed half the
    /// supervisor's time and a later hint never inherits more delay than a
    /// scan actually takes.
    pub(crate) elapsed: std::time::Duration,
}
