// @formal entrypoint session_creation.work_projection_is_disposable_repairable_and_non_authoritative
// @formal entrypoint session_work.only_a_claimed_nonterminal_run_may_revive_stopped_work
//! Session Work: the durable Session's projection into the Environment's
//! WorkQueue and the Session-owned permission to lease that Work.
//!
//! One use case: keep each self-hosted Session's one WorkQueue item in step
//! with its durable root (enqueue while nonterminal, retire through terminal
//! cleanup) and admit a Worker's lease on it. The component holds the root
//! reader, the selected WorkQueue assignment and lease ports, the lifecycle
//! signal and the application clock. It owns no Session state: every decision
//! reads the root fresh, and the queue stays the only Work authority.

use std::sync::Arc;

use awaken_session_application_contract::{
    SessionWorkAcquisition, SessionWorkGrant, SessionWorkOwnership, SessionWorkerAssignment,
    SessionWorkerLeases, WorkDispatchFailure, WorkDispatchReconciliation,
};
use awaken_session_repository_contract::SessionRepository;
use awaken_session_state_contract::{PersistedSession, SessionRepositoryError};
use awaken_session_types::{RunError, SessionId};
use awaken_time::WallClockExt as _;

use crate::SessionPreparationError;
use crate::recovery_scan::scan_recovery_candidates;

pub struct SessionWorkAssignments {
    repository: Arc<dyn SessionRepository>,
    assignment: Arc<dyn SessionWorkerAssignment>,
    leases: Arc<dyn SessionWorkerLeases>,
    lifecycle: Arc<crate::SessionLifecycle>,
    clock: Arc<dyn awaken_time::WallClock>,
}

impl SessionWorkAssignments {
    pub(crate) fn new(
        repository: Arc<dyn SessionRepository>,
        assignment: Arc<dyn SessionWorkerAssignment>,
        leases: Arc<dyn SessionWorkerLeases>,
        lifecycle: Arc<crate::SessionLifecycle>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self {
            repository,
            assignment,
            leases,
            lifecycle,
            clock,
        }
    }

    /// Project one durable Session into its WorkQueue item. Returns whether a
    /// projection was required.
    pub async fn dispatch(&self, session: &PersistedSession) -> Result<bool, RunError> {
        dispatch_session_work(self.assignment.as_ref(), session).await
    }

    /// Re-project every recoverable Session from a complete repository scan.
    pub async fn reconcile(&self) -> WorkDispatchReconciliation {
        reconcile_work_dispatches(
            self.repository.as_ref(),
            self.assignment.as_ref(),
            self.clock.now_ms(),
        )
        .await
    }

    /// Wake one self-hosted Session's stopped Work item for an admitted Run.
    pub(crate) async fn wake(
        &self,
        environment_id: &str,
        session_id: &SessionId,
    ) -> Result<String, RunError> {
        self.assignment
            .wake_session(environment_id, session_id)
            .await
    }

    /// Retire a terminal self-hosted Session's Work item. Only the terminal
    /// cleanup driver calls this, inside its recoverable fence.
    pub(crate) async fn retire(
        &self,
        session: &PersistedSession,
    ) -> Result<(), SessionPreparationError> {
        let Some(baseline) = session
            .frozen_baseline()
            .filter(|baseline| baseline.environment.self_hosted)
        else {
            return Ok(());
        };
        self.assignment
            .retire_session(&baseline.environment.environment_id, &session.session_id)
            .await
            .map(|_| ())
            .map_err(|error| {
                SessionPreparationError::Rejected(RunError::unavailable_classified(
                    "session_work_retire_failed",
                    format!("Session Work could not be retired: {error}"),
                ))
            })
    }

    async fn self_hosted_work_environment(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<(String, bool)>, RunError> {
        let session = match self.repository.get(session_id).await {
            Ok(session) => session,
            Err(SessionRepositoryError::NotFound) => return Ok(None),
            Err(error) => return Err(RunError::unavailable(error.to_string())),
        };
        Ok(session
            .frozen_baseline()
            .filter(|baseline| baseline.environment.self_hosted)
            .map(|baseline| {
                (
                    baseline.environment.environment_id.clone(),
                    session.is_terminal(),
                )
            }))
    }
}

pub(crate) async fn dispatch_session_work(
    assignments: &dyn SessionWorkerAssignment,
    session: &PersistedSession,
) -> Result<bool, RunError> {
    if session.is_terminal() {
        // Keep the externally auditable terminal guard at the effect boundary.
        // The shared plan below repeats the same immutable fact so recovery and
        // create-time callers cannot acquire a second projection policy.
        return Ok(false);
    }
    if !session.requires_work_projection() {
        return Ok(false);
    }
    let Some(baseline) = session.frozen_baseline() else {
        // The closed production decision above cannot admit this state. Keep
        // the adapter fail-closed if the representation is ever extended.
        return Ok(false);
    };
    // Terminal Work retirement belongs exclusively to the recoverable resource
    // cleanup driver. A second reconciler would duplicate the external attempt
    // and obscure its durable completion boundary.
    assignments
        .enqueue_session(&baseline.environment.environment_id, &session.session_id)
        .await?;
    Ok(true)
}

pub(crate) async fn reconcile_work_dispatches(
    sessions: &dyn SessionRepository,
    assignments: &dyn SessionWorkerAssignment,
    now_unix_ms: u64,
) -> WorkDispatchReconciliation {
    let mut report = WorkDispatchReconciliation::default();
    let candidates = match scan_recovery_candidates(sessions, false, now_unix_ms).await {
        Ok(candidates) => candidates,
        Err(error) => {
            report.failures.push(WorkDispatchFailure {
                session_id: "<repository>".to_string(),
                environment_id: String::new(),
                message: error.to_string(),
            });
            return report;
        }
    };
    report.pending = candidates.sessions.len();
    report.quarantined = candidates.quarantined;
    for candidate in candidates.sessions {
        // The complete scan retains identities only. Reuse the ordinary fresh
        // root read and dispatch policy before an external WorkQueue effect.
        let session = match sessions.get(&candidate.session_id).await {
            Ok(session) => session,
            Err(SessionRepositoryError::NotFound) => continue,
            Err(error) => {
                report.failures.push(WorkDispatchFailure {
                    session_id: candidate.session_id.to_string(),
                    environment_id: String::new(),
                    message: error.to_string(),
                });
                continue;
            }
        };
        match dispatch_session_work(assignments, &session).await {
            Ok(true) => report.settled += 1,
            Ok(false) => {}
            Err(error) => report.failures.push(WorkDispatchFailure {
                session_id: session.session_id.to_string(),
                environment_id: session.environment_id().to_string(),
                message: error.to_string(),
            }),
        }
    }
    report
}

#[async_trait::async_trait]
impl awaken_session_application_contract::SessionWorkerLeaseAuthority for SessionWorkAssignments {
    fn request_lifecycle_reconciliation(&self) {
        self.lifecycle.request_reconciliation();
    }

    async fn acquire_session_work(
        &self,
        session_id: &SessionId,
        worker_owner: &str,
        now_ms: u64,
        acquisition: SessionWorkAcquisition,
    ) -> Result<SessionWorkOwnership, RunError> {
        let Some((environment_id, terminal)) =
            self.self_hosted_work_environment(session_id).await?
        else {
            return Ok(SessionWorkOwnership::NotRequired);
        };
        let acquired = self
            .leases
            .acquire_session(&environment_id, session_id, worker_owner, now_ms)
            .await?;
        if !awaken_session_application_contract::session_work_revival_is_authorized(
            acquisition,
            terminal,
            acquired.is_some(),
        ) {
            return Ok(acquired
                .map(SessionWorkGrant::from_transfer)
                .map(SessionWorkOwnership::Leased)
                .unwrap_or(SessionWorkOwnership::Unowned));
        }
        // A predecessor may retire the stable Session Work item after a
        // successor admission already attempted to wake it. The authenticated,
        // claimed successor Run is itself a durable wake intent: revive through
        // the one WorkQueue path and retry acquisition exactly once. Terminal
        // Sessions are excluded above and can never be resurrected here.
        self.assignment
            .wake_session(&environment_id, session_id)
            .await?;
        Ok(self
            .leases
            .acquire_session(&environment_id, session_id, worker_owner, now_ms)
            .await?
            .map(SessionWorkGrant::from_transfer)
            .map(SessionWorkOwnership::Leased)
            .unwrap_or(SessionWorkOwnership::Unowned))
    }

    async fn release_session_work(&self, lease: &SessionWorkGrant) -> Result<bool, RunError> {
        let Some((environment_id, _terminal)) = self
            .self_hosted_work_environment(lease.session_id())
            .await?
        else {
            return Ok(true);
        };
        if lease.environment_id() != environment_id {
            return Ok(false);
        }
        self.leases.release_session(&lease.to_transfer()).await
    }

    async fn release_worker_session_work(&self, worker_owner: &str) -> Result<usize, RunError> {
        self.leases.release_owner(worker_owner).await
    }
}
