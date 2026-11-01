// @formal entrypoint session_resources.terminal_assignment_retains_desired_generation
//! Terminal cleanup claims: which process may hold a terminal Session's
//! realization lease to run its cleanup, for the co-located driver and for an
//! external Worker scanning Worker-owned Sessions.

use awaken_session_configuration_contract::resource::SessionRealizationLease;
use awaken_session_contract::{PersistedSession, SessionRepositoryError};
use awaken_session_execution_contract::{
    SessionRealizationControlFailure, SessionRealizationLeaseReceipt,
    SessionTerminalCleanupAssignment, terminal_cleanup_worker_requirements,
};
use awaken_time::WallClockExt as _;

use super::SessionTerminalCleanups;
use crate::SessionAggregate;
use crate::SessionMutationError;
use crate::archive_intent::unattempted_profiled_archive_witness;
use crate::mutation::ROOT_CAS_ATTEMPTS;
use crate::realization::{
    LOCAL_SESSION_REALIZATION_LEASE_MS, mutation_control, repository_control, unavailable,
    validate_realization_target,
};

/// Selector for the one terminal-assignment claim algorithm. External Workers
/// scan only Worker-owned Sessions; the co-located recovery driver asks for one
/// exact local Session. Keeping the selection fact beside the shared CAS loop
/// prevents local cleanup from growing a second lease state machine.
#[derive(Clone, Copy)]
pub(crate) enum TerminalCleanupClaimScope<'a> {
    External,
    LocalSession(&'a str),
}

/// One uncertain root-CAS attempt. Retaining the attempted and predecessor
/// leases lets a retry compensate only its own exact write when the Worker
/// Registry withdraws authority; it is not a second lease record.
pub(crate) struct TerminalCleanupClaimConflict {
    pub(crate) owner_scope: String,
    pub(crate) session_id: String,
    pub(crate) attempted_lease: SessionRealizationLease,
    pub(crate) previous_realization: Option<SessionRealizationLease>,
}

pub(crate) enum TerminalCleanupRootClaim {
    Assignment(Box<SessionTerminalCleanupAssignment>),
    Skip,
    Conflict(Box<TerminalCleanupClaimConflict>),
    Abort(SessionRealizationControlFailure),
    Failed(SessionRealizationControlFailure),
}

impl<'a> TerminalCleanupClaimScope<'a> {
    pub(crate) fn admits(self, session: &PersistedSession) -> bool {
        if !session.is_terminal() || !session.terminal_cleanup.is_requested() {
            return false;
        }
        match self {
            Self::External => session.requires_external_environment_owner(),
            Self::LocalSession(session_id) => {
                session.session_id == session_id && !session.requires_external_environment_owner()
            }
        }
    }

    pub(crate) fn is_exact_local(self) -> bool {
        matches!(self, Self::LocalSession(_))
    }

    pub(crate) const fn is_external(self) -> bool {
        matches!(self, Self::External)
    }
}

/// Release only terminal Actor affinity from existing exact no-Create or
/// independently observed predecessor-exit evidence. Source identity is not
/// transferred; the provider still owns every physical verification/effect.
/// The existing claim CAS still owns replacement, Registry fencing and replay.
pub(crate) async fn terminal_cleanup_worker_affinity_rejection(
    repository: &dyn awaken_session_contract::SessionRepository,
    workers: &dyn awaken_worker_contract::WorkerIncarnationSource,
    scope: TerminalCleanupClaimScope<'_>,
    owner_scope: &str,
    session: &PersistedSession,
    target: &awaken_session_execution_contract::SessionRealizationTarget,
    now_unix_ms: u64,
) -> Option<TerminalCleanupRootClaim> {
    use awaken_session_contract::{
        SessionTerminalCleanupAffinity, session_terminal_cleanup_affinity_for,
    };
    if !scope.is_external() {
        return None;
    }
    match session_terminal_cleanup_affinity_for(
        session,
        &target.owner,
        &target.runtime_incarnation,
        now_unix_ms,
    ) {
        SessionTerminalCleanupAffinity::Unchanged => None,
        SessionTerminalCleanupAffinity::Retained => Some(TerminalCleanupRootClaim::Skip),
        SessionTerminalCleanupAffinity::RequiresUnattemptedArchiveReceipt => {
            match unattempted_profiled_archive_witness(repository, owner_scope, session).await {
                Ok(witness) => (!witness).then_some(TerminalCleanupRootClaim::Skip),
                Err(error) => Some(TerminalCleanupRootClaim::Failed(repository_control(error))),
            }
        }
        SessionTerminalCleanupAffinity::RequiresPredecessorExit => {
            let Some(lease) = session.realization.as_ref() else {
                return Some(TerminalCleanupRootClaim::Skip);
            };
            let Some(identity) = awaken_worker_contract::WorkerIdentity::from_lease_owner(
                &lease.owner,
                &lease.runtime_incarnation,
            ) else {
                return Some(TerminalCleanupRootClaim::Skip);
            };
            match awaken_worker_contract::read_closed_incarnation_exit(workers, &identity).await {
                Ok(Some(_)) => None,
                Ok(_) => Some(TerminalCleanupRootClaim::Skip),
                Err(error) => Some(TerminalCleanupRootClaim::Failed(
                    SessionRealizationControlFailure::Unavailable(error.to_string()),
                )),
            }
        }
    }
}

/// Freeze the predecessor and apply its exact successor as one candidate command.
fn claim_terminal_cleanup_candidate(
    owner_scope: &str,
    session: &mut SessionAggregate,
    target: &awaken_session_execution_contract::SessionRealizationTarget,
) -> Result<TerminalCleanupClaimConflict, SessionRealizationControlFailure> {
    use awaken_lease_contract::fence::Epoch;
    let epoch = session
        .realization
        .as_ref()
        .map_or(Some(Epoch::FIRST), |lease| {
            Epoch::from_persisted(lease.epoch).and_then(Epoch::next)
        })
        .ok_or_else(|| {
            SessionRealizationControlFailure::Invalid(
                "Session realization lease epoch is exhausted".into(),
            )
        })?;
    let claim = TerminalCleanupClaimConflict {
        owner_scope: owner_scope.to_string(),
        session_id: session.session_id.to_string(),
        attempted_lease: SessionRealizationLease {
            owner: target.owner.clone(),
            runtime_incarnation: target.runtime_incarnation.clone(),
            epoch: epoch.get(),
            expires_at_unix_ms: target.lease_expires_at_unix_ms,
        },
        previous_realization: session.realization.clone(),
    };
    session
        .assignment()
        .claim(claim.attempted_lease.clone(), false)
        .map_err(unavailable)?;
    Ok(claim)
}

enum TerminalCleanupWorkerRevalidation {
    Admitted,
    Ineligible,
    Failed(SessionRealizationControlFailure),
    Abort(SessionRealizationControlFailure),
}

impl TerminalCleanupWorkerRevalidation {
    fn rejection(self) -> Option<TerminalCleanupRootClaim> {
        match self {
            Self::Admitted => None,
            Self::Ineligible => Some(TerminalCleanupRootClaim::Skip),
            Self::Failed(error) => Some(TerminalCleanupRootClaim::Failed(error)),
            Self::Abort(error) => Some(TerminalCleanupRootClaim::Abort(error)),
        }
    }
}

impl SessionTerminalCleanups {
    /// Build the only terminal assignment projection from one already-read
    /// Session root. Claim recovery and warm work polling share this adapter so
    /// neither can select a lease or frozen projection independently.
    pub(crate) async fn terminal_cleanup_assignment_from_snapshot(
        &self,
        owner_scope: &str,
        session: &PersistedSession,
    ) -> Result<SessionTerminalCleanupAssignment, SessionRealizationControlFailure> {
        let lease = session
            .realization
            .clone()
            .ok_or(SessionRealizationControlFailure::StaleOwnership)?;
        // Terminal effects must retain both aggregate-authored generations.
        // A Worker may have installed desired B before its receipt committed;
        // projecting only active A regresses that slot to A -> A and strands
        // cleanup. The terminal Host install stages A -> B without activating B.
        let projection = self
            .queries
            .frozen_session_projection(owner_scope.to_string(), session, false)
            .await
            .map_err(unavailable)?;
        Ok(SessionTerminalCleanupAssignment {
            session_id: session.session_id.clone(),
            projection,
            environment_creation_not_attempted:
                crate::archive_intent::terminal_environment_creation_not_attempted(
                    self.mutations.repository().as_ref(),
                    owner_scope,
                    session,
                )
                .await
                .map_err(repository_control)?,
            authority: SessionRealizationLeaseReceipt::issued_at(lease, self.clock.now_ms())?,
        })
    }

    /// Undo only this invocation's exact uncertain claim after Registry
    /// authority disappears. Each retry reloads the aggregate and preserves
    /// every concurrent field; a different realization proves that another
    /// root writer already won and must never be overwritten.
    async fn compensate_terminal_cleanup_claim(
        &self,
        conflict: &TerminalCleanupClaimConflict,
        initial: Option<PersistedSession>,
    ) -> Result<(), SessionRealizationControlFailure> {
        let mutations = &self.mutations;
        let current = match initial {
            Some(session) => session,
            None => match self
                .mutations
                .repository()
                .get(&awaken_session_types::SessionId::from(&conflict.session_id))
                .await
            {
                Ok(session) => session,
                Err(SessionRepositoryError::NotFound) => return Ok(()),
                Err(error) => return Err(repository_control(error)),
            },
        };
        let mut current =
            crate::mutation::aggregate_from_snapshot(current).map_err(repository_control)?;
        for attempt in 0..ROOT_CAS_ATTEMPTS {
            if current.session_id != conflict.session_id
                || current.realization.as_ref() != Some(&conflict.attempted_lease)
            {
                return Ok(());
            }
            current
                .assignment()
                .restore_predecessor(
                    &conflict.attempted_lease,
                    conflict.previous_realization.clone(),
                )
                .map_err(unavailable)?;
            match mutations
                .commit_session_snapshot_with_outcome(
                    &conflict.owner_scope,
                    current,
                    "revoke-terminal-cleanup-recovery",
                    Vec::new(),
                )
                .await
            {
                Ok(_) => return Ok(()),
                Err(SessionMutationError::Conflict) if attempt + 1 < ROOT_CAS_ATTEMPTS => {
                    current = match mutations
                        .session_aggregate(&awaken_session_types::SessionId::from(
                            &conflict.session_id,
                        ))
                        .await
                    {
                        Ok(session) => session,
                        Err(SessionRepositoryError::NotFound) => return Ok(()),
                        Err(error) => return Err(repository_control(error)),
                    };
                }
                Err(SessionMutationError::Conflict) => {
                    return Err(SessionRealizationControlFailure::Conflict);
                }
                Err(error) => return Err(mutation_control(error)),
            }
        }
        Err(SessionRealizationControlFailure::Conflict)
    }

    async fn revalidate_terminal_cleanup_worker(
        &self,
        target: &awaken_session_execution_contract::SessionRealizationTarget,
        owner_scope: &str,
        session: &PersistedSession,
        action: &awaken_session_contract::SessionTerminalCleanupAction,
        conflict: Option<&TerminalCleanupClaimConflict>,
    ) -> TerminalCleanupWorkerRevalidation {
        // A just-written claim is not prior ownership at readback. Reuse the
        // conflict record for both this decision and exact compensation below.
        let prior = match conflict {
            Some(conflict) => conflict.previous_realization.as_ref(),
            None => session.realization.as_ref(),
        };
        // Selection cannot acquire another Actor's unprepared obligation when
        // the very next live-source effect is already known to be inadmissible.
        // Preserve original-owner continuation and receipt-only disposal. Read
        // source eligibility BEFORE the Registry check so this additional I/O
        // cannot widen the final Worker authority window. Exact compensation
        // below also covers a source change after an uncertain/winning CAS.
        let source_admitted = prior.is_some_and(|lease| {
            lease.owner == target.owner && lease.runtime_incarnation == target.runtime_incarnation
        }) || !matches!(
            action,
            awaken_session_contract::SessionTerminalCleanupAction::Prepare { .. }
        ) || !matches!(
            session.environment,
            awaken_session_contract::SessionEnvironmentState::Resident { .. }
        ) || matches!(
            self.coordination_reads
                .source_dispatch_observation(session)
                .await,
            awaken_session_execution_contract::SessionSourceDispatchObservation::Settled
        );
        let admission = if source_admitted {
            match self.realizations.worker_for_target(target).await {
                Ok(Some(worker)) if !target.admits_terminal_cleanup_generation(&worker, prior) => {
                    Ok(false)
                }
                Ok(Some(worker)) => {
                    terminal_cleanup_worker_requirements(owner_scope, session, action).map(
                        |requirements| {
                            awaken_worker_contract::can_claim(&worker.manifest, &requirements)
                                .is_ok()
                        },
                    )
                }
                Ok(None) => Ok(false),
                Err(error) => Err(error),
            }
        } else {
            Ok(false)
        };
        let rejected = match admission {
            Ok(true) => return TerminalCleanupWorkerRevalidation::Admitted,
            Ok(false) => TerminalCleanupWorkerRevalidation::Ineligible,
            Err(error) => TerminalCleanupWorkerRevalidation::Failed(error),
        };
        if let Some(conflict) = conflict
            && let Err(error) = self
                .compensate_terminal_cleanup_claim(conflict, Some(session.clone()))
                .await
        {
            return TerminalCleanupWorkerRevalidation::Abort(error);
        }
        rejected
    }

    async fn claim_terminal_cleanup_root(
        &self,
        owner_scope: &str,
        mut session: SessionAggregate,
        target: &awaken_session_execution_contract::SessionRealizationTarget,
        scope: TerminalCleanupClaimScope<'_>,
        replay_after_conflict: Option<&TerminalCleanupClaimConflict>,
    ) -> TerminalCleanupRootClaim {
        if !scope.admits(&session) {
            return TerminalCleanupRootClaim::Skip;
        }
        if let Some(rejection) = terminal_cleanup_worker_affinity_rejection(
            self.mutations.repository().as_ref(),
            self.execution_admission.workers.as_ref(),
            scope,
            owner_scope,
            &session,
            target,
            self.clock.now_ms(),
        )
        .await
        {
            return rejection;
        }
        // With no historical lease, only an Environment that has never acquired
        // a physical binding is eligible for first placement. Resident and
        // continuation states without an affinity fact are legacy/inconsistent
        // evidence and must not be guessed from the current Worker manifest.
        if scope.is_external()
            && session.realization.is_none()
            && !matches!(
                &session.environment,
                awaken_session_contract::SessionEnvironmentState::Unmaterialized
            )
        {
            return TerminalCleanupRootClaim::Failed(SessionRealizationControlFailure::Invalid(
                "terminal Session has physical Environment state without a realization owner"
                    .into(),
            ));
        }
        let worker_action = match session.terminal_cleanup_work_action() {
            Ok(action) => action,
            Err(error) => {
                return TerminalCleanupRootClaim::Failed(
                    SessionRealizationControlFailure::Invalid(error.to_string()),
                );
            }
        };
        let Some(worker_action) = worker_action else {
            return TerminalCleanupRootClaim::Skip;
        };
        if scope.is_external()
            && let Some(rejection) = self
                .revalidate_terminal_cleanup_worker(
                    target,
                    owner_scope,
                    &session,
                    &worker_action,
                    replay_after_conflict,
                )
                .await
                .rejection()
        {
            return rejection;
        }

        let current_is_live = session.realization.as_ref().is_some_and(|current| {
            awaken_session_configuration_contract::resource::realization_lease_is_live_at(
                current.expires_at_unix_ms,
                self.clock.now_ms(),
            )
        });
        if current_is_live {
            let current = session
                .realization
                .clone()
                .expect("a live realization lease was checked");
            let exact_incarnation = current.owner == target.owner
                && current.runtime_incarnation == target.runtime_incarnation;
            if exact_incarnation {
                let assignment = self
                    .terminal_cleanup_assignment_from_snapshot(owner_scope, &session)
                    .await;
                if scope.is_external()
                    && let Some(rejection) = self
                        .revalidate_terminal_cleanup_worker(
                            target,
                            owner_scope,
                            &session,
                            &worker_action,
                            replay_after_conflict,
                        )
                        .await
                        .rejection()
                {
                    return rejection;
                }
                return match assignment {
                    Ok(assignment) => TerminalCleanupRootClaim::Assignment(Box::new(assignment)),
                    Err(error) => TerminalCleanupRootClaim::Failed(error),
                };
            }
            // A live predecessor incarnation or foreign logical owner remains
            // provider-affined. The current incarnation uses its exact lease
            // above; the request-local keyset cursor prevents it from being
            // returned twice in one bounded Worker recovery sweep.
            return if scope.is_exact_local() {
                TerminalCleanupRootClaim::Failed(SessionRealizationControlFailure::StaleOwnership)
            } else {
                TerminalCleanupRootClaim::Skip
            };
        }

        let claim = match claim_terminal_cleanup_candidate(owner_scope, &mut session, target) {
            Ok(claim) => claim,
            Err(error) => return TerminalCleanupRootClaim::Failed(error),
        };
        let committed = match self
            .mutations
            .commit_session_snapshot_with_outcome(
                owner_scope,
                session,
                "claim-terminal-cleanup-recovery",
                Vec::new(),
            )
            .await
        {
            Ok(commit) => commit.into_session(),
            Err(SessionMutationError::Conflict) => {
                return TerminalCleanupRootClaim::Conflict(Box::new(claim));
            }
            Err(error) => return TerminalCleanupRootClaim::Failed(unavailable(error)),
        };
        let assignment = self
            .terminal_cleanup_assignment_from_snapshot(owner_scope, &committed)
            .await;
        if scope.is_external() {
            let committed_action = match committed.terminal_cleanup_work_action() {
                Ok(Some(action)) => action,
                Ok(None) => {
                    return TerminalCleanupRootClaim::Failed(
                        SessionRealizationControlFailure::Invalid(
                            "claimed terminal Session has no remaining cleanup action".into(),
                        ),
                    );
                }
                Err(error) => {
                    return TerminalCleanupRootClaim::Failed(
                        SessionRealizationControlFailure::Invalid(error.to_string()),
                    );
                }
            };
            if let Some(rejection) = self
                .revalidate_terminal_cleanup_worker(
                    target,
                    owner_scope,
                    &committed,
                    &committed_action,
                    Some(&claim),
                )
                .await
                .rejection()
            {
                return rejection;
            }
        }
        match assignment {
            Ok(assignment) => TerminalCleanupRootClaim::Assignment(Box::new(assignment)),
            // The claim is already durable. An exact local retry reuses this
            // lease; an external scan skips it until expiry.
            Err(error) => TerminalCleanupRootClaim::Failed(error),
        }
    }

    pub(super) async fn claim_terminal_cleanup_after_refresh(
        &self,
        target: awaken_session_execution_contract::SessionRealizationTarget,
        after_session_id: Option<&awaken_session_types::SessionId>,
    ) -> Result<Option<SessionTerminalCleanupAssignment>, SessionRealizationControlFailure> {
        validate_realization_target(&target, self.clock.now_ms())?;
        if target.reassign_existing_lease {
            return Err(SessionRealizationControlFailure::Invalid(
                "terminal cleanup recovery claims cannot reassign a live logical owner".into(),
            ));
        }
        let mut conflicted_claim: Option<Box<TerminalCleanupClaimConflict>> = None;
        for attempt in 0..ROOT_CAS_ATTEMPTS {
            // Re-read the canonical Worker registry on every root-CAS attempt.
            // A conflict never carries eligibility from the older Session or
            // Worker snapshot into the retry.
            let worker = self.realizations.worker_for_target(&target).await;
            if !matches!(&worker, Ok(Some(_))) {
                if let Some(conflict) = conflicted_claim.as_deref() {
                    self.compensate_terminal_cleanup_claim(conflict, None)
                        .await?;
                }
                return worker.map(|_| None);
            }
            let mut first_projection_failure = None;
            let mut retry_after_conflict = false;
            let mut cursor = after_session_id.map(|session_id| {
                awaken_session_contract::SessionScanCursor::after_session_id(session_id.as_str())
            });
            loop {
                let scan = self
                    .mutations
                    .repository()
                    .reconciliation_scan(cursor.as_ref())
                    .await
                    .map_err(repository_control)?;
                let next_cursor = scan.next_cursor;
                for scoped in scan.sessions {
                    let replay_after_conflict = conflicted_claim
                        .as_deref()
                        .filter(|conflict| conflict.session_id == scoped.session.session_id);
                    match self
                        .claim_terminal_cleanup_root(
                            &scoped.workspace_id,
                            crate::mutation::aggregate_from_snapshot(scoped.session)
                                .map_err(repository_control)?,
                            &target,
                            TerminalCleanupClaimScope::External,
                            replay_after_conflict,
                        )
                        .await
                    {
                        TerminalCleanupRootClaim::Assignment(assignment) => {
                            return Ok(Some(*assignment));
                        }
                        TerminalCleanupRootClaim::Skip => {}
                        TerminalCleanupRootClaim::Conflict(conflict)
                            if attempt + 1 < ROOT_CAS_ATTEMPTS =>
                        {
                            conflicted_claim = Some(conflict);
                            retry_after_conflict = true;
                            break;
                        }
                        TerminalCleanupRootClaim::Conflict(_) => {
                            return Err(SessionRealizationControlFailure::Conflict);
                        }
                        TerminalCleanupRootClaim::Abort(error) => return Err(error),
                        TerminalCleanupRootClaim::Failed(error) => {
                            first_projection_failure.get_or_insert(error);
                        }
                    }
                }
                if retry_after_conflict {
                    break;
                }
                let Some(next_cursor) = next_cursor else {
                    break;
                };
                if cursor
                    .as_ref()
                    .is_some_and(|cursor| next_cursor.session_id() <= cursor.session_id())
                {
                    return Err(SessionRealizationControlFailure::Unavailable(
                        "Session reconciliation cursor did not advance".into(),
                    ));
                }
                cursor = Some(next_cursor);
            }
            if retry_after_conflict {
                continue;
            }
            return match first_projection_failure {
                Some(error) => Err(error),
                None => Ok(None),
            };
        }
        Err(SessionRealizationControlFailure::Conflict)
    }

    pub(crate) async fn claim_local_terminal_cleanup_assignment(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<SessionTerminalCleanupAssignment, SessionRealizationControlFailure> {
        let mutations = &self.mutations;
        let admission = self
            .executable_projections
            .refresh()
            .await
            .map_err(unavailable)?;
        let lease_expires_at_unix_ms = self
            .clock
            .now_ms()
            .checked_add(LOCAL_SESSION_REALIZATION_LEASE_MS)
            .ok_or_else(|| {
                SessionRealizationControlFailure::Unavailable(
                    "terminal cleanup lease expiry overflow".into(),
                )
            })?;
        let target = awaken_session_execution_contract::SessionRealizationTarget {
            owner: self.realizations.local_realization_owner().to_string(),
            runtime_incarnation: self.realizations.runtime_incarnation().to_string(),
            lease_expires_at_unix_ms,
            reassign_existing_lease: false,
        };
        validate_realization_target(&target, self.clock.now_ms())?;
        for attempt in 0..ROOT_CAS_ATTEMPTS {
            let owner_scope = admission
                .enter(|| mutations.owner(session_id))
                .await
                .map_err(mutation_control)?;
            let session = mutations
                .session_aggregate(session_id)
                .await
                .map_err(repository_control)?;
            match self
                .claim_terminal_cleanup_root(
                    &owner_scope,
                    session,
                    &target,
                    TerminalCleanupClaimScope::LocalSession(session_id.as_str()),
                    None,
                )
                .await
            {
                TerminalCleanupRootClaim::Assignment(assignment) => return Ok(*assignment),
                TerminalCleanupRootClaim::Skip => {
                    return Err(SessionRealizationControlFailure::NotReady);
                }
                TerminalCleanupRootClaim::Conflict(_) if attempt + 1 < ROOT_CAS_ATTEMPTS => {}
                TerminalCleanupRootClaim::Conflict(_) => {
                    return Err(SessionRealizationControlFailure::Conflict);
                }
                TerminalCleanupRootClaim::Abort(error) => return Err(error),
                TerminalCleanupRootClaim::Failed(error) => return Err(error),
            }
        }
        Err(SessionRealizationControlFailure::Conflict)
    }
}
