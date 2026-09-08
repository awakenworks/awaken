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

/// Complete, storage-neutral shape of the persisted physical-attempt slot.
///
/// Adapters derive this value while holding their existing row/aggregate lock.
/// Keeping the four states explicit prevents a partially persisted owner/epoch
/// pair from being treated as either empty or as an ordinary competing claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicalAttemptSlot {
    Empty,
    ExactClaim,
    OtherClaim,
    Corrupt,
}

impl PhysicalAttemptSlot {
    /// Logical completion does not release physical authority. Both settlement
    /// and its pre-settlement effects require an actually empty slot; corrupt
    /// or still-occupied slots fail closed even under a current claim.
    #[must_use]
    pub const fn permits_settlement(self) -> bool {
        matches!(self, Self::Empty)
    }

    /// Classify nullable storage columns without allowing either half of the
    /// owner/epoch pair to disappear. Match bits are ignored unless both facts
    /// are present.
    #[must_use]
    pub const fn from_facts(
        owner_present: bool,
        epoch_present: bool,
        owner_matches: bool,
        epoch_matches: bool,
    ) -> Self {
        match (owner_present, epoch_present) {
            (false, false) => Self::Empty,
            (true, true) if owner_matches && epoch_matches => Self::ExactClaim,
            (true, true) => Self::OtherClaim,
            _ => Self::Corrupt,
        }
    }
}

/// Mutation selected by the physical-attempt reducer. Storage adapters perform
/// only the named write after this decision; they do not own another admission
/// policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicalAttemptTransition {
    Install,
    Replace,
    Clear,
    Stutter,
    Blocked,
    Fenced,
    Corrupt,
}

/// Result of relinquishing an exact mutation claim while accounting for the
/// physical-attempt slot in the same dispatch aggregate.
///
/// `Pending` is ordinary pre-execution rollback. `Recovering` deliberately
/// retains the existing Leased state and asks the adapter to expire its lease:
/// that is the canonical crash-recovery representation and preserves the
/// `Claimed::recovered` fact without adding a parallel recovery state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelinquishTransition {
    Pending(DispatchTransition),
    Recovering(DispatchTransition),
    Fenced,
    Corrupt,
}

/// Decide whether the current durable claim may enter the physical executor.
/// The caller supplies `claim_is_current` from the same locked snapshot that
/// supplied `slot` and `thread_peer_occupied`. Thread exclusion is mandatory,
/// not an optional post-decision check, and does not depend on peer Run status.
#[must_use]
pub const fn begin_physical_attempt(
    claim_is_current: bool,
    slot: PhysicalAttemptSlot,
    thread_peer_occupied: bool,
) -> PhysicalAttemptTransition {
    if !claim_is_current {
        return PhysicalAttemptTransition::Fenced;
    }
    if !matches!(slot, PhysicalAttemptSlot::Corrupt) && thread_peer_occupied {
        return PhysicalAttemptTransition::Blocked;
    }
    match slot {
        PhysicalAttemptSlot::Empty => PhysicalAttemptTransition::Install,
        PhysicalAttemptSlot::ExactClaim => PhysicalAttemptTransition::Stutter,
        PhysicalAttemptSlot::OtherClaim => PhysicalAttemptTransition::Blocked,
        PhysicalAttemptSlot::Corrupt => PhysicalAttemptTransition::Corrupt,
    }
}

/// Replace a predecessor physical slot with a current claim that can prove it
/// is continuing the same durable remote operation without repeating the
/// initiating effect.
///
/// The proof is deliberately established by the authoritative leaf executor;
/// this reducer still owns the one atomic Dispatch-slot mutation. Ordinary
/// recovered attempts continue to use [`begin_physical_attempt`] and block.
#[must_use]
pub const fn handoff_physical_attempt(
    claim_is_current: bool,
    slot: PhysicalAttemptSlot,
    thread_peer_occupied: bool,
) -> PhysicalAttemptTransition {
    // Handoff changes only the disposition of a complete predecessor. Current
    // authority, exact replay, empty admission and corruption use the ordinary
    // kernel so that adding a guard cannot leave a parallel recovery bypass.
    match begin_physical_attempt(claim_is_current, slot, thread_peer_occupied) {
        PhysicalAttemptTransition::Blocked if !thread_peer_occupied => {
            PhysicalAttemptTransition::Replace
        }
        transition => transition,
    }
}

/// Decide how an exact physical-quiescence acknowledgement changes the slot.
/// Finishing an already empty slot is an idempotent replay; another claim and a
/// torn owner/epoch pair are never cleared.
#[must_use]
pub const fn finish_physical_attempt(slot: PhysicalAttemptSlot) -> PhysicalAttemptTransition {
    match slot {
        PhysicalAttemptSlot::Empty => PhysicalAttemptTransition::Stutter,
        PhysicalAttemptSlot::ExactClaim => PhysicalAttemptTransition::Clear,
        PhysicalAttemptSlot::OtherClaim => PhysicalAttemptTransition::Fenced,
        PhysicalAttemptSlot::Corrupt => PhysicalAttemptTransition::Corrupt,
    }
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

    /// Remove only an unleased reservation after its Session authority made a
    /// definitive rejection. Recovery-leased or executable rows are fenced.
    #[must_use]
    pub fn reject_reservation(self) -> GuardedTransition {
        if self.state == crate::DispatchState::Reserved {
            GuardedTransition::Removed
        } else {
            GuardedTransition::Fenced
        }
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

    /// Apply settlement only to the exact leased epoch with no physical owner.
    /// The slot must come from the same locked snapshot as this transition.
    /// Keeping the physical gate inside the transition prevents a new adapter
    /// from accidentally deleting an occupied row by omitting a preflight.
    pub fn settle(
        self,
        claim_epoch: u64,
        done: bool,
        slot: PhysicalAttemptSlot,
    ) -> GuardedTransition {
        if self.state != crate::DispatchState::Leased
            || self.lease_epoch != claim_epoch
            || !slot.permits_settlement()
        {
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

    /// Release an exact current mutation claim under the aggregate's physical
    /// attempt evidence. Owner equality is supplied by the adapter because
    /// owner representation is deliberately not in this storage-neutral
    /// kernel.
    pub fn relinquish(
        self,
        claim_epoch: u64,
        owner_matches: bool,
        slot: PhysicalAttemptSlot,
    ) -> RelinquishTransition {
        if self.state != crate::DispatchState::Leased
            || self.lease_epoch != claim_epoch
            || !owner_matches
        {
            return RelinquishTransition::Fenced;
        }
        match slot {
            PhysicalAttemptSlot::Empty => RelinquishTransition::Pending(Self {
                state: crate::DispatchState::Pending,
                ..self
            }),
            PhysicalAttemptSlot::ExactClaim => RelinquishTransition::Fenced,
            PhysicalAttemptSlot::OtherClaim => RelinquishTransition::Recovering(self),
            PhysicalAttemptSlot::Corrupt => RelinquishTransition::Corrupt,
        }
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

    /// Move an exact expired current attempt to dead-letter without minting a
    /// new claim authority. Retry eligibility remains a separate evidence
    /// predicate because it includes attempt count and expiry time.
    #[must_use]
    pub fn exhaust_retries(self, claim_epoch: u64, owner_matches: bool) -> GuardedTransition {
        if self.state != crate::DispatchState::Leased
            || self.lease_epoch != claim_epoch
            || !owner_matches
        {
            return GuardedTransition::Fenced;
        }
        GuardedTransition::Applied(Self {
            state: crate::DispatchState::DeadLetter,
            ..self
        })
    }

    /// Requeue only a non-cancelled dead letter. Cancellation is terminal
    /// intent for this dispatch row and cannot be bypassed by maintenance.
    #[must_use]
    pub fn requeue_dead_letter(self) -> Option<Self> {
        (self.state == crate::DispatchState::DeadLetter && !self.cancellation_requested).then_some(
            Self {
                state: crate::DispatchState::Pending,
                ..self
            },
        )
    }

    /// Supersession is limited to unclaimed, non-cancelled executable rows.
    /// It never revokes a live lease or rewrites reservation admission.
    #[must_use]
    pub fn supersede(self) -> Option<Self> {
        (matches!(
            self.state,
            crate::DispatchState::Pending | crate::DispatchState::Awaiting
        ) && !self.cancellation_requested)
            .then_some(Self {
                state: crate::DispatchState::Superseded,
                ..self
            })
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
    fn physical_attempt_slot_follows_the_complete_decision_table() {
        // Cause/effect graph: C1 the asserted durable claim is current; C2 the
        // physical slot is empty, exact, occupied by another claim, or torn.
        // Cause C3: a leaf executor proved a committed continuation that cannot
        // repeat the initiating external effect. Effects: E1 install once; E2
        // exact begin/empty finish stutter; E3 a predecessor blocks ordinary
        // begin and fences finish; E4 a stale claim is fenced before slot
        // interpretation; E5 torn persistence fails closed; E6 only C1+C3 may
        // replace another complete slot.
        //
        // | Rule | C1 | C2 | C3 | Begin | Handoff | Finish |
        // |---|---|---|---|---|---|---|
        // | R1 | yes | empty | any | install | install | stutter |
        // | R2 | yes | exact | any | stutter | stutter | clear |
        // | R3 | yes | other | no | blocked | n/a | fenced |
        // | R4 | yes | other | yes | blocked | replace | fenced |
        // | R5 | no | any | any | fenced | fenced | n/a |
        // | R6 | yes | corrupt | any | corrupt | corrupt | corrupt |
        // Constraint: `Replace` is reachable only through the separately named
        // handoff input; ordinary begin never erases an unacknowledged slot.
        use PhysicalAttemptSlot::{Corrupt, Empty, ExactClaim, OtherClaim};
        use PhysicalAttemptTransition::{
            Blocked, Clear, Corrupt as CorruptTransition, Fenced, Install, Replace, Stutter,
        };
        assert_eq!(begin_physical_attempt(true, Empty, false), Install, "R1/E1");
        assert_eq!(
            handoff_physical_attempt(true, Empty, false),
            Install,
            "R1/E1"
        );
        assert_eq!(finish_physical_attempt(Empty), Stutter, "R1/E2");
        assert_eq!(
            begin_physical_attempt(true, ExactClaim, false),
            Stutter,
            "R2/E2"
        );
        assert_eq!(
            handoff_physical_attempt(true, ExactClaim, false),
            Stutter,
            "R2/E2"
        );
        assert_eq!(finish_physical_attempt(ExactClaim), Clear, "R2/E2");
        assert_eq!(
            begin_physical_attempt(true, OtherClaim, false),
            Blocked,
            "R3/E3"
        );
        assert_eq!(
            handoff_physical_attempt(true, OtherClaim, false),
            Replace,
            "R4/E6"
        );
        assert_eq!(finish_physical_attempt(OtherClaim), Fenced, "R3/E3");
        for slot in [Empty, ExactClaim, OtherClaim, Corrupt] {
            assert_eq!(begin_physical_attempt(false, slot, false), Fenced, "R4/E4");
            assert_eq!(
                handoff_physical_attempt(false, slot, false),
                Fenced,
                "R5/E4"
            );
        }
        assert_eq!(
            begin_physical_attempt(true, Corrupt, false),
            CorruptTransition,
            "R5/E5"
        );
        assert_eq!(finish_physical_attempt(Corrupt), CorruptTransition, "R5/E5");
        assert_eq!(
            handoff_physical_attempt(true, Corrupt, false),
            CorruptTransition,
            "R6/E5"
        );
    }

    #[test]
    fn stale_settle_and_relinquish_are_fenced() {
        let current = leased(8);
        assert_eq!(
            current.settle(7, true, PhysicalAttemptSlot::Empty),
            GuardedTransition::Fenced
        );
        assert_eq!(
            current.relinquish(8, false, PhysicalAttemptSlot::Empty),
            RelinquishTransition::Fenced
        );
        assert_eq!(
            current.relinquish(7, true, PhysicalAttemptSlot::Empty),
            RelinquishTransition::Fenced
        );
    }

    #[test]
    fn relinquish_preserves_recovery_evidence_for_a_stale_physical_attempt() {
        // Cause/effect graph: C1 exact mutation claim; C2 physical slot is
        // empty, exact, other, or torn. Effects: E1 Pending rollback; E2 fence;
        // E3 retain Leased so the adapter can expire it for immediate recovery;
        // E4 reject corrupt persistence.
        //
        // | Rule | C1 | C2 | Effect |
        // |---|---|---|---|
        // | R1 | exact | empty | E1 Pending |
        // | R2 | exact | exact | E2 Fenced |
        // | R3 | exact | other | E3 Recovering Leased |
        // | R4 | exact | corrupt | E4 Corrupt |
        // | R5 | stale | any | E2 Fenced |
        //
        // Invariant: an occupied predecessor slot is never cleared, and its
        // successor never loses the recovery classification by becoming an
        // ordinary fresh Pending dispatch.
        let current = leased(8);
        assert_eq!(
            current.relinquish(8, true, PhysicalAttemptSlot::Empty),
            RelinquishTransition::Pending(DispatchTransition {
                state: crate::DispatchState::Pending,
                ..current
            })
        );
        assert_eq!(
            current.relinquish(8, true, PhysicalAttemptSlot::ExactClaim),
            RelinquishTransition::Fenced
        );
        assert_eq!(
            current.relinquish(8, true, PhysicalAttemptSlot::OtherClaim),
            RelinquishTransition::Recovering(current)
        );
        assert_eq!(
            current.relinquish(8, true, PhysicalAttemptSlot::Corrupt),
            RelinquishTransition::Corrupt
        );
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
        assert_eq!(
            current.settle(8, true, PhysicalAttemptSlot::Empty),
            GuardedTransition::Removed
        );
        assert_eq!(
            current.settle(8, false, PhysicalAttemptSlot::Empty),
            GuardedTransition::Applied(DispatchTransition {
                state: crate::DispatchState::Awaiting,
                ..current
            })
        );
    }

    #[test]
    fn settlement_requires_one_joint_claim_and_physical_snapshot() {
        // Causes: C1 phase is leased or any other phase; C2 epoch is exact/stale;
        // C3 slot is empty/exact/other/corrupt; C4 Done/Awaiting.
        // Effects: E1 only C1+C2+empty may remove/await; E2 every other
        // combination fences. NO adapter preflight precedes this call, so an
        // omitted preflight cannot delete work. Exhaust the 7*2*4*2 table.
        for phase in [
            crate::DispatchState::Reserved,
            crate::DispatchState::ReservationLeased,
            crate::DispatchState::Pending,
            crate::DispatchState::Leased,
            crate::DispatchState::Awaiting,
            crate::DispatchState::DeadLetter,
            crate::DispatchState::Superseded,
        ] {
            let state = DispatchTransition {
                state: phase,
                ..leased(8)
            };
            for epoch in [7, 8] {
                for slot in [
                    PhysicalAttemptSlot::Empty,
                    PhysicalAttemptSlot::ExactClaim,
                    PhysicalAttemptSlot::OtherClaim,
                    PhysicalAttemptSlot::Corrupt,
                ] {
                    for done in [false, true] {
                        let expected = if phase != crate::DispatchState::Leased
                            || epoch != 8
                            || slot != PhysicalAttemptSlot::Empty
                        {
                            GuardedTransition::Fenced
                        } else if done {
                            GuardedTransition::Removed
                        } else {
                            GuardedTransition::Applied(DispatchTransition {
                                state: crate::DispatchState::Awaiting,
                                ..state
                            })
                        };
                        assert_eq!(state.settle(epoch, done, slot), expected);
                    }
                }
            }
        }
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
        assert_eq!(
            old.settle(8, true, PhysicalAttemptSlot::Empty),
            GuardedTransition::Removed
        );
        assert_eq!(
            cancelled.settle(8, true, PhysicalAttemptSlot::Empty),
            GuardedTransition::Fenced
        );
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

    fn arbitrary_physical_slot() -> PhysicalAttemptSlot {
        match kani::any::<u8>() % 4 {
            0 => PhysicalAttemptSlot::Empty,
            1 => PhysicalAttemptSlot::ExactClaim,
            2 => PhysicalAttemptSlot::OtherClaim,
            _ => PhysicalAttemptSlot::Corrupt,
        }
    }

    #[kani::proof]
    fn physical_attempt_slot_installs_and_clears_only_exact_authority() {
        let current = kani::any::<bool>();
        let slot = arbitrary_physical_slot();
        // R2/R4: neither a current mutation claim nor logical completion can
        // authorize settlement effects while any physical slot remains.
        assert_eq!(
            slot.permits_settlement(),
            slot == PhysicalAttemptSlot::Empty
        );
        let begin = begin_physical_attempt(current, slot, false);
        let handoff = handoff_physical_attempt(current, slot, false);
        let finish = finish_physical_attempt(slot);

        // Normalization rule: continuation changes only the disposition of a
        // complete predecessor. Every other admission guard/result is shared,
        // including a stale claim combined with corrupt persisted evidence.
        assert_eq!(
            handoff,
            if begin == PhysicalAttemptTransition::Blocked {
                PhysicalAttemptTransition::Replace
            } else {
                begin
            }
        );
        assert_eq!(
            begin == PhysicalAttemptTransition::Install,
            current && slot == PhysicalAttemptSlot::Empty
        );
        assert_eq!(
            begin == PhysicalAttemptTransition::Stutter,
            current && slot == PhysicalAttemptSlot::ExactClaim
        );
        assert_eq!(
            finish == PhysicalAttemptTransition::Clear,
            slot == PhysicalAttemptSlot::ExactClaim
        );
        if !current {
            assert_eq!(begin, PhysicalAttemptTransition::Fenced);
            assert_eq!(handoff, PhysicalAttemptTransition::Fenced);
        }
        if slot == PhysicalAttemptSlot::OtherClaim {
            assert_ne!(begin, PhysicalAttemptTransition::Install);
            assert_ne!(finish, PhysicalAttemptTransition::Clear);
            assert_eq!(handoff == PhysicalAttemptTransition::Replace, current);
        }
    }

    #[kani::proof]
    fn thread_physical_peer_vetoes_begin_and_handoff() {
        // P1 current/stale, P2 all four slot shapes, P3 begin/handoff,
        // P4 peer absent/present. A peer can only restrict admission; it never
        // changes stale/corrupt classification and never becomes Clear.
        let current = kani::any();
        let slot = arbitrary_physical_slot();
        let handoff = kani::any();
        let peer = kani::any();
        let base = if handoff {
            handoff_physical_attempt(current, slot, false)
        } else {
            begin_physical_attempt(current, slot, false)
        };
        let result = if handoff {
            handoff_physical_attempt(current, slot, peer)
        } else {
            begin_physical_attempt(current, slot, peer)
        };
        if !peer {
            assert_eq!(result, base);
        } else {
            assert!(!matches!(
                result,
                PhysicalAttemptTransition::Install
                    | PhysicalAttemptTransition::Replace
                    | PhysicalAttemptTransition::Stutter
            ));
        }
        assert_eq!(
            result == PhysicalAttemptTransition::Fenced,
            base == PhysicalAttemptTransition::Fenced
        );
        assert_eq!(
            result == PhysicalAttemptTransition::Corrupt,
            base == PhysicalAttemptTransition::Corrupt
        );
    }

    #[kani::proof]
    fn physical_attempt_slot_never_normalizes_a_torn_owner_epoch_pair() {
        let owner_present = kani::any::<bool>();
        let epoch_present = kani::any::<bool>();
        let owner_matches = kani::any::<bool>();
        let epoch_matches = kani::any::<bool>();
        let slot = PhysicalAttemptSlot::from_facts(
            owner_present,
            epoch_present,
            owner_matches,
            epoch_matches,
        );
        assert_eq!(
            slot == PhysicalAttemptSlot::Corrupt,
            owner_present != epoch_present
        );
        assert_eq!(slot.permits_settlement(), !owner_present && !epoch_present);
        assert_eq!(
            slot == PhysicalAttemptSlot::ExactClaim,
            owner_present && epoch_present && owner_matches && epoch_matches
        );
    }

    #[kani::proof]
    fn stale_dispatch_claim_cannot_modify_authoritative_state() {
        let state = arbitrary_transition();
        let stale_epoch = kani::any();
        kani::assume(stale_epoch != state.lease_epoch);
        assert_eq!(
            state.settle(stale_epoch, kani::any(), PhysicalAttemptSlot::Empty),
            GuardedTransition::Fenced
        );
        assert_eq!(
            state.relinquish(stale_epoch, kani::any(), PhysicalAttemptSlot::Empty),
            RelinquishTransition::Fenced
        );
        assert_eq!(
            state.exhaust_retries(stale_epoch, kani::any()),
            GuardedTransition::Fenced
        );
    }

    #[kani::proof]
    fn exact_dispatch_settlement_is_terminal_or_awaiting_only() {
        let state = arbitrary_transition();
        let done = kani::any();
        // S1 exact claim + empty slot => ordinary settlement; S2 any occupied
        // or corrupt slot => fenced, regardless of logical outcome. Exercise
        // the same joint production decision used by all stores.
        let slot = arbitrary_physical_slot();
        let result = state.settle(state.lease_epoch, done, slot);
        if state.state != crate::DispatchState::Leased || slot != PhysicalAttemptSlot::Empty {
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
                        cancelled.settle(
                            state.lease_epoch,
                            kani::any(),
                            PhysicalAttemptSlot::Empty
                        ),
                        GuardedTransition::Fenced
                    );
                    assert_eq!(
                        cancelled.relinquish(state.lease_epoch, true, PhysicalAttemptSlot::Empty,),
                        RelinquishTransition::Fenced
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

    #[kani::proof]
    fn dispatch_maintenance_transitions_are_closed_and_exact() {
        // Cause/effect table: Reserved alone may be rejected; non-cancelled
        // DeadLetter alone may requeue; non-cancelled Pending/Awaiting alone may
        // supersede. Every complementary state/flag partition is a no-op/fence.
        let state = arbitrary_transition();
        assert_eq!(
            matches!(state.reject_reservation(), GuardedTransition::Removed),
            state.state == crate::DispatchState::Reserved
        );
        assert_eq!(
            state.requeue_dead_letter().is_some(),
            state.state == crate::DispatchState::DeadLetter && !state.cancellation_requested
        );
        assert_eq!(
            state.supersede().is_some(),
            matches!(
                state.state,
                crate::DispatchState::Pending | crate::DispatchState::Awaiting
            ) && !state.cancellation_requested
        );
    }
}
