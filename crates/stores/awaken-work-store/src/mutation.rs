// @formal entrypoint work.lease_authority_snapshot_and_owner_mutations_are_atomic
// @formal entrypoint work.persisted_epoch_is_exact_nonnegative_and_nonwrapping
//! Pure mutation helpers shared by the durable WorkQueue adapters.

use std::collections::BTreeMap;

use awaken_lease_contract::fence::Epoch;
use awaken_sql_storage::StoredU64;
use awaken_worker_contract::{WorkQueueError, WorkQueueError::Storage};

pub(super) fn apply_metadata_patch(
    metadata: &mut BTreeMap<String, String>,
    patch: BTreeMap<String, Option<String>>,
) {
    for (key, value) in patch {
        match value {
            Some(value) => {
                metadata.insert(key, value);
            }
            None => {
                metadata.remove(&key);
            }
        }
    }
}

/// Decode the persisted `lease_epoch` column. A negative value is corrupt; `0`
/// is the column's value before any claim, i.e. no lease was ever granted;
/// every other value is the generation of the current (or last) grant.
pub(super) fn stored_epoch(current: i64) -> Result<Option<Epoch>, WorkQueueError> {
    Ok(Epoch::from_persisted(
        StoredU64::try_from(current)
            .map_err(|_| Storage("negative work lease epoch".into()))?
            .domain_value(),
    ))
}

/// The generation of an active, token-bound Session lease row. A live bearer
/// row at epoch 0 was never granted and is corrupt, not an absent lease.
pub(super) fn granted_session_epoch(stored: i64) -> Result<Epoch, WorkQueueError> {
    stored_epoch(stored)?.ok_or_else(|| Storage("active Session Work has no lease epoch".into()))
}

/// The generation a claim grants over `current`: the first grant for an
/// item never leased, otherwise [`Epoch::next`]. `None` once exhausted.
pub(super) fn claimed_epoch(current: Option<Epoch>) -> Option<Epoch> {
    current.map_or(Some(Epoch::FIRST), Epoch::next)
}

/// Which lease generation an owner mutation is fenced to.
///
/// Consumer: every Work acknowledgement, heartbeat, stop and renewal. The
/// standard self-hosted Worker protocol names its lease by worker id only (its
/// requests carry no epoch), so its mutations are [`LeaseFence::OwnerOnly`]:
/// an owner id that lost its lease and re-claimed the item is
/// indistinguishable to it, and the only further fence it has is the
/// heartbeat receipt it echoes. Every caller that holds a generation (a
/// sessions token, a Session Work lease) uses [`LeaseFence::Exact`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LeaseFence {
    Exact(Epoch),
    OwnerOnly,
}

impl LeaseFence {
    /// Whether a lease currently at `current` is the one this fence names.
    pub(super) fn admits(self, current: Option<Epoch>) -> bool {
        match self {
            Self::Exact(expected) => current == Some(expected),
            Self::OwnerOnly => true,
        }
    }
}

/// The complete lease authority value changed by every Work claim, renewal,
/// reclaim, and release.  Stores keep the transaction/lock as the linearization
/// boundary; this pure kernel owns which tuple may be installed inside it.
/// `epoch` is `None` only before the first grant; release and reclaim keep the
/// last generation so the next claim advances past it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct LeaseAuthorityTuple<O, T> {
    pub(super) owner: Option<O>,
    pub(super) epoch: Option<Epoch>,
    pub(super) refreshed_at: Option<T>,
    pub(super) expires_at: Option<T>,
}

impl<O, T> LeaseAuthorityTuple<O, T> {
    /// The generation of an authority a claim or renewal just installed.
    pub(super) fn granted_epoch(&self) -> Result<Epoch, WorkQueueError> {
        self.epoch
            .ok_or_else(|| Storage("installed work lease authority has no epoch".into()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum LeaseAuthorityCommand<O, T> {
    Claim {
        owner: O,
        refreshed_at: T,
        expires_at: T,
    },
    Renew {
        owner: O,
        fence: LeaseFence,
        refreshed_at: T,
        expires_at: T,
    },
    Reclaim,
    Release {
        owner: O,
        fence: LeaseFence,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum LeaseAuthorityDecision<O, T> {
    Apply(LeaseAuthorityTuple<O, T>),
    Reject,
    Exhausted,
}

pub(super) fn lease_authority_transition<O: Eq, T>(
    current: LeaseAuthorityTuple<O, T>,
    command: LeaseAuthorityCommand<O, T>,
) -> LeaseAuthorityDecision<O, T> {
    match command {
        LeaseAuthorityCommand::Claim {
            owner,
            refreshed_at,
            expires_at,
        } => match claimed_epoch(current.epoch) {
            Some(epoch) => LeaseAuthorityDecision::Apply(LeaseAuthorityTuple {
                owner: Some(owner),
                epoch: Some(epoch),
                refreshed_at: Some(refreshed_at),
                expires_at: Some(expires_at),
            }),
            None => LeaseAuthorityDecision::Exhausted,
        },
        LeaseAuthorityCommand::Renew {
            owner,
            fence,
            refreshed_at,
            expires_at,
        } => {
            let complete = current.owner.as_ref() == Some(&owner)
                && current.epoch.is_some()
                && current.refreshed_at.is_some()
                && current.expires_at.is_some();
            if !complete || !fence.admits(current.epoch) {
                return LeaseAuthorityDecision::Reject;
            }
            LeaseAuthorityDecision::Apply(LeaseAuthorityTuple {
                owner: current.owner,
                epoch: current.epoch,
                refreshed_at: Some(refreshed_at),
                expires_at: Some(expires_at),
            })
        }
        LeaseAuthorityCommand::Reclaim => LeaseAuthorityDecision::Apply(LeaseAuthorityTuple {
            owner: None,
            epoch: current.epoch,
            refreshed_at: None,
            expires_at: None,
        }),
        LeaseAuthorityCommand::Release { owner, fence } => {
            if current.owner.as_ref() != Some(&owner)
                || current.epoch.is_none()
                || !fence.admits(current.epoch)
            {
                return LeaseAuthorityDecision::Reject;
            }
            LeaseAuthorityDecision::Apply(LeaseAuthorityTuple {
                owner: None,
                epoch: current.epoch,
                refreshed_at: None,
                expires_at: None,
            })
        }
    }
}

pub(super) fn stored_lease_authority(
    owner: Option<String>,
    epoch: i64,
    refreshed_at: Option<i64>,
    expires_at: Option<i64>,
) -> Result<LeaseAuthorityTuple<String, i64>, WorkQueueError> {
    Ok(LeaseAuthorityTuple {
        owner,
        epoch: stored_epoch(epoch)?,
        refreshed_at,
        expires_at,
    })
}

/// Encode a granted generation into the signed `lease_epoch` column, refusing
/// one the column cannot hold rather than wrapping it negative.
pub(super) fn stored_lease_epoch(epoch: Epoch) -> Result<i64, WorkQueueError> {
    i64::try_from(epoch.get()).map_err(|_| Storage("work lease epoch exhausted".into()))
}

pub(super) fn claim_stored_lease_authority(
    owner: Option<String>,
    epoch: i64,
    refreshed_at: Option<i64>,
    expires_at: Option<i64>,
    claimant: String,
    next_refreshed_at: i64,
    next_expires_at: i64,
) -> Result<LeaseAuthorityTuple<String, i64>, WorkQueueError> {
    match lease_authority_transition(
        stored_lease_authority(owner, epoch, refreshed_at, expires_at)?,
        LeaseAuthorityCommand::Claim {
            owner: claimant,
            refreshed_at: next_refreshed_at,
            expires_at: next_expires_at,
        },
    ) {
        LeaseAuthorityDecision::Apply(next) => Ok(next),
        _ => Err(Storage("work lease epoch exhausted".into())),
    }
}

pub(super) struct StoredLeaseAuthorityInput {
    pub(super) owner: Option<String>,
    pub(super) epoch: i64,
    pub(super) refreshed_at: Option<i64>,
    pub(super) expires_at: Option<i64>,
}

pub(super) fn renew_stored_lease_authority(
    current: StoredLeaseAuthorityInput,
    owner: String,
    fence: LeaseFence,
    refreshed_at: i64,
    expires_at: i64,
) -> Result<Option<LeaseAuthorityTuple<String, i64>>, WorkQueueError> {
    Ok(
        match lease_authority_transition(
            stored_lease_authority(
                current.owner,
                current.epoch,
                current.refreshed_at,
                current.expires_at,
            )?,
            LeaseAuthorityCommand::Renew {
                owner,
                fence,
                refreshed_at,
                expires_at,
            },
        ) {
            LeaseAuthorityDecision::Apply(next) => Some(next),
            _ => None,
        },
    )
}

pub(super) fn release_stored_lease_authority(
    current: StoredLeaseAuthorityInput,
    owner: String,
    fence: LeaseFence,
) -> Result<Option<LeaseAuthorityTuple<String, i64>>, WorkQueueError> {
    Ok(
        match lease_authority_transition(
            stored_lease_authority(
                current.owner,
                current.epoch,
                current.refreshed_at,
                current.expires_at,
            )?,
            LeaseAuthorityCommand::Release { owner, fence },
        ) {
            LeaseAuthorityDecision::Apply(next) => Some(next),
            _ => None,
        },
    )
}

pub(super) fn reclaimed_lease_authority() -> LeaseAuthorityTuple<String, i64> {
    match lease_authority_transition(
        LeaseAuthorityTuple {
            owner: None,
            epoch: None,
            refreshed_at: None,
            expires_at: None,
        },
        LeaseAuthorityCommand::Reclaim,
    ) {
        LeaseAuthorityDecision::Apply(next) => next,
        _ => unreachable!("reclaim always yields one cleared tuple"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epoch(value: u64) -> Option<Epoch> {
        Epoch::from_persisted(value)
    }

    #[test]
    fn authority_transition_covers_claim_renew_reclaim_and_release() {
        // Cause/effect decision table: C1 claim on epoch 4 -> E1 complete
        // owner/epoch-5/window; C2 exact renew -> E2 same owner/epoch and new
        // whole window; C3 wrong-owner release -> E3 Reject/stutter; C4 exact
        // release -> E4 one complete clear; C5 reclaim any complete authority
        // -> E5 one complete clear; C6 first claim of a never-leased item ->
        // Epoch::FIRST. Exhaustion is covered over full u64 by Kani.
        let never = LeaseAuthorityTuple::<&str, i32> {
            owner: None,
            epoch: None,
            refreshed_at: None,
            expires_at: None,
        };
        let LeaseAuthorityDecision::Apply(first) = lease_authority_transition(
            never,
            LeaseAuthorityCommand::Claim {
                owner: "worker-a",
                refreshed_at: 1,
                expires_at: 2,
            },
        ) else {
            panic!("C6")
        };
        assert_eq!(first.epoch, Some(Epoch::FIRST), "C6");
        let clear = LeaseAuthorityTuple {
            owner: None,
            epoch: epoch(4),
            refreshed_at: None,
            expires_at: None,
        };
        let LeaseAuthorityDecision::Apply(claimed) = lease_authority_transition(
            clear,
            LeaseAuthorityCommand::Claim {
                owner: "worker-a",
                refreshed_at: 10,
                expires_at: 20,
            },
        ) else {
            panic!("C1/E1")
        };
        assert_eq!(claimed.epoch, epoch(5), "C1/E1");
        let LeaseAuthorityDecision::Apply(renewed) = lease_authority_transition(
            claimed,
            LeaseAuthorityCommand::Renew {
                owner: "worker-a",
                fence: LeaseFence::Exact(epoch(5).unwrap()),
                refreshed_at: 11,
                expires_at: 21,
            },
        ) else {
            panic!("C2/E2")
        };
        assert_eq!(
            (
                renewed.owner,
                renewed.epoch,
                renewed.refreshed_at,
                renewed.expires_at
            ),
            (Some("worker-a"), epoch(5), Some(11), Some(21)),
            "C2/E2"
        );
        assert_eq!(
            lease_authority_transition(
                renewed.clone(),
                LeaseAuthorityCommand::Release {
                    owner: "worker-b",
                    fence: LeaseFence::Exact(epoch(5).unwrap()),
                },
            ),
            LeaseAuthorityDecision::Reject,
            "C3/E3"
        );
        let LeaseAuthorityDecision::Apply(released) = lease_authority_transition(
            renewed.clone(),
            LeaseAuthorityCommand::Release {
                owner: "worker-a",
                fence: LeaseFence::Exact(epoch(5).unwrap()),
            },
        ) else {
            panic!("C4/E4")
        };
        assert_eq!(
            released,
            LeaseAuthorityTuple {
                owner: None,
                epoch: epoch(5),
                refreshed_at: None,
                expires_at: None
            },
            "C4/E4"
        );
        assert_eq!(
            lease_authority_transition(renewed, LeaseAuthorityCommand::Reclaim),
            LeaseAuthorityDecision::Apply(LeaseAuthorityTuple {
                owner: None,
                epoch: epoch(5),
                refreshed_at: None,
                expires_at: None
            }),
            "C5/E5"
        );
    }

    #[test]
    fn lease_epoch_conversion_fails_closed_at_corrupt_and_exhausted_boundaries() {
        // Cause/effect graph: C1 persisted epoch is negative/corrupt; C2 epoch
        // is the largest SQL BIGINT and a new owner requires advancement; C3
        // the same largest epoch is only being read/renewed; C4 the column's
        // pre-claim zero. Effects: E1/C1 and E1/C2 return typed Storage errors
        // without zeroing or reusing a fence; E2/C3 preserves the current
        // fence; E3/C4 reads as never granted and a claim grants the first
        // epoch. Decision rows: X1=C1->E1, X2=C2->E1, X3=C3->E2, X4=C4->E3.
        assert!(
            matches!(stored_epoch(-1), Err(WorkQueueError::Storage(_))),
            "X1"
        );
        let top = stored_epoch(i64::MAX).expect("X3").expect("X3 granted");
        assert!(
            matches!(
                claimed_epoch(Some(top)).map(stored_lease_epoch),
                Some(Err(WorkQueueError::Storage(_)))
            ),
            "X2"
        );
        assert_eq!(top.get(), i64::MAX as u64, "X3");
        assert_eq!(stored_lease_epoch(top).expect("X3"), i64::MAX, "X3");
        assert_eq!(stored_epoch(0).expect("X4"), None, "X4");
        assert_eq!(claimed_epoch(None), Some(Epoch::FIRST), "X4");
    }

    #[test]
    fn a_renewal_fence_names_its_generation_or_says_it_has_none() {
        // Owner "w" re-claimed the item: the lease is now at epoch 6.
        // | Rule | fence            | owner | effect                               |
        // | F1   | Exact(6)         | w     | renew                                |
        // | F2   | Exact(5)         | w     | reject: a predecessor's heartbeat    |
        // | F3   | OwnerOnly        | w     | renew: the standard protocol has no  |
        // |      |                  |       | epoch, so its owner id is the fence  |
        // | F4   | OwnerOnly        | x     | reject                               |
        // | F5   | Exact(0)         | -     | unrepresentable: zero never granted  |
        let held = LeaseAuthorityTuple {
            owner: Some("w"),
            epoch: epoch(6),
            refreshed_at: Some(1),
            expires_at: Some(2),
        };
        let renew = |owner, fence| {
            lease_authority_transition(
                held.clone(),
                LeaseAuthorityCommand::Renew {
                    owner,
                    fence,
                    refreshed_at: 3,
                    expires_at: 4,
                },
            )
        };
        assert!(
            matches!(
                renew("w", LeaseFence::Exact(epoch(6).unwrap())),
                LeaseAuthorityDecision::Apply(_)
            ),
            "F1"
        );
        assert_eq!(
            renew("w", LeaseFence::Exact(epoch(5).unwrap())),
            LeaseAuthorityDecision::Reject,
            "F2"
        );
        assert!(
            matches!(
                renew("w", LeaseFence::OwnerOnly),
                LeaseAuthorityDecision::Apply(_)
            ),
            "F3"
        );
        assert_eq!(
            renew("x", LeaseFence::OwnerOnly),
            LeaseAuthorityDecision::Reject,
            "F4"
        );
        assert_eq!(epoch(0).map(LeaseFence::Exact), None, "F5");
    }
}

#[cfg(kani)]
/// @obligation work.persisted_epoch_is_exact_nonnegative_and_nonwrapping
#[kani::proof]
fn work_lease_epoch_is_exact_nonnegative_and_nonwrapping() {
    // Full i64 domain, no artificial collection bound or SQL stubs.
    // X1 negative -> reject; X2 zero -> never granted, the claim grants 1;
    // X3 positive -> that exact generation, the claim grants its successor;
    // X4 a successor beyond the signed column -> refused, never wrapped.
    // The i128 oracle is independent of the production StoredU64/Epoch
    // arithmetic. This proves the numeric transition used by both claim
    // paths, not database locking, bearer entropy, or whole-queue
    // linearizability.
    let current: i64 = kani::any();
    match stored_epoch(current) {
        Ok(decoded) => {
            assert!(current >= 0);
            assert!(decoded.map_or(0, Epoch::get) as i128 == i128::from(current));
            match claimed_epoch(decoded).map(stored_lease_epoch) {
                Some(Ok(database)) => {
                    assert!(i128::from(database) == i128::from(current) + 1);
                }
                Some(Err(WorkQueueError::Storage(_))) | None => {
                    assert!(current == i64::MAX);
                }
            }
        }
        Err(WorkQueueError::Storage(_)) => assert!(current < 0),
    }
}

#[cfg(kani)]
/// @obligation work.lease_authority_snapshot_and_owner_mutations_are_atomic
#[kani::proof]
fn work_lease_authority_transition_is_complete_and_fenced() {
    // Cause/effect table over the full finite tuple/command domain:
    // R1 Claim + epoch room -> one complete tuple and exact successor epoch;
    // R2 Claim + MAX -> Exhausted and therefore no installable owner;
    // R3 Renew + exact complete owner[/epoch] -> only timestamps change;
    // R4 Renew mismatch/incomplete -> Reject; R5 Reclaim -> complete clear;
    // R6 Release exact -> complete clear; R7 Release mismatch -> Reject.
    // Database/Mutex linearizability and physical durability remain the TCB.
    let current = LeaseAuthorityTuple {
        owner: kani::any::<Option<u8>>(),
        epoch: Epoch::from_persisted(kani::any::<u64>()),
        refreshed_at: kani::any::<Option<u8>>(),
        expires_at: kani::any::<Option<u8>>(),
    };
    let original = current.clone();
    let tag: u8 = kani::any();
    kani::assume(tag < 4);
    let owner = kani::any::<u8>();
    let refreshed_at = kani::any::<u8>();
    let expires_at = kani::any::<u8>();
    let fence = match Epoch::from_persisted(kani::any::<u64>()) {
        Some(expected) if kani::any::<bool>() => LeaseFence::Exact(expected),
        _ => LeaseFence::OwnerOnly,
    };
    let command = match tag {
        0 => LeaseAuthorityCommand::Claim {
            owner,
            refreshed_at,
            expires_at,
        },
        1 => LeaseAuthorityCommand::Renew {
            owner,
            fence,
            refreshed_at,
            expires_at,
        },
        2 => LeaseAuthorityCommand::Reclaim,
        _ => LeaseAuthorityCommand::Release { owner, fence },
    };
    let decision = lease_authority_transition(current, command);
    match decision {
        LeaseAuthorityDecision::Apply(next) => {
            let complete = next.owner.is_some()
                && next.epoch.is_some()
                && next.refreshed_at.is_some()
                && next.expires_at.is_some();
            let clear =
                next.owner.is_none() && next.refreshed_at.is_none() && next.expires_at.is_none();
            assert!(complete || clear);
            if tag == 0 {
                assert_eq!(next.epoch, claimed_epoch(original.epoch));
            } else {
                assert_eq!(next.epoch, original.epoch);
            }
            if tag == 1 || tag == 3 {
                assert!(original.owner == Some(owner) && fence.admits(original.epoch));
            }
        }
        LeaseAuthorityDecision::Reject => assert!(tag == 1 || tag == 3),
        LeaseAuthorityDecision::Exhausted => {
            assert!(tag == 0 && original.epoch.map(Epoch::get) == Some(u64::MAX))
        }
    }
}
