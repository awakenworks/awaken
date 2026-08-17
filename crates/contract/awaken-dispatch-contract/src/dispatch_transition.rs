//! Backend-neutral state transitions for one durable dispatch row.
//!
//! Storage adapters remain responsible for atomic compare-and-set and durable
//! persistence. This kernel defines the state/epoch decision they must apply.

/// Storage-independent lifecycle of one durable dispatch row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchPhase {
    Pending,
    Leased,
    Awaiting,
    DeadLetter,
    Superseded,
}

/// The authority-bearing portion of one durable dispatch row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchTransition {
    pub phase: DispatchPhase,
    pub lease_epoch: u64,
    pub cancellation_requested: bool,
}

/// A transition guarded by the current claim was rejected or applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardedTransition {
    Fenced,
    Applied(DispatchTransition),
    Removed,
}

/// Result of requesting cancellation for one row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelTransition {
    NotCancellable,
    Applied {
        state: DispatchTransition,
        revoked_lease: bool,
    },
}

/// The only bounded arithmetic failure in the transition kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchTransitionError {
    LeaseEpochExhausted,
}

/// Whether one durable dispatch is eligible for retry-exhaustion terminal
/// claiming. Store queries may prefilter candidates, but every backend applies
/// this kernel to the transactionally read evidence before advancing the epoch.
#[must_use]
pub fn retry_exhaustion_eligible(
    phase: DispatchPhase,
    lease_until_ms: Option<u64>,
    attempt_count: u64,
    max_attempts: u64,
    now_ms: u64,
) -> bool {
    phase == DispatchPhase::Leased
        && attempt_count >= max_attempts
        && lease_until_ms.is_some_and(|lease_until_ms| lease_until_ms < now_ms)
}

impl crate::DispatchState {
    /// Storage-neutral phase used by transactionally rechecked policy kernels.
    #[must_use]
    pub fn transition_phase(self) -> DispatchPhase {
        match self {
            Self::Pending => DispatchPhase::Pending,
            Self::Leased => DispatchPhase::Leased,
            Self::Awaiting => DispatchPhase::Awaiting,
            Self::DeadLetter => DispatchPhase::DeadLetter,
            Self::Superseded => DispatchPhase::Superseded,
        }
    }
}

impl DispatchTransition {
    /// Mint exactly one newer claim epoch after the caller has established that
    /// this pending, awaiting, or recovery row is runnable.
    pub fn claim(self) -> Result<Option<Self>, DispatchTransitionError> {
        if !matches!(
            self.phase,
            DispatchPhase::Pending | DispatchPhase::Leased | DispatchPhase::Awaiting
        ) {
            return Ok(None);
        }
        let lease_epoch = self
            .lease_epoch
            .checked_add(1)
            .ok_or(DispatchTransitionError::LeaseEpochExhausted)?;
        Ok(Some(Self {
            phase: DispatchPhase::Leased,
            lease_epoch,
            cancellation_requested: self.cancellation_requested,
        }))
    }

    /// Apply a settlement only to the exact current leased epoch. `done` removes
    /// the row; awaiting preserves its cancellation intent and epoch.
    pub fn settle(self, claim_epoch: u64, done: bool) -> GuardedTransition {
        if self.phase != DispatchPhase::Leased || self.lease_epoch != claim_epoch {
            return GuardedTransition::Fenced;
        }
        if done {
            GuardedTransition::Removed
        } else {
            GuardedTransition::Applied(Self {
                phase: DispatchPhase::Awaiting,
                ..self
            })
        }
    }

    /// Return an exact current claim to pending. Owner equality is supplied by
    /// the adapter because owner representation is deliberately not in this
    /// storage-neutral kernel.
    pub fn relinquish(self, claim_epoch: u64, owner_matches: bool) -> GuardedTransition {
        if self.phase != DispatchPhase::Leased || self.lease_epoch != claim_epoch || !owner_matches
        {
            return GuardedTransition::Fenced;
        }
        GuardedTransition::Applied(Self {
            phase: DispatchPhase::Pending,
            ..self
        })
    }

    /// Record cancellation. A live lease is revoked by minting the next epoch;
    /// repeated cancellation of the resulting pending row is a state stutter.
    pub fn cancel(self) -> Result<CancelTransition, DispatchTransitionError> {
        match self.phase {
            DispatchPhase::Pending | DispatchPhase::Awaiting => Ok(CancelTransition::Applied {
                state: Self {
                    cancellation_requested: true,
                    ..self
                },
                revoked_lease: false,
            }),
            DispatchPhase::Leased => {
                let lease_epoch = self
                    .lease_epoch
                    .checked_add(1)
                    .ok_or(DispatchTransitionError::LeaseEpochExhausted)?;
                Ok(CancelTransition::Applied {
                    state: Self {
                        phase: DispatchPhase::Pending,
                        lease_epoch,
                        cancellation_requested: true,
                    },
                    revoked_lease: true,
                })
            }
            DispatchPhase::DeadLetter | DispatchPhase::Superseded => {
                Ok(CancelTransition::NotCancellable)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leased(epoch: u64) -> DispatchTransition {
        DispatchTransition {
            phase: DispatchPhase::Leased,
            lease_epoch: epoch,
            cancellation_requested: false,
        }
    }

    #[test]
    fn stale_settle_and_relinquish_are_fenced() {
        let current = leased(8);
        assert_eq!(current.settle(7, true), GuardedTransition::Fenced);
        assert_eq!(current.relinquish(8, false), GuardedTransition::Fenced);
        assert_eq!(current.relinquish(7, true), GuardedTransition::Fenced);
    }

    #[test]
    fn exact_settlement_has_only_terminal_or_awaiting_result() {
        let current = leased(8);
        assert_eq!(current.settle(8, true), GuardedTransition::Removed);
        assert_eq!(
            current.settle(8, false),
            GuardedTransition::Applied(DispatchTransition {
                phase: DispatchPhase::Awaiting,
                ..current
            })
        );
    }

    #[test]
    fn cancellation_revokes_a_live_epoch_and_is_then_idempotent() {
        let old = leased(8);
        let CancelTransition::Applied {
            state: cancelled,
            revoked_lease,
        } = old.cancel().unwrap()
        else {
            panic!("leased row must be cancellable")
        };
        assert!(revoked_lease);
        assert_eq!(cancelled.phase, DispatchPhase::Pending);
        assert_eq!(cancelled.lease_epoch, 9);
        assert_eq!(old.settle(8, true), GuardedTransition::Removed);
        assert_eq!(cancelled.settle(8, true), GuardedTransition::Fenced);
        assert_eq!(
            cancelled.cancel().unwrap(),
            CancelTransition::Applied {
                state: cancelled,
                revoked_lease: false,
            }
        );
    }

    /// Retry-exhaustion cause/effect graph:
    /// C1=phase is Leased; C2=lease expiry is present and strictly before now;
    /// C3=attempt_count reaches max_attempts. E1=eligible for the one terminal
    /// claim. Constraints: an exact expiry boundary is still live, and retrying
    /// terminalization may increase attempts beyond the threshold without
    /// becoming ineligible.
    ///
    /// | Rule | C1 | C2 | C3 | Effect |
    /// |---|---|---|---|---|
    /// | R1 | T | T | T | E1 |
    /// | R2 | F | T | T | not eligible |
    /// | R3 | T | F/missing | T | not eligible |
    /// | R4 | T | T | F | not eligible |
    #[test]
    fn retry_exhaustion_eligibility_follows_the_shared_decision_table() {
        assert!(retry_exhaustion_eligible(
            DispatchPhase::Leased,
            Some(9),
            3,
            3,
            10
        ));
        assert!(retry_exhaustion_eligible(
            DispatchPhase::Leased,
            Some(9),
            4,
            3,
            10
        ));
        assert!(!retry_exhaustion_eligible(
            DispatchPhase::Awaiting,
            Some(9),
            3,
            3,
            10
        ));
        assert!(!retry_exhaustion_eligible(
            DispatchPhase::Leased,
            None,
            3,
            3,
            10
        ));
        assert!(!retry_exhaustion_eligible(
            DispatchPhase::Leased,
            Some(10),
            3,
            3,
            10
        ));
        assert!(!retry_exhaustion_eligible(
            DispatchPhase::Leased,
            Some(9),
            2,
            3,
            10
        ));
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    fn arbitrary_phase() -> DispatchPhase {
        match kani::any::<u8>() % 5 {
            0 => DispatchPhase::Pending,
            1 => DispatchPhase::Leased,
            2 => DispatchPhase::Awaiting,
            3 => DispatchPhase::DeadLetter,
            _ => DispatchPhase::Superseded,
        }
    }

    fn arbitrary_state() -> DispatchTransition {
        DispatchTransition {
            phase: arbitrary_phase(),
            lease_epoch: kani::any(),
            cancellation_requested: kani::any(),
        }
    }

    #[kani::proof]
    fn stale_dispatch_claim_cannot_modify_authoritative_state() {
        let state = arbitrary_state();
        let stale_epoch = kani::any();
        kani::assume(stale_epoch != state.lease_epoch);
        assert_eq!(
            state.settle(stale_epoch, kani::any()),
            GuardedTransition::Fenced
        );
        assert_eq!(
            state.relinquish(stale_epoch, kani::any()),
            GuardedTransition::Fenced
        );
    }

    #[kani::proof]
    fn exact_dispatch_settlement_is_terminal_or_awaiting_only() {
        let state = arbitrary_state();
        let done = kani::any();
        let result = state.settle(state.lease_epoch, done);
        if state.phase != DispatchPhase::Leased {
            assert_eq!(result, GuardedTransition::Fenced);
        } else if done {
            assert_eq!(result, GuardedTransition::Removed);
        } else {
            assert_eq!(
                result,
                GuardedTransition::Applied(DispatchTransition {
                    phase: DispatchPhase::Awaiting,
                    ..state
                })
            );
        }
    }

    #[kani::proof]
    fn dispatch_cancel_revokes_old_epoch_and_is_idempotent() {
        let state = arbitrary_state();
        kani::assume(state.lease_epoch < u64::MAX);
        let first = state.cancel().unwrap();
        match first {
            CancelTransition::NotCancellable => {
                assert!(matches!(
                    state.phase,
                    DispatchPhase::DeadLetter | DispatchPhase::Superseded
                ));
            }
            CancelTransition::Applied {
                state: cancelled,
                revoked_lease,
            } => {
                assert!(cancelled.cancellation_requested);
                if state.phase == DispatchPhase::Leased {
                    assert!(revoked_lease);
                    assert_eq!(cancelled.phase, DispatchPhase::Pending);
                    assert_eq!(cancelled.lease_epoch, state.lease_epoch + 1);
                    assert_eq!(
                        cancelled.settle(state.lease_epoch, kani::any()),
                        GuardedTransition::Fenced
                    );
                    assert_eq!(
                        cancelled.relinquish(state.lease_epoch, true),
                        GuardedTransition::Fenced
                    );
                } else {
                    assert!(!revoked_lease);
                    assert_eq!(cancelled.lease_epoch, state.lease_epoch);
                    assert_eq!(cancelled.phase, state.phase);
                }
                assert_eq!(
                    cancelled.cancel().unwrap(),
                    CancelTransition::Applied {
                        state: cancelled,
                        revoked_lease: false,
                    }
                );
            }
        }
    }

    #[kani::proof]
    fn dispatch_claim_mints_exactly_one_epoch_and_never_reopens_closed_rows() {
        let state = arbitrary_state();
        let result = state.claim();
        if matches!(
            state.phase,
            DispatchPhase::DeadLetter | DispatchPhase::Superseded
        ) {
            assert_eq!(result, Ok(None));
        } else if state.lease_epoch == u64::MAX {
            assert_eq!(result, Err(DispatchTransitionError::LeaseEpochExhausted));
        } else {
            assert_eq!(
                result,
                Ok(Some(DispatchTransition {
                    phase: DispatchPhase::Leased,
                    lease_epoch: state.lease_epoch + 1,
                    cancellation_requested: state.cancellation_requested,
                }))
            );
        }
    }
}
