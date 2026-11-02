// @formal entrypoint delegation.independent_child_scheduling_lease
// @formal entrypoint ingress.epoch_reclaim
// @formal entrypoint ingress.settle_fence
// @formal entrypoint ingress.single_owner
// @formal entrypoint remote_worker.execution_requires_claimed_snapshot
// @formal entrypoint remote_worker.single_live_claim_owner
// @formal entrypoint run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing
// @formal entrypoint run_ingress.failed_attempt_release_spends_one_attempt_and_exhausts_through_retry_exhaustion
// @formal entrypoint run_ingress.relinquish_requires_exact_current_claim_and_preserves_retry_budget
// @formal entrypoint session_event_batch.processed_requires_commit_observation_activity_and_dispatch_settlement
// @formal entrypoint session_execution.initial_event_path_reaches_idle_only_after_full_settlement
// @formal entrypoint session_execution.one_physical_attempt_per_thread
// @formal entrypoint session_run.recovery_claim_is_admission_only
// @formal entrypoint tool.session_thread_composition
// @formal entrypoint worker.retry_preserves_pinned_binding
// @formal entrypoint worker_credential.exact_revision_and_current_epoch_required
// @formal entrypoint worker_credential.expired_observation_never_executes
// @formal evidence run_ingress.a_deadline_passes_once_it_is_reached
//! Backend-neutral state transitions for one durable dispatch row.
//!
//! Storage adapters remain responsible for atomic compare-and-set and durable
//! persistence. This kernel defines the state/epoch decision they must apply.

use awaken_lease_contract::fence::Epoch;

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
        /// Only the first cancellation makes a deferred row runnable now.
        /// Later failed attempts keep their request-local retry deadline.
        clear_retry_deadline: bool,
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

    /// Whether explicit operator quarantine may dead-letter one expired,
    /// retry-exhausted row while this slot is still occupied.
    ///
    /// Every other disposition keeps requiring [`Self::permits_settlement`]: a
    /// predecessor that may still be executing must not have `Done`, an outcome,
    /// or a successor's execution authority decided under it. Quarantine is
    /// admissible on an occupied slot precisely because it does none of those
    /// things. It records no completion and no Run outcome truth (G35), it grants
    /// nobody execution, and it only removes the row from the claim pool. The
    /// slot is never cleared here, so a predecessor that does return still
    /// acknowledges its own quiescence exactly.
    ///
    /// Without this rule a row whose predecessor vanished without that
    /// acknowledgement can never leave `Leased`: settlement is fenced by the
    /// slot, every successor is blocked by it, and the operator escape hatch
    /// filters it out. The row then re-leases forever while holding execution
    /// capacity it can never use.
    ///
    /// Recorded cancellation intent is required, so an ordinary crashed row
    /// whose predecessor may still legitimately finish stays untouched. A torn
    /// owner/epoch pair remains fail-closed.
    #[must_use]
    pub const fn quarantine_admits_occupation(self, cancellation_requested: bool) -> bool {
        cancellation_requested && matches!(self, Self::ExactClaim | Self::OtherClaim)
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

/// How a released claim's attempt is accounted against the dispatch retry
/// budget (ADR-0015 D1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleasedAttempt {
    /// The attempt did not fail (a busy subordinate authority, a capacity
    /// refusal): the release keeps the budget.
    Unspent,
    /// A classified failed attempt: the release spends one attempt of
    /// `max_attempts`, and the attempt that reaches it exhausts the budget.
    Failed { max_attempts: u64 },
}

/// Result of relinquishing an exact mutation claim while accounting for the
/// physical-attempt slot in the same dispatch aggregate.
///
/// `Pending` is ordinary pre-execution rollback. `Recovering` deliberately
/// retains the existing Leased state and asks the adapter to expire its lease:
/// that is the canonical crash-recovery representation and preserves the
/// `Claimed::recovered` fact without adding a parallel recovery state. A failed
/// attempt that exhausts the budget also returns `Recovering`, so the one
/// retry-exhaustion claim commits its terminal outcome exactly as it does for a
/// crashed attempt. Both applied variants return the attempt count the adapter
/// writes, so an adapter cannot spend or reset retry budget on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelinquishTransition {
    Pending {
        state: DispatchTransition,
        attempt_count: u64,
    },
    Recovering {
        state: DispatchTransition,
        attempt_count: u64,
    },
    Fenced,
    Corrupt,
}

/// An acknowledged or conservatively retired epoch is never a fresh execution
/// identity, even while its lease
/// remains current. Compare instead of incrementing to avoid wraparound.
#[must_use]
pub const fn attempt_epoch_is_open(epoch: u64, retired_epoch: u64) -> bool {
    epoch > retired_epoch
}

#[test]
fn retired_attempts_never_reenter_even_at_integer_boundaries() {
    // Causes: C1 fresh/equal/older epoch; C2 live/stale lease; C3 ordinary/
    // continuation admission. R1 C1=fresh preserves ordinary reducer semantics;
    // R2 C1=equal/older or stale lease => Fenced for either admission route.
    // MC/DC plus zero/MAX boundaries: no subtraction/addition can wrap.
    for epoch in [0, 1, 2, u64::MAX - 1, u64::MAX] {
        for retired in [0, 1, 2, u64::MAX - 1, u64::MAX] {
            for live in [false, true] {
                let current = live && attempt_epoch_is_open(epoch, retired);
                let expected = if live && epoch > retired {
                    PhysicalAttemptTransition::Install
                } else {
                    PhysicalAttemptTransition::Fenced
                };
                assert_eq!(
                    begin_physical_attempt(current, PhysicalAttemptSlot::Empty, false, false),
                    expected
                );
                assert_eq!(
                    handoff_physical_attempt(current, PhysicalAttemptSlot::Empty, false, false),
                    expected
                );
            }
        }
    }
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
    closing: bool,
) -> PhysicalAttemptTransition {
    if !claim_is_current {
        return PhysicalAttemptTransition::Fenced;
    }
    if !matches!(slot, PhysicalAttemptSlot::Corrupt) && (thread_peer_occupied || closing) {
        return if closing && matches!(slot, PhysicalAttemptSlot::Empty) {
            PhysicalAttemptTransition::Corrupt
        } else {
            PhysicalAttemptTransition::Blocked
        };
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
    closing: bool,
) -> PhysicalAttemptTransition {
    // Handoff changes only the disposition of a complete predecessor. Current
    // authority, exact replay, empty admission and corruption use the ordinary
    // kernel so that adding a guard cannot leave a parallel recovery bypass.
    match begin_physical_attempt(claim_is_current, slot, thread_peer_occupied, closing) {
        PhysicalAttemptTransition::Blocked if !thread_peer_occupied && !closing => {
            PhysicalAttemptTransition::Replace
        }
        transition => transition,
    }
}

/// Decide how an exact physical-quiescence acknowledgement changes the slot.
/// Finishing an already empty slot is an idempotent replay; another claim and a
/// torn owner/epoch pair are never cleared.
#[must_use]
pub const fn finish_physical_attempt(
    slot: PhysicalAttemptSlot,
    closing: bool,
    closure_evidence: bool,
) -> PhysicalAttemptTransition {
    if closing && matches!(slot, PhysicalAttemptSlot::Empty) {
        return PhysicalAttemptTransition::Corrupt;
    }
    if closing != closure_evidence
        && !matches!(
            slot,
            PhysicalAttemptSlot::Empty | PhysicalAttemptSlot::Corrupt
        )
    {
        return PhysicalAttemptTransition::Fenced;
    }
    match slot {
        PhysicalAttemptSlot::Empty => PhysicalAttemptTransition::Stutter,
        PhysicalAttemptSlot::ExactClaim => PhysicalAttemptTransition::Clear,
        PhysicalAttemptSlot::OtherClaim => PhysicalAttemptTransition::Fenced,
        PhysicalAttemptSlot::Corrupt => PhysicalAttemptTransition::Corrupt,
    }
}

/// The original exact slot may close its local entry after losing its mutation
/// lease. This changes no owner/epoch and grants no ACK or replacement.
pub const fn can_close_physical_attempt(slot: PhysicalAttemptSlot) -> bool {
    matches!(slot, PhysicalAttemptSlot::ExactClaim)
}

#[cfg(test)]
mod closure_phase_tests {
    use super::*;
    #[test]
    fn closure_phase_fences_entry_handoff_and_ordinary_late_return() {
        // C1 current/stale claimant; C2 empty/exact/other/corrupt slot; C3
        // open/closing phase; C4 ordinary/independent-closure return.
        // R1 stale => Fenced. R2 closing+occupied => both entry paths Blocked.
        // R3 closing+empty => corrupt, not reusable. R4 exact+matching return
        // kind => Clear. R5 exact+wrong return kind => Fenced. R6 other =>
        // Fenced; R7 corrupt => Corrupt. This exhausts every interacting slot
        // and return-kind partition against literal expected outcomes.
        for slot in [
            PhysicalAttemptSlot::Empty,
            PhysicalAttemptSlot::ExactClaim,
            PhysicalAttemptSlot::OtherClaim,
            PhysicalAttemptSlot::Corrupt,
        ] {
            for current in [false, true] {
                let expected = if !current {
                    PhysicalAttemptTransition::Fenced
                } else if matches!(
                    slot,
                    PhysicalAttemptSlot::Empty | PhysicalAttemptSlot::Corrupt
                ) {
                    PhysicalAttemptTransition::Corrupt
                } else {
                    PhysicalAttemptTransition::Blocked
                };
                assert_eq!(begin_physical_attempt(current, slot, false, true), expected);
                assert_eq!(
                    handoff_physical_attempt(current, slot, false, true),
                    expected
                );
            }
            for closing in [false, true] {
                for evidence in [false, true] {
                    let expected = match slot {
                        PhysicalAttemptSlot::Empty if closing => PhysicalAttemptTransition::Corrupt,
                        PhysicalAttemptSlot::Empty => PhysicalAttemptTransition::Stutter,
                        PhysicalAttemptSlot::Corrupt => PhysicalAttemptTransition::Corrupt,
                        PhysicalAttemptSlot::OtherClaim => PhysicalAttemptTransition::Fenced,
                        PhysicalAttemptSlot::ExactClaim if closing == evidence => {
                            PhysicalAttemptTransition::Clear
                        }
                        PhysicalAttemptSlot::ExactClaim => PhysicalAttemptTransition::Fenced,
                    };
                    assert_eq!(finish_physical_attempt(slot, closing, evidence), expected);
                }
            }
        }
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
        && crate::deadline_has_passed(lease_until_ms, now_ms)
}

/// How many recovery re-claims after a predecessor's own claim may be blocked
/// by its still-occupied physical slot without spending retry budget.
///
/// Successive blocked re-claims are separated by the Pool's exact-claim
/// backoff (exponential, capped at 30 s), so this is also a wall-clock bound
/// of roughly ten minutes for one predecessor's closure to progress.
pub const BLOCKED_RECLAIM_LIMIT: u64 = 20;

/// The retry budget written when an exact claim is released while another
/// claim's physical slot is still occupied.
///
/// A recovery re-claim spends one attempt when it is claimed. If it then only
/// finds the predecessor still closing (Blocked admission, deferred as
/// `Unspent`), it did no work: the re-claim's attempt is refunded. The refund
/// is bounded by claims, not by closure evidence: past [`BLOCKED_RECLAIM_LIMIT`]
/// re-claims since its own epoch every blocked re-claim spends again, so the
/// budget is already spent when the slot finally closes and retry exhaustion
/// then ends the Run `Indeterminate` at once. The budget never overrides an
/// occupied slot (ADR-0078): retry exhaustion refuses a Run whose physical
/// slot is still occupied. A predecessor whose slot never closes is resolved
/// only by its process-end evidence (exact exit, Superseded) or operator
/// quarantine. `reclaims_since_predecessor` is
/// the releasing claim's epoch minus the slot owner's epoch, `None` when the
/// slot owner is absent or not older.
///
/// | Row | Release | Re-claims since the predecessor | Written count |
/// |---|---|---|---|
/// | B1 | `Unspent` | `1..=LIMIT` | `count - 1` (saturating) |
/// | B2 | `Unspent` | `> LIMIT` | `count` |
/// | B3 | `Unspent` | `None` or `0` | `count` |
/// | B4 | `Failed` | any | `count` (the re-claim already spent it) |
#[must_use]
pub const fn blocked_reclaim_attempt_count(
    attempt: ReleasedAttempt,
    reclaims_since_predecessor: Option<u64>,
    attempt_count: u64,
) -> u64 {
    match (attempt, reclaims_since_predecessor) {
        (ReleasedAttempt::Unspent, Some(reclaims))
            if reclaims != 0 && reclaims <= BLOCKED_RECLAIM_LIMIT =>
        {
            attempt_count.saturating_sub(1)
        }
        _ => attempt_count,
    }
}

impl DispatchTransition {
    /// Persisted zero is the legacy never-claimed sentinel, not a grant.
    /// Every minted successor uses the lease kernel's non-wrapping epoch law.
    fn next_lease_epoch(self) -> Result<u64, DispatchTransitionError> {
        let next = match Epoch::from_persisted(self.lease_epoch) {
            Some(epoch) => epoch.next(),
            None => Some(Epoch::FIRST),
        };
        next.map(Epoch::get)
            .ok_or(DispatchTransitionError::LeaseEpochExhausted)
    }

    /// Acquire admission-only tenure over an exact unbound legacy Pending row.
    /// These observations come from the same locked row as this transition;
    /// ordinary claiming remains the sole execution-authority transition.
    pub fn adopt_pending_reservation(
        self,
        original_exact: bool,
        legacy_unbound: bool,
        authority_empty: bool,
        physical_slot: PhysicalAttemptSlot,
        closing: bool,
    ) -> Result<Option<Self>, DispatchTransitionError> {
        if self.state != crate::DispatchState::Pending
            || !original_exact
            || !legacy_unbound
            || !authority_empty
            || physical_slot != PhysicalAttemptSlot::Empty
            || closing
        {
            return Ok(None);
        }
        let lease_epoch = self.next_lease_epoch()?;
        Ok(Some(Self {
            state: crate::DispatchState::ReservationLeased,
            lease_epoch,
            cancellation_requested: self.cancellation_requested,
        }))
    }

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
        let lease_epoch = self.next_lease_epoch()?;
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
        let lease_epoch = self.next_lease_epoch()?;
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
    /// kernel. `attempt_count` and `predecessor_epoch` (the epoch of the
    /// claim occupying the physical slot, if any) come from the same locked
    /// aggregate snapshot.
    ///
    /// An [`Unspent`](ReleasedAttempt::Unspent) release returns it unchanged,
    /// except a release blocked behind a predecessor's slot, which refunds its
    /// recovery re-claim within [`BLOCKED_RECLAIM_LIMIT`]
    /// ([`blocked_reclaim_attempt_count`]).
    /// A [`Failed`](ReleasedAttempt::Failed) release from an empty slot spends
    /// one attempt: below `max_attempts` the row returns to Pending, and the
    /// attempt that reaches it keeps the row Leased for the adapter to expire,
    /// so retry exhaustion claims it as it claims a crashed attempt. A failed
    /// release behind a predecessor's slot already stays in recovery, whose
    /// re-claim spends that attempt, so it is not counted twice.
    pub fn relinquish(
        self,
        claim_epoch: u64,
        owner_matches: bool,
        slot: PhysicalAttemptSlot,
        predecessor_epoch: Option<u64>,
        attempt_count: u64,
        attempt: ReleasedAttempt,
    ) -> RelinquishTransition {
        if self.state != crate::DispatchState::Leased
            || self.lease_epoch != claim_epoch
            || !owner_matches
        {
            return RelinquishTransition::Fenced;
        }
        match slot {
            PhysicalAttemptSlot::Empty => match attempt {
                ReleasedAttempt::Unspent => RelinquishTransition::Pending {
                    state: Self {
                        state: crate::DispatchState::Pending,
                        ..self
                    },
                    attempt_count,
                },
                ReleasedAttempt::Failed { max_attempts } => {
                    let spent = attempt_count.saturating_add(1);
                    if spent >= max_attempts {
                        RelinquishTransition::Recovering {
                            state: self,
                            attempt_count: spent,
                        }
                    } else {
                        RelinquishTransition::Pending {
                            state: Self {
                                state: crate::DispatchState::Pending,
                                ..self
                            },
                            attempt_count: spent,
                        }
                    }
                }
            },
            PhysicalAttemptSlot::ExactClaim => RelinquishTransition::Fenced,
            PhysicalAttemptSlot::OtherClaim => RelinquishTransition::Recovering {
                state: self,
                attempt_count: blocked_reclaim_attempt_count(
                    attempt,
                    match predecessor_epoch {
                        Some(predecessor) if claim_epoch > predecessor => {
                            Some(claim_epoch - predecessor)
                        }
                        _ => None,
                    },
                    attempt_count,
                ),
            },
            PhysicalAttemptSlot::Corrupt => RelinquishTransition::Corrupt,
        }
    }

    /// Record cancellation. A live lease is revoked by minting the next epoch;
    /// repeated cancellation of the resulting pending row is a state stutter.
    pub fn cancel(self) -> Result<CancelTransition, DispatchTransitionError> {
        match self.state {
            crate::DispatchState::Reserved
            | crate::DispatchState::Pending
            | crate::DispatchState::Awaiting
            // A dead letter only receives the cancellation bit; its phase stays sealed.
            | crate::DispatchState::DeadLetter => Ok(CancelTransition::Applied {
                state: Self {
                    cancellation_requested: true,
                    ..self
                },
                revoked_lease: false,
                clear_retry_deadline: !self.cancellation_requested,
            }),
            crate::DispatchState::ReservationLeased | crate::DispatchState::Leased => {
                let lease_epoch = self.next_lease_epoch()?;
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
                    clear_retry_deadline: !self.cancellation_requested,
                })
            }
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

    #[test]
    fn quarantine_is_the_only_disposition_that_admits_an_occupied_slot() {
        // Causes: C1 recorded cancellation intent; C2 the persisted slot is
        // empty / exact / other / torn.
        // Effects: E1 quarantine admits an occupied slot only with C1, so the
        // operator escape hatch reaches a predecessor that vanished without
        // acknowledging quiescence; E2 a torn pair stays fail-closed under every
        // cause; E3 an empty slot needs no admission because ordinary settlement
        // already permits it; E4 settlement itself is unchanged by C1, so this
        // rule cannot become a second way to declare `Done`.
        //
        // | Rule | C1 cancel | C2 slot | quarantine_admits_occupation | permits_settlement |
        // |---|---|---|---|---|
        // | R1 | no  | Empty      | false (E3) | true  |
        // | R2 | no  | ExactClaim | false      | false |
        // | R3 | no  | OtherClaim | false      | false |
        // | R4 | no  | Corrupt    | false (E2) | false |
        // | R5 | yes | Empty      | false (E3) | true  |
        // | R6 | yes | ExactClaim | true  (E1) | false |
        // | R7 | yes | OtherClaim | true  (E1) | false |
        // | R8 | yes | Corrupt    | false (E2) | false |
        let rules = [
            (false, PhysicalAttemptSlot::Empty, false, "R1"),
            (false, PhysicalAttemptSlot::ExactClaim, false, "R2"),
            (false, PhysicalAttemptSlot::OtherClaim, false, "R3"),
            (false, PhysicalAttemptSlot::Corrupt, false, "R4"),
            (true, PhysicalAttemptSlot::Empty, false, "R5"),
            (true, PhysicalAttemptSlot::ExactClaim, true, "R6"),
            (true, PhysicalAttemptSlot::OtherClaim, true, "R7"),
            (true, PhysicalAttemptSlot::Corrupt, false, "R8"),
        ];
        for (cancellation_requested, slot, expected, rule) in rules {
            assert_eq!(
                slot.quarantine_admits_occupation(cancellation_requested),
                expected,
                "{rule}"
            );
            // E4: settlement never widens, whatever the cancellation intent is.
            assert_eq!(
                slot.permits_settlement(),
                slot == PhysicalAttemptSlot::Empty,
                "{rule} settlement unchanged"
            );
        }
    }

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
        assert_eq!(
            begin_physical_attempt(true, Empty, false, false),
            Install,
            "R1/E1"
        );
        assert_eq!(
            handoff_physical_attempt(true, Empty, false, false),
            Install,
            "R1/E1"
        );
        assert_eq!(
            finish_physical_attempt(Empty, false, false),
            Stutter,
            "R1/E2"
        );
        assert_eq!(
            begin_physical_attempt(true, ExactClaim, false, false),
            Stutter,
            "R2/E2"
        );
        assert_eq!(
            handoff_physical_attempt(true, ExactClaim, false, false),
            Stutter,
            "R2/E2"
        );
        assert_eq!(
            finish_physical_attempt(ExactClaim, false, false),
            Clear,
            "R2/E2"
        );
        assert_eq!(
            begin_physical_attempt(true, OtherClaim, false, false),
            Blocked,
            "R3/E3"
        );
        assert_eq!(
            handoff_physical_attempt(true, OtherClaim, false, false),
            Replace,
            "R4/E6"
        );
        assert_eq!(
            finish_physical_attempt(OtherClaim, false, false),
            Fenced,
            "R3/E3"
        );
        for slot in [Empty, ExactClaim, OtherClaim, Corrupt] {
            assert_eq!(
                begin_physical_attempt(false, slot, false, false),
                Fenced,
                "R4/E4"
            );
            assert_eq!(
                handoff_physical_attempt(false, slot, false, false),
                Fenced,
                "R5/E4"
            );
        }
        assert_eq!(
            begin_physical_attempt(true, Corrupt, false, false),
            CorruptTransition,
            "R5/E5"
        );
        assert_eq!(
            finish_physical_attempt(Corrupt, false, false),
            CorruptTransition,
            "R5/E5"
        );
        assert_eq!(
            handoff_physical_attempt(true, Corrupt, false, false),
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
            current.relinquish(
                8,
                false,
                PhysicalAttemptSlot::Empty,
                None,
                13,
                ReleasedAttempt::Unspent
            ),
            RelinquishTransition::Fenced
        );
        assert_eq!(
            current.relinquish(
                7,
                true,
                PhysicalAttemptSlot::Empty,
                None,
                13,
                ReleasedAttempt::Unspent
            ),
            RelinquishTransition::Fenced
        );
    }

    #[test]
    fn relinquish_preserves_recovery_evidence_for_a_stale_physical_attempt() {
        // Cause/effect graph: C1 exact mutation claim; C2 physical slot is
        // empty, exact, other, or torn; C3 retry budget is zero, ordinary, or
        // MAX. Effects: E1 Pending rollback; E2 fence; E3 retain Leased so the
        // adapter can expire it for immediate recovery; E4 reject corrupt
        // persistence; E5 preserve C3 exactly on every applied transition.
        //
        // | Rule | C1 | C2 | C3 | Effect |
        // |---|---|---|---|---|
        // | R1 | exact | empty | any | E1 Pending + E5 |
        // | R2 | exact | exact | any | E2 Fenced |
        // | R3 | exact | other | any | E3 Recovering Leased + E5 |
        // | R4 | exact | corrupt | any | E4 Corrupt |
        // | R5 | stale | any | any | E2 Fenced |
        //
        // Invariant: an occupied predecessor slot is never cleared, and its
        // successor never loses the recovery classification by becoming an
        // ordinary fresh Pending dispatch.
        let current = leased(8);
        for attempt_count in [0, 7, u64::MAX] {
            assert_eq!(
                current.relinquish(
                    8,
                    true,
                    PhysicalAttemptSlot::Empty,
                    None,
                    attempt_count,
                    ReleasedAttempt::Unspent
                ),
                RelinquishTransition::Pending {
                    state: DispatchTransition {
                        state: crate::DispatchState::Pending,
                        ..current
                    },
                    attempt_count,
                },
                "R1/E1+E5"
            );
            assert_eq!(
                current.relinquish(
                    8,
                    true,
                    PhysicalAttemptSlot::OtherClaim,
                    None,
                    attempt_count,
                    ReleasedAttempt::Unspent
                ),
                RelinquishTransition::Recovering {
                    state: current,
                    attempt_count,
                },
                "R3/E3+E5"
            );
        }
        assert_eq!(
            current.relinquish(
                8,
                true,
                PhysicalAttemptSlot::ExactClaim,
                None,
                7,
                ReleasedAttempt::Unspent
            ),
            RelinquishTransition::Fenced,
            "R2/E2"
        );
        assert_eq!(
            current.relinquish(
                8,
                true,
                PhysicalAttemptSlot::Corrupt,
                None,
                7,
                ReleasedAttempt::Unspent
            ),
            RelinquishTransition::Corrupt,
            "R4/E4"
        );
    }

    #[test]
    fn a_blocked_recovery_reclaim_refunds_its_attempt_within_a_bounded_window() {
        // Rows B1-B4 of `blocked_reclaim_attempt_count`, exhaustively over the
        // window edges, plus the relinquish integration: an exact recovery
        // claim deferred behind another claim's occupied slot stays Leased
        // (Recovering) and writes the refunded count. Past the window it
        // spends again, so a predecessor that never closes spends the budget
        // (retry exhaustion still waits for its slot to close).
        let failed = ReleasedAttempt::Failed { max_attempts: 5 };
        for (row, attempt, reclaims, count, expected) in [
            ("B1 first re-claim", ReleasedAttempt::Unspent, Some(1), 1, 0),
            (
                "B1 last free re-claim",
                ReleasedAttempt::Unspent,
                Some(BLOCKED_RECLAIM_LIMIT),
                4,
                3,
            ),
            ("B1 saturates", ReleasedAttempt::Unspent, Some(1), 0, 0),
            (
                "B2 past the window",
                ReleasedAttempt::Unspent,
                Some(BLOCKED_RECLAIM_LIMIT + 1),
                4,
                4,
            ),
            (
                "B2 far past",
                ReleasedAttempt::Unspent,
                Some(u64::MAX),
                4,
                4,
            ),
            ("B3 no predecessor", ReleasedAttempt::Unspent, None, 4, 4),
            ("B3 not older", ReleasedAttempt::Unspent, Some(0), 4, 4),
            ("B4 failed", failed, Some(1), 4, 4),
        ] {
            assert_eq!(
                blocked_reclaim_attempt_count(attempt, reclaims, count),
                expected,
                "{row}"
            );
        }

        let current = leased(30);
        for (row, predecessor_epoch, count, expected) in [
            ("B1", Some(29), 2, 1),
            ("B2", Some(30 - BLOCKED_RECLAIM_LIMIT - 1), 2, 2),
            ("B3", None, 2, 2),
            ("B3 equal epoch", Some(30), 2, 2),
            ("B3 newer predecessor", Some(31), 2, 2),
        ] {
            assert_eq!(
                current.relinquish(
                    30,
                    true,
                    PhysicalAttemptSlot::OtherClaim,
                    predecessor_epoch,
                    count,
                    ReleasedAttempt::Unspent
                ),
                RelinquishTransition::Recovering {
                    state: current,
                    attempt_count: expected,
                },
                "{row} relinquish"
            );
        }
        // An empty slot is no blocked re-claim: the ordinary rollback keeps
        // the count even when a stale predecessor epoch is supplied.
        assert_eq!(
            current.relinquish(
                30,
                true,
                PhysicalAttemptSlot::Empty,
                Some(29),
                2,
                ReleasedAttempt::Unspent
            ),
            RelinquishTransition::Pending {
                state: DispatchTransition {
                    state: crate::DispatchState::Pending,
                    ..current
                },
                attempt_count: 2,
            },
            "empty slot"
        );
    }

    #[test]
    fn a_failed_attempt_spends_one_attempt_and_exhausts_into_retry_exhaustion() {
        // Cause/effect graph: C1 exact / stale claim; C2 physical slot empty,
        // exact, other or torn; C3 the attempt count before the release against
        // max_attempts (remaining / last / already exhausted). Effects: E1
        // Pending with one attempt spent; E2 Leased, left for the adapter to
        // expire, with one attempt spent, so the retry-exhaustion claim
        // terminalizes it; E3 Recovering with the count unchanged (the
        // recovery re-claim spends it); E4 fence; E5 reject corrupt persistence.
        //
        // | Rule | C1 | C2 | C3 | Effect |
        // |---|---|---|---|---|
        // | F1 | exact | empty | count + 1 < max (remaining) | E1 |
        // | F2 | exact | empty | count + 1 == max (last) | E2 |
        // | F3 | exact | empty | count >= max (exhausted), incl. u64::MAX | E2, saturating |
        // | F4 | exact | other | any | E3 |
        // | F5 | exact | exact | any | E4 |
        // | F6 | exact | corrupt | any | E5 |
        // | F7 | stale epoch or owner | any | any | E4 |
        let current = leased(8);
        let failed = ReleasedAttempt::Failed { max_attempts: 5 };
        let pending = DispatchTransition {
            state: crate::DispatchState::Pending,
            ..current
        };
        for (rule, count, expected) in [
            (
                "F1",
                0,
                RelinquishTransition::Pending {
                    state: pending,
                    attempt_count: 1,
                },
            ),
            (
                "F1",
                3,
                RelinquishTransition::Pending {
                    state: pending,
                    attempt_count: 4,
                },
            ),
            (
                "F2",
                4,
                RelinquishTransition::Recovering {
                    state: current,
                    attempt_count: 5,
                },
            ),
            (
                "F3",
                9,
                RelinquishTransition::Recovering {
                    state: current,
                    attempt_count: 10,
                },
            ),
            (
                "F3",
                u64::MAX,
                RelinquishTransition::Recovering {
                    state: current,
                    attempt_count: u64::MAX,
                },
            ),
        ] {
            assert_eq!(
                current.relinquish(8, true, PhysicalAttemptSlot::Empty, None, count, failed),
                expected,
                "{rule} count {count}"
            );
        }
        for count in [0, 4, u64::MAX] {
            assert_eq!(
                current.relinquish(
                    8,
                    true,
                    PhysicalAttemptSlot::OtherClaim,
                    None,
                    count,
                    failed
                ),
                RelinquishTransition::Recovering {
                    state: current,
                    attempt_count: count,
                },
                "F4"
            );
        }
        assert_eq!(
            current.relinquish(8, true, PhysicalAttemptSlot::ExactClaim, None, 0, failed),
            RelinquishTransition::Fenced,
            "F5"
        );
        assert_eq!(
            current.relinquish(8, true, PhysicalAttemptSlot::Corrupt, None, 0, failed),
            RelinquishTransition::Corrupt,
            "F6"
        );
        assert_eq!(
            current.relinquish(7, true, PhysicalAttemptSlot::Empty, None, 0, failed),
            RelinquishTransition::Fenced,
            "F7 stale epoch"
        );
        assert_eq!(
            current.relinquish(8, false, PhysicalAttemptSlot::Empty, None, 0, failed),
            RelinquishTransition::Fenced,
            "F7 stale owner"
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
        // A1: the new adoption is admission-only, not an alternate ordinary
        // claim selector. Causes are exact original identity, unbound legacy
        // shape, no lease/assignment/credential authority, empty physical slot,
        // no closing residue, Pending phase, and an incrementable epoch. A2:
        // any failed cause stutters; A3: an otherwise eligible MAX epoch errors;
        // A4: the legacy never-claimed zero sentinel mints the first grant.
        // The full Boolean/phase/slot cross-product below covers interacting
        // refusals, including cancelled rows (whose intent must be retained).
        for phase in [
            crate::DispatchState::Reserved,
            crate::DispatchState::ReservationLeased,
            crate::DispatchState::Pending,
            crate::DispatchState::Leased,
            crate::DispatchState::Awaiting,
            crate::DispatchState::DeadLetter,
            crate::DispatchState::Superseded,
        ] {
            for bits in 0..32_u8 {
                for slot in [
                    PhysicalAttemptSlot::Empty,
                    PhysicalAttemptSlot::ExactClaim,
                    PhysicalAttemptSlot::OtherClaim,
                    PhysicalAttemptSlot::Corrupt,
                ] {
                    let original_exact = bits & 1 != 0;
                    let legacy_unbound = bits & 2 != 0;
                    let authority_empty = bits & 4 != 0;
                    let closing = bits & 8 != 0;
                    let cancelled = bits & 16 != 0;
                    for (epoch, successor) in [
                        (0, Some(1)),
                        (1, Some(2)),
                        (4, Some(5)),
                        (u64::MAX - 1, Some(u64::MAX)),
                        (u64::MAX, None),
                    ] {
                        let original = DispatchTransition {
                            state: phase,
                            lease_epoch: epoch,
                            cancellation_requested: cancelled,
                        };
                        let adopted = original.adopt_pending_reservation(
                            original_exact,
                            legacy_unbound,
                            authority_empty,
                            slot,
                            closing,
                        );
                        let eligible = phase == crate::DispatchState::Pending
                            && original_exact
                            && legacy_unbound
                            && authority_empty
                            && slot == PhysicalAttemptSlot::Empty
                            && !closing;
                        if !eligible {
                            assert_eq!(adopted, Ok(None), "A2");
                        } else if epoch == u64::MAX {
                            assert_eq!(
                                adopted,
                                Err(DispatchTransitionError::LeaseEpochExhausted),
                                "A3"
                            );
                        } else {
                            assert_eq!(
                                adopted,
                                Ok(Some(DispatchTransition {
                                    state: crate::DispatchState::ReservationLeased,
                                    lease_epoch: successor.expect("A1/A4 incrementable epoch"),
                                    cancellation_requested: cancelled,
                                })),
                                "A1"
                            );
                        }
                    }
                }
            }
        }
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
            clear_retry_deadline,
        } = old.cancel().unwrap()
        else {
            panic!("leased row must be cancellable")
        };
        assert!(revoked_lease);
        assert!(
            clear_retry_deadline,
            "first cancellation wakes a deferred row"
        );
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
                clear_retry_deadline: false,
            }
        );
    }

    /// Retry-exhaustion cause/effect graph:
    /// C1=phase is Leased; C2=lease expiry is present and reached (`now >=`
    /// expiry, the one Expiry law); C3=attempt_count reaches max_attempts.
    /// E1=eligible for the one terminal claim. Constraints: the exact expiry
    /// boundary is already expired, and retrying terminalization may increase
    /// attempts beyond the threshold without becoming ineligible.
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
        // R1 at the boundary: now == expiry has expired.
        assert!(retry_exhaustion_eligible(
            crate::DispatchState::Leased,
            Some(10),
            3,
            3,
            10
        ));
        // R3 one instant before the boundary: still live.
        assert!(!retry_exhaustion_eligible(
            crate::DispatchState::Leased,
            Some(11),
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

    fn arbitrary_released_attempt() -> ReleasedAttempt {
        if kani::any() {
            ReleasedAttempt::Unspent
        } else {
            ReleasedAttempt::Failed {
                max_attempts: kani::any(),
            }
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

    /// @obligation session_execution.one_physical_attempt_per_thread
    #[kani::proof]
    fn quarantine_admission_never_widens_settlement_or_execution() {
        // R1 quantifies every slot and cancellation intent. The new rule may
        // only add quarantine admission; it must never make settlement or
        // physical execution admissible where they were not already.
        let slot = arbitrary_physical_slot();
        let cancellation_requested: bool = kani::any();

        assert_eq!(
            slot.quarantine_admits_occupation(cancellation_requested),
            cancellation_requested
                && matches!(
                    slot,
                    PhysicalAttemptSlot::ExactClaim | PhysicalAttemptSlot::OtherClaim
                )
        );
        // A torn pair is never admitted by any cause.
        if slot == PhysicalAttemptSlot::Corrupt {
            assert!(!slot.quarantine_admits_occupation(cancellation_requested));
        }
        // Settlement stays exactly as strict as before.
        assert_eq!(
            slot.permits_settlement(),
            slot == PhysicalAttemptSlot::Empty
        );
        // Execution admission is owned by
        // physical_attempt_slot_installs_and_clears_only_exact_authority and
        // thread_physical_peer_vetoes_begin_and_handoff. Repeating the same
        // production call on both sides would prove no additional property.
        if slot.quarantine_admits_occupation(cancellation_requested) {
            assert!(!slot.permits_settlement());
        }
    }

    /// @obligation session_execution.one_physical_attempt_per_thread
    #[kani::proof]
    fn quarantined_cancelled_row_can_never_re_enter_execution() {
        // The root-cause theorem. Admitting quarantine on an occupied slot is
        // only safe if the resulting row can never be handed back to a claimant
        // while its predecessor stays unaccounted for. R1 quantifies every lease
        // epoch and slot: quarantine preserves the recorded cancellation intent,
        // and a cancelled dead letter is unconditionally non-requeueable, so the
        // composition has no path back into Pending — hence none into execution.
        let lease_epoch: u64 = kani::any();
        let slot = arbitrary_physical_slot();
        let cancellation_requested: bool = kani::any();
        let leased = DispatchTransition {
            state: crate::DispatchState::Leased,
            lease_epoch,
            cancellation_requested,
        };

        // Quarantine only proceeds on an empty slot or under recorded intent.
        let admissible =
            slot.permits_settlement() || slot.quarantine_admits_occupation(cancellation_requested);
        if admissible && !slot.permits_settlement() {
            // Occupied admission implies the intent was recorded.
            assert!(cancellation_requested);
        }

        // Q1: Leased + exact epoch + matching owner -> Applied(DeadLetter),
        // preserving epoch and cancellation. Check that total result before
        // inspecting its payload: a branch-local false assertion would itself
        // be unreachable when the production reducer satisfies this rule.
        let outcome = leased.exhaust_retries(lease_epoch, true);
        assert_eq!(
            outcome,
            GuardedTransition::Applied(DispatchTransition {
                state: crate::DispatchState::DeadLetter,
                ..leased
            })
        );
        let GuardedTransition::Applied(dead) = outcome else {
            return;
        };
        assert_eq!(dead.state, crate::DispatchState::DeadLetter);
        // Intent survives the disposition change.
        assert_eq!(dead.cancellation_requested, cancellation_requested);
        // A row quarantined under recorded intent is never requeueable.
        if cancellation_requested {
            assert!(dead.requeue_dead_letter().is_none());
        }
        // And a dead letter is never claimable back into a lease.
        assert!(matches!(dead.claim(), Ok(None)));
    }

    /// @obligation session_execution.one_physical_attempt_per_thread
    #[kani::proof]
    fn retired_epoch_cannot_admit_another_execution() {
        // R2 quantifies the full u64 range and both admission routes; no bounded
        // enumeration or trusted external provider is used in this decision.
        let epoch: u64 = kani::any();
        let retired: u64 = kani::any();
        let live: bool = kani::any();
        let slot = arbitrary_physical_slot();
        let peer: bool = kani::any();
        let current = live && attempt_epoch_is_open(epoch, retired);
        if epoch <= retired {
            assert_eq!(
                begin_physical_attempt(current, slot, peer, false),
                PhysicalAttemptTransition::Fenced
            );
            assert_eq!(
                handoff_physical_attempt(current, slot, peer, false),
                PhysicalAttemptTransition::Fenced
            );
        }
        assert_eq!(current, live && epoch > retired);
    }

    /// @obligation remote_worker.execution_requires_claimed_snapshot
    /// @obligation remote_worker.single_live_claim_owner
    /// @obligation session_execution.one_physical_attempt_per_thread
    /// @obligation worker_credential.exact_revision_and_current_epoch_required
    /// @obligation worker_credential.expired_observation_never_executes
    #[kani::proof]
    fn physical_attempt_slot_installs_and_clears_only_exact_authority() {
        let current = kani::any::<bool>();
        let slot = arbitrary_physical_slot();
        let closing: bool = kani::any();
        let closure_evidence: bool = kani::any();
        // R2/R4: neither a current mutation claim nor logical completion can
        // authorize settlement effects while any physical slot remains.
        assert_eq!(
            slot.permits_settlement(),
            slot == PhysicalAttemptSlot::Empty
        );
        let begin = begin_physical_attempt(current, slot, false, closing);
        let handoff = handoff_physical_attempt(current, slot, false, closing);
        let finish = finish_physical_attempt(slot, closing, closure_evidence);

        // Normalization rule: continuation changes only the disposition of a
        // complete predecessor. Every other admission guard/result is shared,
        // including a stale claim combined with corrupt persisted evidence.
        assert_eq!(
            handoff,
            if begin == PhysicalAttemptTransition::Blocked && !closing {
                PhysicalAttemptTransition::Replace
            } else {
                begin
            }
        );
        assert_eq!(
            begin == PhysicalAttemptTransition::Install,
            current && !closing && slot == PhysicalAttemptSlot::Empty
        );
        assert_eq!(
            begin == PhysicalAttemptTransition::Stutter,
            current && !closing && slot == PhysicalAttemptSlot::ExactClaim
        );
        assert_eq!(
            finish == PhysicalAttemptTransition::Clear,
            slot == PhysicalAttemptSlot::ExactClaim && closing == closure_evidence
        );
        if !current {
            assert_eq!(begin, PhysicalAttemptTransition::Fenced);
            assert_eq!(handoff, PhysicalAttemptTransition::Fenced);
        }
        if slot == PhysicalAttemptSlot::OtherClaim {
            assert_ne!(begin, PhysicalAttemptTransition::Install);
            assert_ne!(finish, PhysicalAttemptTransition::Clear);
            assert_eq!(
                handoff == PhysicalAttemptTransition::Replace,
                current && !closing
            );
        }
    }

    /// @obligation session_execution.one_physical_attempt_per_thread
    #[kani::proof]
    fn thread_physical_peer_vetoes_begin_and_handoff() {
        // P1 current/stale, P2 all four slot shapes, P3 begin/handoff,
        // P4 peer absent/present. A peer can only restrict admission; it never
        // changes stale/corrupt classification and never becomes Clear.
        let current = kani::any();
        let slot = arbitrary_physical_slot();
        let handoff = kani::any();
        let peer = kani::any();
        let closing = kani::any();
        let base = if handoff {
            handoff_physical_attempt(current, slot, false, closing)
        } else {
            begin_physical_attempt(current, slot, false, closing)
        };
        let result = if handoff {
            handoff_physical_attempt(current, slot, peer, closing)
        } else {
            begin_physical_attempt(current, slot, peer, closing)
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

    /// @obligation remote_worker.single_live_claim_owner
    /// @obligation session_execution.one_physical_attempt_per_thread
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

    /// @obligation ingress.settle_fence
    /// @obligation run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing
    #[kani::proof]
    fn stale_dispatch_claim_cannot_modify_authoritative_state() {
        let state = arbitrary_transition();
        let candidate_epoch = kani::any();
        let slot = arbitrary_physical_slot();
        // Cause/effect table: C1 the supplied epoch is current/stale; C2 the
        // logical action is settle/relinquish/exhaust; C3 the physical slot is
        // empty/exact/other/corrupt. E1 stale is fenced for every action and
        // slot; E2 current remains owned by each action's dedicated harness.
        // Constraint: epoch inequality is evaluated by the production reducer
        // without constraining Kani's input domain. Rule SF1=C1(stale) x
        // C2(any) x C3(any) -> E1; C1(current) -> E2.
        if candidate_epoch != state.lease_epoch {
            assert_eq!(
                state.settle(candidate_epoch, kani::any(), slot),
                GuardedTransition::Fenced
            );
            assert_eq!(
                state.relinquish(
                    candidate_epoch,
                    kani::any(),
                    slot,
                    None,
                    kani::any(),
                    arbitrary_released_attempt()
                ),
                RelinquishTransition::Fenced
            );
            assert_eq!(
                state.exhaust_retries(candidate_epoch, kani::any()),
                GuardedTransition::Fenced
            );
        }
    }

    /// @obligation run_ingress.relinquish_requires_exact_current_claim_and_preserves_retry_budget
    #[kani::proof]
    fn relinquish_requires_exact_current_claim_and_preserves_retry_budget() {
        let state = arbitrary_transition();
        let claim_epoch: u64 = kani::any();
        let owner_matches: bool = kani::any();
        let slot = arbitrary_physical_slot();
        let attempt_count: u64 = kani::any();
        let result = state.relinquish(
            claim_epoch,
            owner_matches,
            slot,
            None,
            attempt_count,
            ReleasedAttempt::Unspent,
        );

        let exact = state.state == crate::DispatchState::Leased
            && state.lease_epoch == claim_epoch
            && owner_matches;
        if !exact {
            assert_eq!(result, RelinquishTransition::Fenced);
            return;
        }

        match slot {
            PhysicalAttemptSlot::Empty => assert_eq!(
                result,
                RelinquishTransition::Pending {
                    state: DispatchTransition {
                        state: crate::DispatchState::Pending,
                        ..state
                    },
                    attempt_count,
                }
            ),
            PhysicalAttemptSlot::ExactClaim => {
                assert_eq!(result, RelinquishTransition::Fenced);
            }
            PhysicalAttemptSlot::OtherClaim => assert_eq!(
                result,
                RelinquishTransition::Recovering {
                    state,
                    attempt_count,
                }
            ),
            PhysicalAttemptSlot::Corrupt => {
                assert_eq!(result, RelinquishTransition::Corrupt);
            }
        }
    }

    /// @obligation run_ingress.failed_attempt_release_spends_one_attempt_and_exhausts_through_retry_exhaustion
    #[kani::proof]
    fn failed_attempt_release_spends_one_attempt_and_exhausts_through_retry_exhaustion() {
        let state = arbitrary_transition();
        let claim_epoch = kani::any();
        let owner_matches: bool = kani::any();
        let slot = arbitrary_physical_slot();
        let attempt_count: u64 = kani::any();
        let max_attempts: u64 = kani::any();
        let result = state.relinquish(
            claim_epoch,
            owner_matches,
            slot,
            None,
            attempt_count,
            ReleasedAttempt::Failed { max_attempts },
        );

        let exact = state.state == crate::DispatchState::Leased
            && state.lease_epoch == claim_epoch
            && owner_matches;
        if !exact {
            assert_eq!(result, RelinquishTransition::Fenced);
            return;
        }

        let spent = attempt_count.saturating_add(1);
        match slot {
            // One attempt spent; reaching the limit keeps the Leased row for
            // the retry-exhaustion claim instead of returning it to Pending.
            PhysicalAttemptSlot::Empty if spent >= max_attempts => assert_eq!(
                result,
                RelinquishTransition::Recovering {
                    state,
                    attempt_count: spent,
                }
            ),
            PhysicalAttemptSlot::Empty => assert_eq!(
                result,
                RelinquishTransition::Pending {
                    state: DispatchTransition {
                        state: crate::DispatchState::Pending,
                        ..state
                    },
                    attempt_count: spent,
                }
            ),
            PhysicalAttemptSlot::ExactClaim => {
                assert_eq!(result, RelinquishTransition::Fenced);
            }
            // The recovery re-claim spends this attempt; never twice.
            PhysicalAttemptSlot::OtherClaim => assert_eq!(
                result,
                RelinquishTransition::Recovering {
                    state,
                    attempt_count,
                }
            ),
            PhysicalAttemptSlot::Corrupt => {
                assert_eq!(result, RelinquishTransition::Corrupt);
            }
        }
    }

    /// @obligation ingress.settle_fence
    /// @obligation run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing
    /// @obligation session_event_batch.processed_requires_commit_observation_activity_and_dispatch_settlement
    /// @obligation session_execution.initial_event_path_reaches_idle_only_after_full_settlement
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

    /// @obligation run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing
    #[kani::proof]
    fn dispatch_cancel_revokes_old_epoch_and_is_idempotent() {
        // Causes: C1 the row is superseded, dead-lettered, live-leased, or another cancellable
        // phase; C2 its lease epoch is incrementable; C3 cancellation repeats.
        // Effects: E1 superseded rows reject cancellation; E2 a live lease returns
        // to its non-leased phase, advances exactly one epoch, and fences the old
        // claim; E3 other cancellable phases retain phase/epoch and set the flag;
        // E4 a repeat is an applied state stutter with no second revocation or
        // retry-deadline reset; E5 the first request clears any prior deadline.
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
                clear_retry_deadline,
            } => {
                assert!(cancelled.cancellation_requested);
                assert_eq!(clear_retry_deadline, !state.cancellation_requested);
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
                        cancelled.relinquish(
                            state.lease_epoch,
                            true,
                            PhysicalAttemptSlot::Empty,
                            None,
                            kani::any(),
                            arbitrary_released_attempt(),
                        ),
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
                        clear_retry_deadline: false,
                    }
                );
            }
        }
    }

    /// @obligation delegation.independent_child_scheduling_lease
    /// @obligation ingress.epoch_reclaim
    /// @obligation ingress.single_owner
    /// @obligation remote_worker.single_live_claim_owner
    /// @obligation run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing
    /// @obligation session_run.recovery_claim_is_admission_only
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
        // A1-A3 exhaust the production admission-only adoption decision over
        // the original arbitrary phase/epoch/cancellation domain plus all
        // equality, legacy-shape, vacancy and closing observations. No derived
        // authority is assumed: SQL/identity refinement remains native evidence.
        // The ordinary-claim proof above already owns the exact successor law
        // over the full u64 domain, including zero and MAX. Adoption must use
        // that same generation while entering only ReservationLeased.
        let original_exact = kani::any();
        let legacy_unbound = kani::any();
        let authority_empty = kani::any();
        let slot = arbitrary_physical_slot();
        let closing = kani::any();
        let adopted = state.adopt_pending_reservation(
            original_exact,
            legacy_unbound,
            authority_empty,
            slot,
            closing,
        );
        let eligible = state.state == crate::DispatchState::Pending
            && original_exact
            && legacy_unbound
            && authority_empty
            && slot == PhysicalAttemptSlot::Empty
            && !closing;
        if !eligible {
            assert_eq!(adopted, Ok(None));
        } else if state.lease_epoch == u64::MAX {
            assert_eq!(adopted, Err(DispatchTransitionError::LeaseEpochExhausted));
        } else {
            let ordinary = state.claim().unwrap().unwrap();
            assert_eq!(
                adopted,
                Ok(Some(DispatchTransition {
                    state: crate::DispatchState::ReservationLeased,
                    lease_epoch: ordinary.lease_epoch,
                    cancellation_requested: state.cancellation_requested,
                }))
            );
        }
    }

    /// @obligation run_ingress.dispatch_transition_is_claim_fenced_exact_terminal_and_cancel_absorbing
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
