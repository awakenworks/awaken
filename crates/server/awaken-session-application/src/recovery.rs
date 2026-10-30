// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Static owner: the sole Session recovery cycle and its one process lifecycle supervisor.
// @formal reason: Dynamic path: one canonical EOF-proven scan feeds Resource, continuation, realization,
// @formal reason: Event-batch and Outcome reconciliation in order; each phase reloads its root and owns
// @formal reason: CAS, fencing and settlement, and the cutover projection publishes only after a complete
// @formal reason: final scan. Evidence: session.lifecycle.session_state plus the adjacent supervisor,
// @formal reason: cancellation and one-scan-per-cycle decision tables; this is a product-requirement
// @formal reason: classification, not a direct formal-proof claim.
//! The Session recovery cycle and the one lifecycle supervisor that runs it.
//!
//! One cycle reads the canonical recovery scan once and hands the same
//! candidates to Resource, Environment continuation, realization, Event-batch
//! and Outcome reconciliation in that order, then publishes the Event-batch
//! cutover projection from an independent final scan. Every phase reloads its
//! own roots and owns its decisions; this component orders the phases, counts
//! what they settled or left retryable, and schedules the next cycle.

mod backoff;
mod lifecycle_supervisor;

use std::sync::Arc;

use awaken_session_contract::SessionRepository;
use awaken_time::WallClockExt as _;

use crate::event_batches::EventBatchReconciliation;
use crate::outcome_reconciliation::OutcomeReconciliation;
use crate::recovery_scan::ContinuationDiagnostics;
use crate::{SessionEventBatchCutoverValidationSource, SessionReconciliation};
use backoff::SessionRecoveryTrigger;

/// The Session recovery cycle and its lifecycle supervisor.
///
/// It holds the repository the recovery scan reads, the clock continuation
/// hints are decided at, the lifecycle wake and one-shot supervisor fence, the
/// reconciling components in phase order, the Work and lease maintenance the
/// supervisor runs at its cadence, and the cutover projection it publishes.
pub struct SessionRecovery {
    repository: Arc<dyn SessionRepository>,
    clock: Arc<dyn awaken_time::WallClock>,
    lifecycle: Arc<crate::SessionLifecycle>,
    work_assignments: Arc<crate::SessionWorkAssignments>,
    resource_reconciliation: Arc<crate::SessionResourceReconciliation>,
    continuations: Arc<crate::SessionContinuations>,
    realizations: Arc<crate::SessionRealizations>,
    event_batches: Arc<crate::SessionEventBatchCommands>,
    outcome_reconciliation: Arc<crate::SessionOutcomeReconciliation>,
    event_batch_cutover_validation: Arc<SessionEventBatchCutoverValidationSource>,
}

/// The collaborators [`SessionRecovery`] is constructed from.
pub(crate) struct SessionRecoveryCollaborators {
    pub(crate) repository: Arc<dyn SessionRepository>,
    pub(crate) clock: Arc<dyn awaken_time::WallClock>,
    pub(crate) lifecycle: Arc<crate::SessionLifecycle>,
    pub(crate) work_assignments: Arc<crate::SessionWorkAssignments>,
    pub(crate) resource_reconciliation: Arc<crate::SessionResourceReconciliation>,
    pub(crate) continuations: Arc<crate::SessionContinuations>,
    pub(crate) realizations: Arc<crate::SessionRealizations>,
    pub(crate) event_batches: Arc<crate::SessionEventBatchCommands>,
    pub(crate) outcome_reconciliation: Arc<crate::SessionOutcomeReconciliation>,
    pub(crate) event_batch_cutover_validation: Arc<SessionEventBatchCutoverValidationSource>,
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

/// What one reconciliation phase contributes to the cycle report.
#[derive(Clone, Copy, Debug, Default)]
struct PhaseReport {
    failures: usize,
    settled: usize,
    pending: usize,
    quarantined: usize,
}

impl PhaseReport {
    fn resources(report: SessionReconciliation) -> Self {
        for failure in &report.failures {
            tracing::warn!(
                session = %failure.session_id,
                error = %failure.message,
                "Session Resource reconciliation remains pending"
            );
        }
        if !report.settled.is_empty() {
            tracing::info!(
                reconciled_resources = report.settled.len(),
                "reconciled durable Session Resource activations"
            );
        }
        for isolation in &report.quarantined {
            tracing::error!(
                session = %isolation.session_id,
                reason = %isolation.reason,
                "corrupt durable Session remains quarantined"
            );
        }
        Self {
            failures: report.failures.len(),
            settled: report.settled.len(),
            pending: report.pending,
            quarantined: report.quarantined.len(),
        }
    }

    fn realizations(report: SessionReconciliation) -> Self {
        for failure in &report.failures {
            tracing::warn!(
                session = %failure.session_id,
                error = %failure.message,
                "Session realization reconciliation remains pending"
            );
        }
        if !report.settled.is_empty() {
            tracing::info!(
                reconciled_realizations = report.settled.len(),
                "reconciled durable Session Runtime projections"
            );
        }
        Self {
            failures: report.failures.len(),
            settled: report.settled.len(),
            pending: report.pending,
            quarantined: report.quarantined.len(),
        }
    }

    fn event_batches(report: EventBatchReconciliation) -> Self {
        for (session_id, error) in &report.failures {
            tracing::warn!(
                session = %session_id,
                %error,
                "Session Event reconciliation remains pending"
            );
        }
        if report.settled != 0 {
            tracing::info!(
                reconciled_event_batches = report.settled,
                "reconciled durable Session Event batches"
            );
        }
        Self {
            failures: report.failures.len(),
            settled: report.settled,
            pending: report.pending,
            quarantined: report.quarantined,
        }
    }

    fn outcomes(report: OutcomeReconciliation) -> Self {
        for (session_id, error) in &report.failures {
            tracing::warn!(
                session = %session_id,
                %error,
                "Thread Outcome reconciliation remains pending"
            );
        }
        if report.settled != 0 {
            tracing::info!(
                reconciled_outcomes = report.settled,
                "reconciled durable Thread Outcomes"
            );
        }
        Self {
            failures: report.failures.len(),
            settled: report.settled,
            pending: report.pending,
            quarantined: report.quarantined,
        }
    }
}

impl SessionRecovery {
    pub(crate) fn new(collaborators: SessionRecoveryCollaborators) -> Self {
        let SessionRecoveryCollaborators {
            repository,
            clock,
            lifecycle,
            work_assignments,
            resource_reconciliation,
            continuations,
            realizations,
            event_batches,
            outcome_reconciliation,
            event_batch_cutover_validation,
        } = collaborators;
        Self {
            repository,
            clock,
            lifecycle,
            work_assignments,
            resource_reconciliation,
            continuations,
            realizations,
            event_batches,
            outcome_reconciliation,
            event_batch_cutover_validation,
        }
    }

    /// Return the one read-only Event-batch cutover source this recovery
    /// cycle publishes after each complete pass.
    #[must_use]
    pub fn session_event_batch_cutover_validation_source(
        &self,
    ) -> Arc<SessionEventBatchCutoverValidationSource> {
        self.event_batch_cutover_validation.clone()
    }

    /// Read the final recovery scan and the canonical global Environment
    /// count, then publish them. Either failure preserves the preceding
    /// generation.
    pub(crate) async fn refresh_event_batch_cutover_validation(
        &self,
        event_batch_failures: usize,
    ) -> Result<
        crate::SessionEventBatchCutoverValidationSnapshot,
        awaken_session_contract::SessionRepositoryError,
    > {
        self.event_batch_cutover_validation
            .refresh(self.repository.as_ref(), event_batch_failures)
            .await
    }

    #[cfg(test)]
    pub(crate) async fn reconcile_pending_session_state(self: Arc<Self>) -> SessionRecoveryCycle {
        self.reconcile_pending_session_state_for(
            SessionRecoveryTrigger::Scheduled,
            &mut ContinuationDiagnostics::default(),
        )
        .await
    }

    pub(super) async fn reconcile_pending_session_state_for(
        self: Arc<Self>,
        trigger: SessionRecoveryTrigger,
        diagnostics: &mut ContinuationDiagnostics,
    ) -> SessionRecoveryCycle {
        let cycle_started = awaken_time::mono::Stopwatch::start();
        let candidates = match diagnostics
            .scan(
                self.repository.as_ref(),
                self.clock.as_ref(),
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
        // EOF is proven before any diagnostic is consumed. The supervisor
        // owns this value across sequential cycles, so an older concurrent
        // status/phase scan cannot overwrite a newer diagnostic view.
        let scan_elapsed = cycle_started.elapsed();
        let resources = PhaseReport::resources(
            self.resource_reconciliation
                .reconcile_resource_activations_from(&candidates)
                .await,
        );
        let resources_elapsed = cycle_started.elapsed();
        let continuation_failures = self
            .continuations
            .reconcile_environment_continuations_from(&candidates, self.clock.now_ms())
            .await;
        let continuations_elapsed = cycle_started.elapsed();
        let realizations = PhaseReport::realizations(
            self.realizations
                .reconcile_session_realizations_from(&candidates)
                .await,
        );
        let realizations_elapsed = cycle_started.elapsed();
        let event_batches = PhaseReport::event_batches(
            lifecycle_supervisor::reconcile_event_batch_recovery_task(
                &self.event_batches,
                &candidates,
            )
            .await,
        );
        let event_batches_elapsed = cycle_started.elapsed();
        let outcomes =
            PhaseReport::outcomes(self.outcome_reconciliation.continue_from(&candidates).await);
        let outcomes_elapsed = cycle_started.elapsed();
        let validation_failures = self
            .publish_cutover_validation(event_batches.failures)
            .await;
        let phases = [resources, realizations, event_batches, outcomes];
        let retryable_failures = continuation_failures
            + validation_failures
            + phases.iter().map(|phase| phase.failures).sum::<usize>();
        tracing::info!(
            pending_sessions = phases.iter().map(|phase| phase.pending).max().unwrap_or(0),
            quarantined_sessions = phases
                .iter()
                .map(|phase| phase.quarantined)
                .max()
                .unwrap_or(0),
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
            settled: phases.iter().map(|phase| phase.settled).sum(),
            elapsed: cycle_started.elapsed(),
        }
    }

    /// Publish the Event-batch cutover projection after a complete cycle and
    /// count a failed final scan as one retryable failure.
    async fn publish_cutover_validation(&self, event_batch_failures: usize) -> usize {
        match self
            .refresh_event_batch_cutover_validation(event_batch_failures)
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
        }
    }

    /// Run the sole Session lifecycle supervisor for this application instance.
    /// The process startup owns spawning, cancellation, readiness, and join;
    /// this component owns only the convergence sequence and one-shot fence.
    pub async fn run_lifecycle_supervisor(
        self: Arc<Self>,
        cancellation: awaken_runtime_contract::CancellationToken,
    ) -> Result<(), String> {
        if !self.lifecycle.claim_supervisor() {
            return Err("Session lifecycle supervisor was already claimed".into());
        }
        // Observe cancellation during initialization and a whole recovery pass,
        // not only between passes. Dropping the loop also drops its owned
        // JoinSets; durable lease/physical-quiescence authority is unchanged.
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Ok(()),
            result = lifecycle_supervisor::run_lifecycle_loop(self, cancellation.clone()) => result,
        }
    }
}
