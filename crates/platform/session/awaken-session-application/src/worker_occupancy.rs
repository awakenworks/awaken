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

use awaken_runtime_contract::runs::RunError;
use awaken_session_contract::SessionRepository;
use awaken_session_contract::{
    SessionEnvironmentState, SessionExecutionState, SessionTerminalCleanupAction,
};
use awaken_session_execution_contract::{SourceRetentionSubject, WorkerSourceRetentionSubjects};
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

/// Why one Session keeps an exact Worker incarnation from retiring. Each
/// variant is one refusal of the retirement read; none grants anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum WorkerRetirementBlockReason {
    /// The Session's lease belongs to another incarnation of this Worker.
    LeaseHeldByAnotherIncarnation,
    /// Committed Thread events are still being appended.
    IncompleteEventBatches,
    /// A terminal Session has no readable cleanup action.
    TerminalCleanupUnavailable,
    /// A terminal Session's cleanup is fenced or awaiting publication.
    TerminalCleanupWaiting,
    /// The terminal Session's source Dispatch has not settled.
    SourceDispatchUnsettled,
    /// A live Session without a frozen baseline or a retainable execution.
    LiveRootNotRetainable,
    /// A live Session whose Environment is not Resident.
    EnvironmentNotResident,
    /// The Worker has not reported this original source as retained.
    SourceRetentionNotReported,
}

/// One Session that refuses an exact Worker incarnation's retirement.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkerRetirementBlocker {
    pub session_id: String,
    pub reason: WorkerRetirementBlockReason,
}

/// One complete Worker-owned Session read: occupancy, the original sources
/// whose reported retention may admit retirement, and every Session that
/// refuses retirement on its root facts alone. Any blocker refuses; the read
/// continues past it only so that every refusal can be reported.
struct WorkerRealizationScan {
    occupied: bool,
    subjects: Vec<SourceRetentionSubject>,
    blockers: Vec<WorkerRetirementBlocker>,
}

impl SessionWorkerOccupancy {
    pub(crate) fn new(
        repository: Arc<dyn SessionRepository>,
        queries: Arc<crate::SessionQueries>,
        runtime: &crate::SessionExecutionRoles,
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
        let scan = self
            .scan(worker_id, retirement.map(|(identity, _)| identity))
            .await?;
        Ok(crate::WorkerRealizationObservation {
            occupied: scan.occupied,
            sources_retained: scan.blockers.is_empty()
                && retirement.is_some_and(|(_, reported)| {
                    scan.subjects
                        .iter()
                        .all(|subject| subject.reported_retained(reported))
                }),
        })
    }

    /// Every Session that refuses this exact incarnation's retirement, with
    /// its reason, from the same complete read the decision makes. Unreported
    /// sources are named only when no Session refuses on root facts. Empty means
    /// the Session read alone would permit retirement; the Worker-domain owner
    /// still decides Registry, physical and Work admission. Read-only.
    pub async fn explain_worker_retirement(
        &self,
        identity: &WorkerIdentity,
        reported: &std::collections::BTreeSet<awaken_worker_contract::WorkerSourceRetention>,
    ) -> Result<Vec<WorkerRetirementBlocker>, RunError> {
        let scan = self.scan(&identity.worker_id, Some(identity)).await?;
        let mut blockers = scan.blockers;
        if !blockers.is_empty() {
            // Root facts already refuse; source retention cannot decide, and
            // naming subjects would depend on which Sessions the read reached.
            blockers.sort();
            return Ok(blockers);
        }
        blockers.extend(
            scan.subjects
                .iter()
                .filter(|subject| !subject.reported_retained(reported))
                .map(|subject| WorkerRetirementBlocker {
                    session_id: subject.session_id.as_str().to_owned(),
                    reason: WorkerRetirementBlockReason::SourceRetentionNotReported,
                }),
        );
        blockers.sort();
        Ok(blockers)
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
        let mut blockers = Vec::new();
        for scoped in sessions {
            let session = scoped.session;
            match session.has_unfinished_worker_realization() {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) if !blockers.is_empty() => continue,
                Err(error) => return Err(RunError::unavailable(error.to_string())),
            }
            occupied = true;
            let Some(identity) = identity else {
                // An occupancy read needs only `occupied`.
                return Ok(WorkerRealizationScan {
                    occupied,
                    subjects: Vec::new(),
                    blockers,
                });
            };
            let lease = session
                .realization
                .as_ref()
                .ok_or_else(|| RunError::unavailable("Worker realization owner missing"))?;
            if lease.runtime_incarnation != identity.lease_owner() {
                blockers.push(blocker(
                    &session,
                    WorkerRetirementBlockReason::LeaseHeldByAnotherIncarnation,
                ));
                continue;
            }
            if session.has_incomplete_event_batches() {
                blockers.push(blocker(
                    &session,
                    WorkerRetirementBlockReason::IncompleteEventBatches,
                ));
                continue;
            }
            let prepared = if session.is_terminal() {
                let Some(action) = session
                    .terminal_cleanup_work_action()
                    .map_err(|error| RunError::unavailable(error.to_string()))?
                else {
                    blockers.push(blocker(
                        &session,
                        WorkerRetirementBlockReason::TerminalCleanupUnavailable,
                    ));
                    continue;
                };
                if matches!(action, SessionTerminalCleanupAction::Waiting) {
                    blockers.push(blocker(
                        &session,
                        WorkerRetirementBlockReason::TerminalCleanupWaiting,
                    ));
                    continue;
                }
                match crate::archive_intent::terminal_environment_creation_not_attempted(
                    self.repository.as_ref(),
                    &scoped.workspace_id,
                    &session,
                )
                .await
                {
                    Ok(true) => continue,
                    Ok(false) => {}
                    // A recorded refusal already decides; keep reporting the
                    // remaining root facts rather than turn it into an error.
                    Err(_) if !blockers.is_empty() => continue,
                    Err(error) => return Err(session_repository_error(error)),
                }
                if !matches!(
                    self.coordination_reads
                        .source_dispatch_observation(&session)
                        .await,
                    awaken_session_execution_contract::SessionSourceDispatchObservation::Settled
                ) {
                    blockers.push(blocker(
                        &session,
                        WorkerRetirementBlockReason::SourceDispatchUnsettled,
                    ));
                    continue;
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
                    blockers.push(blocker(
                        &session,
                        WorkerRetirementBlockReason::LiveRootNotRetainable,
                    ));
                    continue;
                }
                if awaken_session_contract::session_has_no_create_evidence(&session)
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
                    blockers.push(blocker(
                        &session,
                        WorkerRetirementBlockReason::EnvironmentNotResident,
                    ));
                    continue;
                }
                false
            };
            if !blockers.is_empty() {
                // Refused already: a projection could not change the decision.
                continue;
            }
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
        Ok(WorkerRealizationScan {
            occupied,
            subjects,
            blockers,
        })
    }
}

fn blocker(
    session: &awaken_session_contract::PersistedSession,
    reason: WorkerRetirementBlockReason,
) -> WorkerRetirementBlocker {
    WorkerRetirementBlocker {
        session_id: session.session_id.as_str().to_owned(),
        reason,
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
        Ok(if scan.blockers.is_empty() {
            scan.subjects
        } else {
            Vec::new()
        })
    }
}
