// @formal surface obligation=session_realization.renewal_preserves_owner_incarnation_epoch_and_expiry_fence
// @formal surface obligation=session_realization.concurrent_drivers_preserve_one_owner_fence
// @formal reason: Shared root-lease and terminal-assignment admission helpers keep local recovery and
// @formal reason: external Workers beside one CAS claim algorithm and verify the aggregate's exact
// @formal reason: realization generation, preventing a second terminal lease state machine.
//! Existing root-lease and terminal-claim admission helpers.

use super::*;
use crate::archive_intent::unattempted_profiled_archive_witness;

/// Selector for the one terminal-assignment claim algorithm. External Workers
/// scan only Worker-owned Sessions; the co-located recovery driver asks for one
/// exact local Session. Keeping the selection fact beside the shared CAS loop
/// prevents local cleanup from growing a second lease state machine.
#[derive(Clone, Copy)]
pub(super) enum TerminalCleanupClaimScope<'a> {
    External,
    LocalSession(&'a str),
}

/// One uncertain root-CAS attempt. Retaining the attempted and predecessor
/// leases lets a retry compensate only its own exact write when the Worker
/// Registry withdraws authority; it is not a second lease record.
pub(super) struct TerminalCleanupClaimConflict {
    pub(super) owner_scope: String,
    pub(super) session_id: String,
    pub(super) attempted_lease: SessionRealizationLease,
    pub(super) previous_realization: Option<SessionRealizationLease>,
}

pub(super) enum TerminalCleanupRootClaim {
    Assignment(Box<SessionTerminalCleanupAssignment>),
    Skip,
    Conflict(Box<TerminalCleanupClaimConflict>),
    Abort(SessionRealizationControlFailure),
    Failed(SessionRealizationControlFailure),
}

impl<'a> TerminalCleanupClaimScope<'a> {
    pub(super) fn admits(self, session: &PersistedSession) -> bool {
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

    pub(super) fn is_exact_local(self) -> bool {
        matches!(self, Self::LocalSession(_))
    }

    pub(super) const fn is_external(self) -> bool {
        matches!(self, Self::External)
    }
}

/// Release only terminal Actor affinity from existing exact no-Create or
/// independently observed predecessor-exit evidence. Source identity is not
/// transferred; the provider still owns every physical verification/effect.
/// The existing claim CAS still owns replacement, Registry fencing and replay.
pub(super) async fn terminal_cleanup_worker_affinity_rejection(
    repository: &dyn awaken_session_repository_contract::SessionRepository,
    workers: Option<&dyn awaken_worker_contract::WorkerIncarnationSource>,
    scope: TerminalCleanupClaimScope<'_>,
    owner_scope: &str,
    session: &PersistedSession,
    target: &awaken_session_realization_contract::SessionRealizationTarget,
    now_unix_ms: u64,
) -> Option<TerminalCleanupRootClaim> {
    use awaken_session_state_contract::{
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
            let Some(workers) = workers else {
                return Some(TerminalCleanupRootClaim::Failed(
                    SessionRealizationControlFailure::Unavailable(
                        "exact predecessor Registry reader is not installed".into(),
                    ),
                ));
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

pub(super) fn verify_lease(
    session: &PersistedSession,
    asserted: &SessionRealizationLease,
    now_ms: u64,
) -> Result<(), SessionRealizationControlFailure> {
    if !session.realization.as_ref().is_some_and(|current| {
        awaken_session_realization_contract::realization_lease_authorizes(current, asserted, now_ms)
    }) {
        return Err(SessionRealizationControlFailure::StaleOwnership);
    }
    Ok(())
}

pub(super) fn exact_generation_key(generation: &McpGenerationRef) -> String {
    awaken_agent_contract::stable_fingerprint(generation)
}

pub(super) fn generation_set_authorizes(
    current: &[McpGenerationRef],
    asserted: &[McpGenerationRef],
) -> bool {
    // A bijection is required: two expiry variants of one generation cannot
    // stand in for the receipt of another attachment.
    let mut matched = BTreeSet::new();
    current.len() == asserted.len()
        && current.iter().all(|expected| {
            asserted
                .iter()
                .position(|actual| {
                    awaken_session_realization_contract::realization_generation_authorizes(
                        expected, actual,
                    )
                })
                .is_some_and(|index| matched.insert(index))
        })
}
