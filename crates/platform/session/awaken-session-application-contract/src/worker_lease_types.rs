// @formal entrypoint session_run.execution_requires_dispatch_work_and_realization_owner
// @formal entrypoint session_work.only_a_claimed_nonterminal_run_may_revive_stopped_work
//! Session-owned Worker assignment lease and admission decisions.
use crate::SessionWorkGrant;
use awaken_lease_contract::fence::TenureScope;
use awaken_session_types::SessionId;
/// The lease family of Session Work: which Work item binds which Session in
/// which Environment (the subject), held by one execution owner.
///
/// A receipt or lease compared without the Work item or Environment could be
/// satisfied by another Work item of the same Session; the subject carries all
/// three so that can never happen. Consumers: [`SessionWorkGrant`] and
/// the Session application authority receipt.
#[derive(Debug)]
pub(crate) struct SessionWork;

impl TenureScope for SessionWork {
    /// `(work_id, environment_id, session_id)`.
    type Subject = (String, String, SessionId);
    /// The exact authenticated execution owner. For a registered Worker this is
    /// its incarnation-qualified lease owner, not the reusable display id.
    type Holder = String;
}

/// One-byte distinguishable spellings of a Work lease coordinate. Effect
/// authority proofs vary the owner through this fixture; receipt minting uses
/// its exact owned row. All values enter through the production lease adapter.
/// Receipt and effect-policy harnesses share this sample builder; neither
/// copy nor fixture data grants authority independently of the original port.
#[cfg(test)]
pub(super) fn proof_representative_lease(
    session: bool,
    owner: bool,
    work: bool,
    environment: bool,
    epoch: bool,
    expiry: bool,
) -> SessionWorkGrant {
    awaken_session_types::SessionWorkLease::from_persisted(
        if work { "k" } else { "K" },
        if environment { "e" } else { "E" },
        SessionId::from(if session { "s" } else { "S" }),
        if owner { "w" } else { "W" },
        if epoch { 1 } else { 2 },
        if expiry { 10 } else { 20 },
    )
    .map(SessionWorkGrant::from_transfer)
    .expect("representative epochs are nonzero")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionWorkOwnership {
    NotRequired,
    Unowned,
    Leased(SessionWorkGrant),
}

#[derive(Clone, Copy)]
enum SessionWorkEffectAuthority {
    NotRequired,
    Unowned,
    Leased,
}

const fn session_work_effect_policy(
    authority: SessionWorkEffectAuthority,
    owner_matches: bool,
) -> bool {
    match authority {
        SessionWorkEffectAuthority::NotRequired => true,
        SessionWorkEffectAuthority::Unowned => false,
        SessionWorkEffectAuthority::Leased => owner_matches,
    }
}

impl SessionWorkOwnership {
    /// Whether this observed Work authority permits the authenticated Worker
    /// to enter or renew Session realization effects.
    #[must_use]
    pub fn permits_effect_for(&self, worker_owner: &str) -> bool {
        let (authority, owner_matches) = match self {
            Self::NotRequired => (SessionWorkEffectAuthority::NotRequired, false),
            Self::Unowned => (SessionWorkEffectAuthority::Unowned, false),
            Self::Leased(lease) => (
                SessionWorkEffectAuthority::Leased,
                lease.owner() == worker_owner,
            ),
        };
        session_work_effect_policy(authority, owner_matches)
    }
}

/// Cause authorizing a private registered Worker to acquire Session Work.
/// A live claimed Run may repair a stopped Work projection after an admission /
/// predecessor-retirement race. A claimed Run-free preparation is the root's
/// own durable obligation handed to exactly one Ready Worker: it may take the
/// Session's already-dispatched Work, but it cannot revive stopped Work.
/// Realization renewal carries no driving event and may only renew a live Work
/// lease held by that Worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionWorkAcquisition {
    ClaimedRun,
    ClaimedPreparation,
    RealizationRenewal,
}

impl SessionWorkAcquisition {
    /// Whether this cause may take dispatched Work it does not already hold.
    /// Renewal never takes Work: a released Session's queued item belongs to
    /// the next claimed Run or preparation, not to the previous holder.
    #[must_use]
    pub const fn may_take_dispatched_work(self) -> bool {
        !matches!(self, Self::RealizationRenewal)
    }
}

/// Whether an absent Session Work lease may be repaired through the canonical
/// wake path. A live claimed Run is the only durable intent that can authorize
/// revival; renewal and terminal Session observations must remain inert.
#[must_use]
pub const fn session_work_revival_is_authorized(
    acquisition: SessionWorkAcquisition,
    session_is_terminal: bool,
    lease_was_acquired: bool,
) -> bool {
    !lease_was_acquired
        && !session_is_terminal
        && matches!(acquisition, SessionWorkAcquisition::ClaimedRun)
}

#[cfg(test)]
mod exhaustive {
    use super::*;

    const EVERY_ACQUISITION: [SessionWorkAcquisition; 3] = [
        SessionWorkAcquisition::ClaimedRun,
        SessionWorkAcquisition::ClaimedPreparation,
        SessionWorkAcquisition::RealizationRenewal,
    ];

    const fn listed_acquisition(acquisition: SessionWorkAcquisition) -> usize {
        match acquisition {
            SessionWorkAcquisition::ClaimedRun => 0,
            SessionWorkAcquisition::ClaimedPreparation => 1,
            SessionWorkAcquisition::RealizationRenewal => 2,
        }
    }

    #[test]
    fn only_renewal_is_refused_dispatched_work() {
        // Cause/effect decision table: C1 a claimed Run; C2 a claimed Run-free
        // preparation; C3 a realization renewal. E1 may take the Session's
        // dispatched (queued) Work; E2 may only renew a lease it already holds.
        // Rules: R1=C1=>E1; R2=C2=>E1; R3=C3=>E2.
        for (rule, acquisition, takes) in [
            ("R1", SessionWorkAcquisition::ClaimedRun, true),
            ("R2", SessionWorkAcquisition::ClaimedPreparation, true),
            ("R3", SessionWorkAcquisition::RealizationRenewal, false),
        ] {
            assert_eq!(acquisition.may_take_dispatched_work(), takes, "{rule}");
        }
        assert_eq!(EVERY_ACQUISITION.len(), 3);
    }

    #[test]
    fn every_acquisition_is_listed() {
        for (index, acquisition) in EVERY_ACQUISITION.into_iter().enumerate() {
            assert_eq!(listed_acquisition(acquisition), index);
        }
    }

    /// @exhaustive SessionWorkAcquisition(3) x bool(2) x bool(2) = 12
    /// @obligation session_work.only_a_claimed_nonterminal_run_may_revive_stopped_work
    #[test]
    fn stopped_session_work_is_revived_only_by_a_claimed_nonterminal_run() {
        let mut cases = 0;
        for acquisition in EVERY_ACQUISITION {
            let claimed_run = acquisition == SessionWorkAcquisition::ClaimedRun;
            for session_is_terminal in [false, true] {
                for lease_was_acquired in [false, true] {
                    assert_eq!(
                        session_work_revival_is_authorized(
                            acquisition,
                            session_is_terminal,
                            lease_was_acquired,
                        ),
                        claimed_run && !session_is_terminal && !lease_was_acquired
                    );
                    cases += 1;
                }
            }
        }
        assert_eq!(cases, 12);
    }

    /// @exhaustive bool(2) = 2
    /// @obligation session_run.execution_requires_dispatch_work_and_realization_owner
    #[test]
    fn session_work_effect_requires_not_required_scope_or_exact_owner() {
        // Every closed observed Work-authority class crossed with an exact and
        // a foreign owner spelling, entered through the production wrapper so
        // the owner fact is the one the code derives.
        for owner_matches in [false, true] {
            let leased = SessionWorkOwnership::Leased(proof_representative_lease(
                true,
                owner_matches,
                true,
                true,
                true,
                true,
            ));
            assert_eq!(
                leased.permits_effect_for("w"),
                owner_matches,
                "a lease permits effects only for its exact current owner"
            );
            assert!(SessionWorkOwnership::NotRequired.permits_effect_for("w"));
            assert!(!SessionWorkOwnership::Unowned.permits_effect_for("w"));
        }
    }
}
