//! The Worker-owned Session realization read.
//!
//! One complete, fail-closed read of the Sessions whose realization lease a
//! Worker holds decides whether that Worker is still occupied and, for an
//! exact retirement read, whether every original source is retained without
//! effects. It changes no lease, projection or cleanup receipt.

use std::sync::Arc;

use awaken_session_repository_contract::SessionRepository;
use awaken_session_state_contract::{
    SessionEnvironmentState, SessionExecutionState, SessionTerminalCleanupAction,
};
use awaken_session_types::RunError;

use crate::session_repository_error;

/// The Worker occupancy and retirement read over durable Session roots.
///
/// It holds the Session repository the owner read scans, the query component
/// that freezes a retained Session's projection, and the Runtime source-dispatch
/// and terminal-cleanup retention observations the qualification consults.
pub struct SessionWorkerOccupancy {
    repository: Arc<dyn SessionRepository>,
    queries: Arc<crate::SessionQueries>,
    coordination_reads: Arc<dyn awaken_session_execution_contract::SessionCoordinationReads>,
    terminal_cleanup: Arc<dyn awaken_session_execution_contract::TerminalCleanupRuntime>,
}

impl SessionWorkerOccupancy {
    pub(crate) fn new(
        repository: Arc<dyn SessionRepository>,
        queries: Arc<crate::SessionQueries>,
        runtime: &crate::SessionExecutor,
    ) -> Self {
        Self {
            repository,
            queries,
            coordination_reads: runtime.coordination_reads.clone(),
            terminal_cleanup: runtime.terminal_cleanup.clone(),
        }
    }

    /// One complete Worker-owned Session read. Normal occupancy never depends
    /// on provider availability. Explicit retirement preserves occupancy and
    /// additionally qualifies each original source (terminal or live Resident) without effects,
    /// lease changes, installed projections or cleanup receipts.
    pub async fn observe_worker_realizations(
        &self,
        read: crate::WorkerRealizationRead<'_>,
    ) -> Result<crate::WorkerRealizationObservation, RunError> {
        let (worker_id, identity) = match read {
            crate::WorkerRealizationRead::Occupancy(worker_id) => (worker_id, None),
            crate::WorkerRealizationRead::Retirement(identity) => {
                (identity.worker_id.as_str(), Some(identity))
            }
        };
        if worker_id.trim().is_empty() {
            return Err(RunError::bad_request("Worker realization owner is empty"));
        }
        if identity.is_some_and(|identity| {
            identity.incarnation_id.trim().is_empty() || identity.generation == 0
        }) {
            return Err(RunError::bad_request(
                "exact Worker realization identity is required",
            ));
        }
        let sessions =
            crate::recovery_scan::worker_realizations(self.repository.as_ref(), worker_id)
                .await
                .map_err(session_repository_error)?;
        let mut occupied = false;
        let blocked = crate::WorkerRealizationObservation {
            occupied: true,
            sources_retained: false,
        };
        for scoped in sessions {
            let session = scoped.session;
            if !session
                .has_unfinished_worker_realization()
                .map_err(|error| RunError::unavailable(error.to_string()))?
            {
                continue;
            }
            occupied = true;
            let Some(identity) = identity else {
                return Ok(blocked);
            };
            let lease = session
                .realization
                .as_ref()
                .ok_or_else(|| RunError::unavailable("Worker realization owner missing"))?;
            if lease.runtime_incarnation != identity.lease_owner()
                || session.has_incomplete_event_batches()
            {
                return Ok(blocked);
            }
            let prepared = if session.is_terminal() {
                let Some(action) = session
                    .terminal_cleanup_work_action()
                    .map_err(|error| RunError::unavailable(error.to_string()))?
                else {
                    return Ok(blocked);
                };
                if matches!(action, SessionTerminalCleanupAction::Waiting) {
                    return Ok(blocked);
                }
                if awaken_session_state_contract::session_has_terminal_no_create_evidence(&session)
                    || crate::archive_intent::unattempted_profiled_archive_witness(
                        self.repository.as_ref(),
                        &scoped.workspace_id,
                        &session,
                    )
                    .await
                    .map_err(session_repository_error)?
                {
                    continue;
                }
                if !matches!(
                    self.coordination_reads
                        .source_dispatch_observation(&session)
                        .await,
                    awaken_session_execution_contract::SessionSourceDispatchObservation::Settled
                ) {
                    return Ok(blocked);
                }
                matches!(action, SessionTerminalCleanupAction::Dispose { .. })
            } else {
                // Data retention permits Actor replacement, never completion
                // of this live Session or its original Run. Registry/physical/
                // Work admission remains the Worker-domain owner's prerequisite.
                // is_terminal already covers every disposition denying activity.
                if session.frozen_baseline().is_none()
                    || !matches!(
                        session.execution,
                        SessionExecutionState::Idle
                            | SessionExecutionState::Running
                            | SessionExecutionState::Rescheduling
                    )
                    || !matches!(
                        session.environment,
                        SessionEnvironmentState::Resident { .. }
                    )
                {
                    return Ok(blocked);
                }
                false
            };
            let projection = self
                .queries
                .frozen_session_projection(scoped.workspace_id, &session, false)
                .await?;
            if !self
                .terminal_cleanup
                .observe_terminal_cleanup_retention(&session.session_id, &projection, prepared)
                .await?
            {
                return Ok(blocked);
            }
        }
        Ok(crate::WorkerRealizationObservation {
            occupied,
            sources_retained: identity.is_some(),
        })
    }
}
