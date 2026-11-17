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

use awaken_runtime_contract::runs::RunError;
use awaken_session_application_contract::{
    SessionWorkAcquisition, SessionWorkGrant, SessionWorkOwnership, SessionWorkerAssignment,
    SessionWorkerLeaseAuthority, SessionWorkerLeases,
};
use awaken_session_contract::SessionRepository;
use awaken_session_contract::{PersistedSession, SessionRepositoryError};
use awaken_session_types::SessionId;
use awaken_time::WallClockExt as _;

use crate::recovery_scan::scan_recovery_candidates;
use crate::{SessionPreparationError, WorkDispatchFailure, WorkDispatchReconciliation};

#[derive(Clone)]
pub struct SessionWorkPersistenceViews {
    pub roots: Arc<dyn awaken_session_contract::SessionRepositoryReads>,
    pub leases: Arc<dyn SessionWorkerLeases>,
}

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

    /// These are views of this same owner, never new repositories or queues.
    #[must_use]
    pub fn persistence_views(&self) -> SessionWorkPersistenceViews {
        SessionWorkPersistenceViews {
            roots: self.repository.clone(),
            leases: self.leases.clone(),
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

    /// Project the still-current MCP demand after an accepted Session update.
    /// Replay may reconcile current desired truth, while a superseded observed
    /// generation, a terminal root and a root in a continuation step cannot
    /// wake another Work obligation.
    pub(crate) async fn wake_for_mcp_update(
        &self,
        observed: &PersistedSession,
    ) -> Result<(), RunError> {
        let current = match self.repository.get(&observed.session_id).await {
            Ok(session) => session,
            Err(SessionRepositoryError::NotFound) => return Ok(()),
            Err(error) => return Err(RunError::unavailable(error.to_string())),
        };
        let observed_generations = observed
            .mcp
            .desired_attachments()
            .into_iter()
            .map(|attachment| &attachment.generation)
            .collect::<Vec<_>>();
        let current_generations = current
            .mcp
            .desired_attachments()
            .into_iter()
            .map(|attachment| &attachment.generation)
            .collect::<Vec<_>>();
        // A Suspending, Restoring or Hibernated root's Work belongs to its
        // continuation (`continuation_wakes_session_work`) and to the next
        // admitted Run. The accepted desired MCP generation stays durable and
        // the restore that re-realizes the Environment picks it up, so the
        // update itself wakes nothing.
        if !current.is_run_free_preparation_candidate()
            || !current.environment.admits_realization_effects()
            || current.revision < observed.revision
            || current.mcp.desired_fingerprint != observed.mcp.desired_fingerprint
            || current_generations != observed_generations
        {
            return Ok(());
        }
        let Some((environment_id, false)) = session_work_environment(&current) else {
            return Ok(());
        };
        self.assignment
            .wake_session(&environment_id, &current.session_id)
            .await
            .map(|_| ())
    }

    /// Acquire the canonical Work lease for this current run-free Root grant.
    /// Passive renewal and stopped-Work revival retain their existing owners.
    pub(crate) async fn acquire_for_preparation(
        &self,
        session_id: &SessionId,
        asserted: &awaken_session_configuration_contract::resource::SessionRealizationLease,
    ) -> Result<
        SessionWorkOwnership,
        awaken_session_execution_contract::SessionRealizationControlFailure,
    > {
        let current = self
            .repository
            .get(session_id)
            .await
            .map_err(crate::realization::repository_control)?;
        if !current.is_run_free_preparation_candidate() {
            return Ok(SessionWorkOwnership::Unowned);
        }
        crate::realization::verify_lease(&current, asserted, self.clock.now_ms())?;
        self.acquire_session_work(
            session_id,
            &asserted.runtime_incarnation,
            SessionWorkAcquisition::ClaimedPreparation,
        )
        .await
        .map_err(|error| crate::SessionMutationError::admission(error).into())
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
        Ok(session_work_environment(&session))
    }
}

/// The existing Session-owned Work topology projection, reused by composed persistence.
#[must_use]
pub fn session_work_environment(session: &PersistedSession) -> Option<(String, bool)> {
    session
        .frozen_baseline()
        .filter(|baseline| baseline.environment.self_hosted)
        .map(|baseline| {
            (
                baseline.environment.environment_id.clone(),
                session.is_terminal(),
            )
        })
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

/// A finished Run-free preparation hands back the Work its claim took
/// (ADR-0106) only while the current root still holds its grant and admitted
/// no Run, and the Work store still records the preparation's hold; a vanished
/// root releases nothing.
///
/// | Rule | root decision      | Work store hold                | effect          |
/// |------|--------------------|--------------------------------|-----------------|
/// | R1   | release            | held by this preparation       | release, `true` |
/// | R2   | keep (Run admitted)| any                            | nothing         |
/// | R3   | release            | a Run acquired it since        | nothing         |
/// | R4   | release            | another holder or none         | nothing         |
#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionPreparationWorkRelease for SessionWorkAssignments {
    async fn release_finished_preparation_work(
        &self,
        command: awaken_session_execution_contract::FinishSessionPreparation,
        worker_owner: &str,
    ) -> Result<bool, awaken_session_execution_contract::SessionRealizationControlFailure> {
        let current = match self.repository.get(&command.session_id).await {
            Ok(current) => current,
            Err(SessionRepositoryError::NotFound) => return Ok(false),
            Err(error) => return Err(crate::realization::repository_control(error)),
        };
        if !awaken_session_contract::finished_preparation_releases_work(&current, &command.lease) {
            return Ok(false);
        }
        let Some((environment_id, _terminal)) = session_work_environment(&current) else {
            return Ok(false);
        };
        self.leases
            .release_preparation_session(&environment_id, &command.session_id, worker_owner)
            .await
            .map_err(|error| crate::SessionMutationError::admission(error).into())
    }
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
        acquisition: SessionWorkAcquisition,
    ) -> Result<SessionWorkOwnership, RunError> {
        let session = match self.repository.get(session_id).await {
            Ok(session) => session,
            Err(SessionRepositoryError::NotFound) => {
                return Ok(SessionWorkOwnership::NotRequired);
            }
            Err(error) => return Err(RunError::unavailable(error.to_string())),
        };
        let Some((environment_id, terminal)) = session_work_environment(&session) else {
            return Ok(SessionWorkOwnership::NotRequired);
        };
        if !acquisition.may_take_dispatched_work() {
            return Ok(
                match self
                    .leases
                    .renew_session(&environment_id, session_id, worker_owner)
                    .await?
                {
                    Some(lease) => {
                        SessionWorkOwnership::Leased(SessionWorkGrant::from_transfer(lease))
                    }
                    // The lease renewal alone is no Environment effect: an
                    // idle source keeps its exact generation alive for its
                    // own checkpoint while its stopped Work stays stopped.
                    None if acquisition.renews_lease_only()
                        && awaken_session_contract::idle_source_lease_outlives_session_work(
                            &session,
                        ) =>
                    {
                        SessionWorkOwnership::NotRequired
                    }
                    None => SessionWorkOwnership::Unowned,
                },
            );
        }
        let acquired = if acquisition == SessionWorkAcquisition::ClaimedPreparation {
            self.leases
                .acquire_preparation_session(&environment_id, session_id, worker_owner)
                .await?
        } else {
            self.leases
                .acquire_session(&environment_id, session_id, worker_owner)
                .await?
        };
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
            .acquire_session(&environment_id, session_id, worker_owner)
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
