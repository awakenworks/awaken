// @formal surface requirement=session.lifecycle.session_state
// @formal reason: This complete root read consumes positive no-Create history or Worker source reports;
// @formal reason: the Worker-domain owner separately gates physical/Work admission. No root or receipt is written.
//! The Worker-owned Session realization read.
//!
//! One complete, fail-closed read of the Sessions whose realization lease a
//! Worker holds decides whether that Worker is still occupied and, for an
//! exact retirement read, whether every original source the Worker must
//! retain was reported retained by that Worker. It changes no lease,
//! projection or cleanup receipt, and observes no Sandbox: retention is
//! Worker-reported data ([`SourceRetentionSubject`] out, a
//! `WorkerSourceRetention` fact per subject back in the heartbeat, decided by
//! [`SourceRetentionSubject::reported_retained`]).

use std::sync::Arc;

use awaken_session_realization_contract::{SourceRetentionSubject, WorkerSourceRetentionSubjects};
use awaken_session_repository_contract::SessionRepository;
use awaken_session_state_contract::{
    SessionEnvironmentState, SessionExecutionState, SessionTerminalCleanupAction,
};
use awaken_session_types::RunError;
use awaken_worker_contract::WorkerIdentity;

use crate::session_repository_error;

/// The Worker occupancy and retirement read over durable Session roots.
///
/// It holds the Session repository the owner read scans, the query component
/// that freezes a retained Session's projection, and the source-dispatch
/// observation the qualification consults.
pub struct SessionWorkerOccupancy {
    repository: Arc<dyn SessionRepository>,
    queries: Arc<crate::SessionQueries>,
    coordination_reads: Arc<dyn awaken_session_execution_contract::SessionCoordinationReads>,
}

/// One complete Worker-owned Session read: `None` when a Session the Worker
/// holds keeps it occupied and refuses retirement on its root facts alone;
/// otherwise occupancy and the original sources whose reported retention
/// admits retirement.
type WorkerRealizationScan = Option<(bool, Vec<SourceRetentionSubject>)>;

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
        }
    }

    /// One complete Worker-owned Session read. Normal occupancy never depends
    /// on source retention. Explicit retirement preserves occupancy and
    /// additionally qualifies each original source (terminal or live Resident)
    /// from the retention the exact incarnation reported, without effects,
    /// lease changes, installed projections or cleanup receipts.
    pub async fn observe_worker_realizations(
        &self,
        read: crate::WorkerRealizationRead<'_>,
    ) -> Result<crate::WorkerRealizationObservation, RunError> {
        let (worker_id, retirement) = match read {
            crate::WorkerRealizationRead::Occupancy(worker_id) => (worker_id, None),
            crate::WorkerRealizationRead::Retirement { identity, reported } => {
                (identity.worker_id.as_str(), Some((identity, reported)))
            }
        };
        let scan = self.scan(worker_id, retirement.map(|(identity, _)| identity));
        let Some((occupied, subjects)) = scan.await? else {
            return Ok(crate::WorkerRealizationObservation {
                occupied: true,
                sources_retained: false,
            });
        };
        Ok(crate::WorkerRealizationObservation {
            occupied,
            sources_retained: retirement.is_some_and(|(_, reported)| {
                subjects
                    .iter()
                    .all(|subject| subject.reported_retained(reported))
            }),
        })
    }

    async fn scan(
        &self,
        worker_id: &str,
        identity: Option<&WorkerIdentity>,
    ) -> Result<WorkerRealizationScan, RunError> {
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
        let mut subjects = Vec::new();
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
                return Ok(None);
            };
            let lease = session
                .realization
                .as_ref()
                .ok_or_else(|| RunError::unavailable("Worker realization owner missing"))?;
            if lease.runtime_incarnation != identity.lease_owner()
                || session.has_incomplete_event_batches()
            {
                return Ok(None);
            }
            let prepared = if session.is_terminal() {
                let Some(action) = session
                    .terminal_cleanup_work_action()
                    .map_err(|error| RunError::unavailable(error.to_string()))?
                else {
                    return Ok(None);
                };
                if matches!(action, SessionTerminalCleanupAction::Waiting) {
                    return Ok(None);
                }
                if crate::archive_intent::terminal_environment_creation_not_attempted(
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
                    return Ok(None);
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
                {
                    return Ok(None);
                }
                if awaken_session_state_contract::session_has_no_create_evidence(&session)
                    && session.execution == SessionExecutionState::Idle
                    && !session.has_active_activities()
                    && session.running_interval.is_none()
                    && lease.expires_at_unix_ms == 0
                    && session.resources.pending.is_none()
                    && !session.resources.has_references()
                {
                    // Positive write-ahead history, not a missing binding. A
                    // closed assignment cannot admit Create, and the domain
                    // owner requires Registry drain and zero physical/Work
                    // admission before using this read. Keep occupancy/root
                    // ownership, but there is no original provider source.
                    // The terminal source-Dispatch port requires a frozen
                    // cleanup scope and cannot qualify this live root.
                    continue;
                }
                if !matches!(
                    session.environment,
                    SessionEnvironmentState::Resident { .. }
                ) {
                    return Ok(None);
                }
                false
            };
            let projection = self
                .queries
                .frozen_session_projection(scoped.workspace_id, &session, false)
                .await?;
            subjects.push(SourceRetentionSubject::new(
                session.session_id.clone(),
                prepared,
                projection,
            )?);
        }
        Ok(Some((occupied, subjects)))
    }
}

#[async_trait::async_trait]
impl WorkerSourceRetentionSubjects for SessionWorkerOccupancy {
    /// The same complete read the retirement decision makes, stopped before
    /// the decision. A blocked read names no subject: nothing the Worker
    /// could report would admit its retirement.
    async fn source_retention_subjects(
        &self,
        worker: &WorkerIdentity,
    ) -> Result<Vec<SourceRetentionSubject>, RunError> {
        let scan = self.scan(&worker.worker_id, Some(worker)).await?;
        Ok(scan.map(|(_, subjects)| subjects).unwrap_or_default())
    }
}
