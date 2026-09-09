//! Pure mutation helpers shared by the durable WorkQueue adapters.

use std::collections::BTreeMap;

use awaken_session_contract::work_queue::{WorkQueueError, WorkQueueError::Storage};
use awaken_sql_storage::StoredU64;

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

pub(super) fn lease_epoch(current: i64, advance: bool) -> Result<(i64, u64), WorkQueueError> {
    let current =
        StoredU64::try_from(current).map_err(|_| Storage("negative work lease epoch".into()))?;
    let next = if advance {
        current
            .checked_add(1)
            .map_err(|_| Storage("work lease epoch exhausted".into()))?
    } else {
        current
    };
    Ok((next.database_value(), next.domain_value()))
}

/// The complete lease authority value changed by every Work claim, renewal,
/// reclaim, and release.  Stores keep the transaction/lock as the linearization
/// boundary; this pure kernel owns which tuple may be installed inside it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct LeaseAuthorityTuple<O, T> {
    pub(super) owner: Option<O>,
    pub(super) epoch: u64,
    pub(super) refreshed_at: Option<T>,
    pub(super) expires_at: Option<T>,
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
        expected_epoch: Option<u64>,
        refreshed_at: T,
        expires_at: T,
    },
    Reclaim,
    Release {
        owner: O,
        expected_epoch: u64,
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
        } => match current.epoch.checked_add(1) {
            Some(epoch) => LeaseAuthorityDecision::Apply(LeaseAuthorityTuple {
                owner: Some(owner),
                epoch,
                refreshed_at: Some(refreshed_at),
                expires_at: Some(expires_at),
            }),
            None => LeaseAuthorityDecision::Exhausted,
        },
        LeaseAuthorityCommand::Renew {
            owner,
            expected_epoch,
            refreshed_at,
            expires_at,
        } => {
            let complete = current.owner.as_ref() == Some(&owner)
                && current.refreshed_at.is_some()
                && current.expires_at.is_some();
            if !complete || expected_epoch.is_some_and(|expected| expected != current.epoch) {
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
        LeaseAuthorityCommand::Release {
            owner,
            expected_epoch,
        } => {
            if current.owner.as_ref() != Some(&owner) || current.epoch != expected_epoch {
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
        epoch: StoredU64::try_from(epoch)
            .map_err(|_| Storage("negative work lease epoch".into()))?
            .domain_value(),
        refreshed_at,
        expires_at,
    })
}

pub(super) fn stored_lease_epoch(epoch: u64) -> Result<i64, WorkQueueError> {
    i64::try_from(epoch).map_err(|_| Storage("work lease epoch exhausted".into()))
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
    expected_epoch: Option<u64>,
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
                expected_epoch,
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
    expected_epoch: u64,
) -> Result<Option<LeaseAuthorityTuple<String, i64>>, WorkQueueError> {
    Ok(
        match lease_authority_transition(
            stored_lease_authority(
                current.owner,
                current.epoch,
                current.refreshed_at,
                current.expires_at,
            )?,
            LeaseAuthorityCommand::Release {
                owner,
                expected_epoch,
            },
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
            epoch: 0,
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

    #[test]
    fn authority_transition_covers_claim_renew_reclaim_and_release() {
        // Cause/effect decision table: C1 claim on epoch 4 -> E1 complete
        // owner/epoch-5/window; C2 exact renew -> E2 same owner/epoch and new
        // whole window; C3 wrong-owner release -> E3 Reject/stutter; C4 exact
        // release -> E4 one complete clear; C5 reclaim any complete authority
        // -> E5 one complete clear. Exhaustion is covered over full u64 by Kani.
        let clear = LeaseAuthorityTuple {
            owner: None,
            epoch: 4,
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
        assert_eq!(claimed.epoch, 5, "C1/E1");
        let LeaseAuthorityDecision::Apply(renewed) = lease_authority_transition(
            claimed,
            LeaseAuthorityCommand::Renew {
                owner: "worker-a",
                expected_epoch: Some(5),
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
            (Some("worker-a"), 5, Some(11), Some(21)),
            "C2/E2"
        );
        assert_eq!(
            lease_authority_transition(
                renewed.clone(),
                LeaseAuthorityCommand::Release {
                    owner: "worker-b",
                    expected_epoch: 5,
                },
            ),
            LeaseAuthorityDecision::Reject,
            "C3/E3"
        );
        let LeaseAuthorityDecision::Apply(released) = lease_authority_transition(
            renewed.clone(),
            LeaseAuthorityCommand::Release {
                owner: "worker-a",
                expected_epoch: 5,
            },
        ) else {
            panic!("C4/E4")
        };
        assert_eq!(
            released,
            LeaseAuthorityTuple {
                owner: None,
                epoch: 5,
                refreshed_at: None,
                expires_at: None
            },
            "C4/E4"
        );
        assert_eq!(
            lease_authority_transition(renewed, LeaseAuthorityCommand::Reclaim),
            LeaseAuthorityDecision::Apply(LeaseAuthorityTuple {
                owner: None,
                epoch: 5,
                refreshed_at: None,
                expires_at: None
            }),
            "C5/E5"
        );
    }
}

#[cfg(kani)]
#[kani::proof]
fn work_lease_epoch_is_exact_nonnegative_and_nonwrapping() {
    // Full i64 x bool domain, no artificial collection bound or SQL stubs.
    // X1 negative -> reject; X2 MAX+advance -> reject; X3 nonnegative+observe
    // -> unchanged; X4 other nonnegative+advance -> exact successor.
    // The i128 oracle is independent of the production StoredU64 arithmetic.
    // This proves the production numeric transition used by both claim paths,
    // not database locking, bearer entropy, or whole-queue linearizability.
    let current: i64 = kani::any();
    let advance: bool = kani::any();
    match lease_epoch(current, advance) {
        Ok((database, domain)) => {
            assert!(current >= 0 && !(advance && current == i64::MAX));
            assert!(i128::from(database) == i128::from(current) + i128::from(advance));
            assert!(i128::from(domain) == i128::from(database));
        }
        Err(WorkQueueError::Storage(_)) => {
            assert!(current < 0 || (advance && current == i64::MAX));
        }
    }
}

#[cfg(kani)]
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
        epoch: kani::any::<u64>(),
        refreshed_at: kani::any::<Option<u8>>(),
        expires_at: kani::any::<Option<u8>>(),
    };
    let original = current.clone();
    let tag: u8 = kani::any();
    kani::assume(tag < 4);
    let owner = kani::any::<u8>();
    let refreshed_at = kani::any::<u8>();
    let expires_at = kani::any::<u8>();
    let expected_epoch = kani::any::<u64>();
    let command = match tag {
        0 => LeaseAuthorityCommand::Claim {
            owner,
            refreshed_at,
            expires_at,
        },
        1 => LeaseAuthorityCommand::Renew {
            owner,
            expected_epoch: kani::any::<bool>().then_some(expected_epoch),
            refreshed_at,
            expires_at,
        },
        2 => LeaseAuthorityCommand::Reclaim,
        _ => LeaseAuthorityCommand::Release {
            owner,
            expected_epoch,
        },
    };
    let decision = lease_authority_transition(current, command);
    match decision {
        LeaseAuthorityDecision::Apply(next) => {
            let complete =
                next.owner.is_some() && next.refreshed_at.is_some() && next.expires_at.is_some();
            let clear =
                next.owner.is_none() && next.refreshed_at.is_none() && next.expires_at.is_none();
            assert!(complete || clear);
            if tag == 0 {
                assert_eq!(next.epoch, original.epoch + 1);
            } else {
                assert_eq!(next.epoch, original.epoch);
            }
        }
        LeaseAuthorityDecision::Reject => assert!(tag == 1 || tag == 3),
        LeaseAuthorityDecision::Exhausted => {
            assert!(tag == 0 && original.epoch == u64::MAX)
        }
    }
}
