// @formal entrypoint session_run.execution_requires_dispatch_work_and_realization_owner
// @formal entrypoint session_work.only_a_claimed_nonterminal_run_may_revive_stopped_work
//! Session-owned Worker assignment lease and admission decisions.
use awaken_lease_contract::fence::TenureScope;
use awaken_session_types::SessionId;
use awaken_session_types::SessionWorkLease;
/// The lease family of Session Work: which Work item binds which Session in
/// which Environment (the subject), held by one execution owner.
///
/// A receipt or lease compared without the Work item or Environment could be
/// satisfied by another Work item of the same Session; the subject carries all
/// three so that can never happen. Consumers: [`SessionWorkLease`] and
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
#[cfg(kani)]
pub(super) fn proof_representative_lease(
    session: bool,
    owner: bool,
    work: bool,
    environment: bool,
    epoch: bool,
    expiry: bool,
) -> SessionWorkLease {
    SessionWorkLease::from_persisted(
        if work { "k" } else { "K" },
        if environment { "e" } else { "E" },
        SessionId::from(if session { "s" } else { "S" }),
        if owner { "w" } else { "W" },
        if epoch { 1 } else { 2 },
        if expiry { 10 } else { 20 },
    )
    .expect("representative epochs are nonzero")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionWorkOwnership {
    NotRequired,
    Unowned,
    Leased(SessionWorkLease),
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
/// predecessor-retirement race. Realization renewal carries no driving event
/// and may only renew or acquire already-dispatched Work; it cannot resurrect a
/// stopped Session after its Run has settled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionWorkAcquisition {
    ClaimedRun,
    RealizationRenewal,
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

#[cfg(kani)]
/// @obligation session_work.only_a_claimed_nonterminal_run_may_revive_stopped_work
#[kani::proof]
fn stopped_session_work_is_revived_only_by_a_claimed_nonterminal_run() {
    let session_is_terminal: bool = kani::any();
    let lease_was_acquired: bool = kani::any();
    let claimed_run: bool = kani::any();
    let acquisition = if claimed_run {
        SessionWorkAcquisition::ClaimedRun
    } else {
        SessionWorkAcquisition::RealizationRenewal
    };

    assert_eq!(
        session_work_revival_is_authorized(acquisition, session_is_terminal, lease_was_acquired,),
        claimed_run && !session_is_terminal && !lease_was_acquired
    );
}

#[cfg(kani)]
mod verification {
    use super::*;
    /// @obligation session_run.execution_requires_dispatch_work_and_realization_owner
    #[kani::proof]
    fn session_work_effect_requires_not_required_scope_or_exact_owner() {
        // Complete policy domain, entered through the production wrapper so the
        // owner fact is the one the code derives rather than a free symbol:
        // every closed observed Work-authority class crossed with an exact and
        // a foreign owner spelling. Causes: observed ownership kind and whether
        // the authenticated owner is the lease's own. Effect: only a Work-free
        // scope, or a lease whose owner is exactly the authenticated one,
        // permits a realization effect.
        // Bounds: the observed ownership *kind* is enumerated concretely for the
        // same reason as the receipt harness -- a symbolic variant carrying a
        // String payload does not terminate -- while the owner comparison the
        // wrapper actually performs stays symbolic.
        let owner_matches: bool = kani::any();
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
