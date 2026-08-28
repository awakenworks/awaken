//! Backend-neutral state transitions for one durable dispatch row.
//!
//! Storage adapters remain responsible for atomic compare-and-set and durable
//! persistence. This kernel defines the state/epoch decision they must apply.

/// The authority-bearing portion of one durable dispatch row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchTransition {
    pub state: crate::DispatchState,
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
    state: crate::DispatchState,
    lease_until_ms: Option<u64>,
    attempt_count: u64,
    max_attempts: u64,
    now_ms: u64,
) -> bool {
    state == crate::DispatchState::Leased
        && attempt_count >= max_attempts
        && lease_until_ms.is_some_and(|lease_until_ms| lease_until_ms < now_ms)
}

impl DispatchTransition {
    /// Mint exactly one newer claim epoch after the caller has established that
    /// this pending, awaiting, or recovery row is runnable.
    pub fn claim(self) -> Result<Option<Self>, DispatchTransitionError> {
        if !matches!(
            self.state,
            crate::DispatchState::Pending
                | crate::DispatchState::Leased
                | crate::DispatchState::Awaiting
        ) {
            return Ok(None);
        }
        let lease_epoch = self
            .lease_epoch
            .checked_add(1)
            .ok_or(DispatchTransitionError::LeaseEpochExhausted)?;
        Ok(Some(Self {
            state: crate::DispatchState::Leased,
            lease_epoch,
            cancellation_requested: self.cancellation_requested,
        }))
    }

    /// Resolve the exact recovery claim after the Session authority answered.
    /// `Some(true)` publishes the admitted intent to the ordinary Pending path,
    /// `Some(false)` returns it to its unclaimable reservation state, and `None`
    /// removes a definitively rejected intent without fabricating Run completion
    /// truth. Recovery never executes directly; the existing Pending claim is
    /// the only Thread writer/placement/credential admission owner.
    #[must_use]
    pub fn resolve_reservation(
        self,
        claim_epoch: u64,
        owner_matches: bool,
        resolution: Option<bool>,
    ) -> GuardedTransition {
        if self.state != crate::DispatchState::ReservationLeased
            || self.lease_epoch != claim_epoch
            || !owner_matches
        {
            return GuardedTransition::Fenced;
        }
        match resolution {
            Some(true) => GuardedTransition::Applied(Self {
                state: crate::DispatchState::Pending,
                ..self
            }),
            Some(false) => GuardedTransition::Applied(Self {
                state: crate::DispatchState::Reserved,
                ..self
            }),
            None => GuardedTransition::Removed,
        }
    }

    /// Atomically publish an admitted reservation to the ordinary pending queue.
    /// Replays after publication are reported as a state stutter.
    #[must_use]
    pub fn activate_reservation(self) -> Option<Self> {
        (self.state == crate::DispatchState::Reserved).then_some(Self {
            state: crate::DispatchState::Pending,
            ..self
        })
    }

    /// Lease an expired reservation exclusively for admission recovery. This is
    /// deliberately separate from [`Self::claim`], so a newly persisted intent
    /// cannot execute before its Session activity CAS has committed.
    pub fn recover_reservation(self) -> Result<Option<Self>, DispatchTransitionError> {
        if !matches!(
            self.state,
            crate::DispatchState::Reserved | crate::DispatchState::ReservationLeased
        ) {
            return Ok(None);
        }
        let lease_epoch = self
            .lease_epoch
            .checked_add(1)
            .ok_or(DispatchTransitionError::LeaseEpochExhausted)?;
        Ok(Some(Self {
            state: crate::DispatchState::ReservationLeased,
            lease_epoch,
            cancellation_requested: self.cancellation_requested,
        }))
    }

    /// Apply a settlement only to the exact current leased epoch. `done` removes
    /// the row; awaiting preserves its cancellation intent and epoch.
    pub fn settle(self, claim_epoch: u64, done: bool) -> GuardedTransition {
        if self.state != crate::DispatchState::Leased || self.lease_epoch != claim_epoch {
            return GuardedTransition::Fenced;
        }
        if done {
            GuardedTransition::Removed
        } else {
            GuardedTransition::Applied(Self {
                state: crate::DispatchState::Awaiting,
                ..self
            })
        }
    }

    /// Return an exact current claim to pending. Owner equality is supplied by
    /// the adapter because owner representation is deliberately not in this
    /// storage-neutral kernel.
    pub fn relinquish(self, claim_epoch: u64, owner_matches: bool) -> GuardedTransition {
        if self.state != crate::DispatchState::Leased
            || self.lease_epoch != claim_epoch
            || !owner_matches
        {
            return GuardedTransition::Fenced;
        }
        GuardedTransition::Applied(Self {
            state: crate::DispatchState::Pending,
            ..self
        })
    }

    /// Record cancellation. A live lease is revoked by minting the next epoch;
    /// repeated cancellation of the resulting pending row is a state stutter.
    pub fn cancel(self) -> Result<CancelTransition, DispatchTransitionError> {
        match self.state {
            crate::DispatchState::Reserved
            | crate::DispatchState::Pending
            | crate::DispatchState::Awaiting => Ok(CancelTransition::Applied {
                state: Self {
                    cancellation_requested: true,
                    ..self
                },
                revoked_lease: false,
            }),
            crate::DispatchState::ReservationLeased | crate::DispatchState::Leased => {
                let lease_epoch = self
                    .lease_epoch
                    .checked_add(1)
                    .ok_or(DispatchTransitionError::LeaseEpochExhausted)?;
                Ok(CancelTransition::Applied {
                    state: Self {
                        state: if self.state == crate::DispatchState::ReservationLeased {
                            crate::DispatchState::Reserved
                        } else {
                            crate::DispatchState::Pending
                        },
                        lease_epoch,
                        cancellation_requested: true,
                    },
                    revoked_lease: true,
                })
            }
            // DeadLetter retains its pending input and can ordinarily be
            // requeued. A terminal Session fence therefore seals that existing
            // row by reusing the durable cancellation bit without making it
            // runnable or inventing another terminal state.
            crate::DispatchState::DeadLetter => Ok(CancelTransition::Applied {
                state: Self {
                    cancellation_requested: true,
                    ..self
                },
                revoked_lease: false,
            }),
            crate::DispatchState::Superseded => Ok(CancelTransition::NotCancellable),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leased(epoch: u64) -> DispatchTransition {
        DispatchTransition {
            state: crate::DispatchState::Leased,
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
    fn reservation_activation_and_recovery_are_distinct_claim_paths() {
        // Cause/effect graph: C1 a complete dispatch is Reserved or ordinary
        // Pending; C2 Session admission has committed or the reservation owner
        // expired. Effects: E1 ordinary claim cannot lease Reserved; E2 admitted
        // activation publishes Pending without minting a claim epoch; E3 only
        // explicit recovery leases Reserved and advances the epoch; E4 ordinary
        // Pending remains governed by the existing claim path.
        //
        // | Rule | State | Cause | Effect |
        // |---|---|---|---|
        // | R1 | Reserved | ordinary claim | E1 none |
        // | R2 | Reserved | admission commit | E2 Pending/epoch unchanged |
        // | R3 | Reserved | expired recovery | E3 ReservationLeased/epoch+1 |
        // | R4 | Pending | ordinary claim | E4 Leased/epoch+1 |
        // Constraint/Invariant: reservation recovery never enters execution
        // directly and every resolution is fenced by its exact epoch/owner.
        // Decision rule: R1-R4 cover each reachable reservation/ordinary-claim
        // branch; stale resolution branches must remain fenced.
        let reserved = DispatchTransition {
            state: crate::DispatchState::Reserved,
            lease_epoch: 4,
            cancellation_requested: false,
        };
        assert!(reserved.claim().unwrap().is_none(), "R1/E1");
        let activated = reserved.activate_reservation().expect("R2/E2");
        assert_eq!(activated.state, crate::DispatchState::Pending, "R2/E2");
        assert_eq!(activated.lease_epoch, 4, "R2/E2");
        let recovered = reserved.recover_reservation().unwrap().expect("R3/E3");
        assert_eq!(
            recovered.state,
            crate::DispatchState::ReservationLeased,
            "R3/E3"
        );
        assert_eq!(recovered.lease_epoch, 5, "R3/E3");
        assert_eq!(
            recovered.resolve_reservation(5, true, Some(true)),
            GuardedTransition::Applied(DispatchTransition {
                state: crate::DispatchState::Pending,
                ..recovered
            }),
            "R3/E3 admission publishes through the ordinary claim path"
        );
        assert_eq!(
            recovered.resolve_reservation(5, true, Some(false)),
            GuardedTransition::Applied(DispatchTransition {
                state: crate::DispatchState::Reserved,
                ..recovered
            }),
            "R3/E3 transient failure preserves the same intent"
        );
        assert_eq!(
            recovered.resolve_reservation(5, true, None),
            GuardedTransition::Removed,
            "R3/E3 deterministic rejection removes only the unstarted intent"
        );
        let pending = DispatchTransition {
            state: crate::DispatchState::Pending,
            ..reserved
        };
        assert_eq!(
            pending.claim().unwrap().expect("R4/E4").state,
            crate::DispatchState::Leased,
            "R4/E4"
        );
    }

    #[test]
    fn exact_settlement_has_only_terminal_or_awaiting_result() {
        let current = leased(8);
        assert_eq!(current.settle(8, true), GuardedTransition::Removed);
        assert_eq!(
            current.settle(8, false),
            GuardedTransition::Applied(DispatchTransition {
                state: crate::DispatchState::Awaiting,
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
        assert_eq!(cancelled.state, crate::DispatchState::Pending);
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
            crate::DispatchState::Leased,
            Some(9),
            3,
            3,
            10
        ));
        assert!(retry_exhaustion_eligible(
            crate::DispatchState::Leased,
            Some(9),
            4,
            3,
            10
        ));
        assert!(!retry_exhaustion_eligible(
            crate::DispatchState::Awaiting,
            Some(9),
            3,
            3,
            10
        ));
        assert!(!retry_exhaustion_eligible(
            crate::DispatchState::Leased,
            None,
            3,
            3,
            10
        ));
        assert!(!retry_exhaustion_eligible(
            crate::DispatchState::Leased,
            Some(10),
            3,
            3,
            10
        ));
        assert!(!retry_exhaustion_eligible(
            crate::DispatchState::Leased,
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

    fn arbitrary_dispatch_state() -> crate::DispatchState {
        match kani::any::<u8>() % 7 {
            0 => crate::DispatchState::Reserved,
            1 => crate::DispatchState::ReservationLeased,
            2 => crate::DispatchState::Pending,
            3 => crate::DispatchState::Leased,
            4 => crate::DispatchState::Awaiting,
            5 => crate::DispatchState::DeadLetter,
            _ => crate::DispatchState::Superseded,
        }
    }

    fn arbitrary_transition() -> DispatchTransition {
        DispatchTransition {
            state: arbitrary_dispatch_state(),
            lease_epoch: kani::any(),
            cancellation_requested: kani::any(),
        }
    }

    #[kani::proof]
    fn stale_dispatch_claim_cannot_modify_authoritative_state() {
        let state = arbitrary_transition();
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
        let state = arbitrary_transition();
        let done = kani::any();
        let result = state.settle(state.lease_epoch, done);
        if state.state != crate::DispatchState::Leased {
            assert_eq!(result, GuardedTransition::Fenced);
        } else if done {
            assert_eq!(result, GuardedTransition::Removed);
        } else {
            assert_eq!(
                result,
                GuardedTransition::Applied(DispatchTransition {
                    state: crate::DispatchState::Awaiting,
                    ..state
                })
            );
        }
    }

    #[kani::proof]
    fn dispatch_cancel_revokes_old_epoch_and_is_idempotent() {
        // Causes: C1 the row is superseded, dead-lettered, live-leased, or another cancellable
        // phase; C2 its lease epoch is incrementable; C3 cancellation repeats.
        // Effects: E1 superseded rows reject cancellation; E2 a live lease returns
        // to its non-leased phase, advances exactly one epoch, and fences the old
        // claim; E3 other cancellable phases retain phase/epoch and set the flag;
        // E4 a repeat is an applied state stutter with no second revocation.
        // Constraint/invariant: the epoch remains the sole claim fence and
        // cancellation neither reopens terminal rows nor mints an extra owner.
        // Decision rules: R1 terminal=>E1; R2 leased+C2=>E2; R3 other=>E3;
        // R4 any applied result+C3=>E4.
        let state = arbitrary_transition();
        kani::assume(state.lease_epoch < u64::MAX);
        let first = state.cancel().unwrap();
        match first {
            CancelTransition::NotCancellable => {
                assert_eq!(state.state, crate::DispatchState::Superseded);
            }
            CancelTransition::Applied {
                state: cancelled,
                revoked_lease,
            } => {
                assert!(cancelled.cancellation_requested);
                if matches!(
                    state.state,
                    crate::DispatchState::ReservationLeased | crate::DispatchState::Leased
                ) {
                    assert!(revoked_lease);
                    assert_eq!(
                        cancelled.state,
                        if state.state == crate::DispatchState::ReservationLeased {
                            crate::DispatchState::Reserved
                        } else {
                            crate::DispatchState::Pending
                        }
                    );
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
                    assert_eq!(cancelled.state, state.state);
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
        // Causes: C1 the phase is ordinary runnable, excluded
        // reserved/terminal, or another non-runnable phase; C2 the current epoch
        // is either incrementable or exhausted. Effects: E1 excluded phases
        // return no claim; E2 an exhausted runnable row fails closed; E3 an
        // incrementable runnable row becomes Leased at exactly epoch+1 while
        // preserving its cancellation intent. Constraint/invariant: ordinary
        // claim is the only executable lease transition and cannot recover a
        // reservation or reopen a terminal row. Decision rules: R1 excluded=>E1;
        // R2 runnable+exhausted=>E2; R3 runnable+incrementable=>E3.
        let state = arbitrary_transition();
        let result = state.claim();
        if matches!(
            state.state,
            crate::DispatchState::Reserved
                | crate::DispatchState::ReservationLeased
                | crate::DispatchState::DeadLetter
                | crate::DispatchState::Superseded
        ) {
            assert_eq!(result, Ok(None));
        } else if state.lease_epoch == u64::MAX {
            assert_eq!(result, Err(DispatchTransitionError::LeaseEpochExhausted));
        } else {
            assert_eq!(
                result,
                Ok(Some(DispatchTransition {
                    state: crate::DispatchState::Leased,
                    lease_epoch: state.lease_epoch + 1,
                    cancellation_requested: state.cancellation_requested,
                }))
            );
        }
    }
}
