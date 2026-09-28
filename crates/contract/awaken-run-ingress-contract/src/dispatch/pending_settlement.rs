// @formal entrypoint ingress.pending_input_exactly_once
use crate::{DispatchTransition, GuardedTransition, PhysicalAttemptSlot};

/// One authoritative settlement plan for the Dispatch row and its pending
/// input. Storage adapters translate this plan into their existing atomic
/// transaction; they do not independently decide which pending rows disappear.
#[derive(Debug, Clone, Copy)]
pub enum PendingInputSettlementPlan<'a> {
    /// The supplied epoch or physical-attempt evidence is not authoritative.
    /// Neither the Dispatch row nor pending input may change.
    Fenced,
    /// The exact claim reached another durable await. Only message identities
    /// reported as consumed by that attempt disappear.
    Awaiting {
        next: DispatchTransition,
        consumed: &'a [String],
    },
    /// The exact claim reached terminal delivery. All input bound to this Run,
    /// plus any unbound input consumed by the same attempt, disappears.
    Done {
        run_id: &'a RunId,
        consumed: &'a [String],
    },
}

impl PendingInputSettlementPlan<'_> {
    /// Whether this exact durable identity belongs to the settlement's
    /// deletion set. Accepting only the two durable identity fields prevents
    /// settlement from depending on unrelated pending-input payload data.
    #[must_use]
    pub fn removes(self, input_run_id: &RunId, input_message_id: &str) -> bool {
        match self {
            Self::Fenced => false,
            Self::Awaiting { consumed, .. } => consumed
                .iter()
                .any(|message_id| message_id == input_message_id),
            Self::Done { run_id, consumed } => {
                input_run_id == run_id
                    || consumed
                        .iter()
                        .any(|message_id| message_id == input_message_id)
            }
        }
    }
}

/// Apply the existing Dispatch transition owner and bind its result to the one
/// pending-input deletion plan consumed by every store adapter.
#[must_use]
pub fn pending_input_settlement_plan<'a>(
    current: DispatchTransition,
    claim_epoch: u64,
    outcome: DispatchOutcome,
    slot: PhysicalAttemptSlot,
    run_id: &'a RunId,
    consumed: &'a [String],
) -> PendingInputSettlementPlan<'a> {
    match current.settle(claim_epoch, outcome == DispatchOutcome::Done, slot) {
        GuardedTransition::Fenced => PendingInputSettlementPlan::Fenced,
        GuardedTransition::Applied(next) => PendingInputSettlementPlan::Awaiting { next, consumed },
        GuardedTransition::Removed => PendingInputSettlementPlan::Done { run_id, consumed },
    }
}

#[cfg(test)]
mod pending_settlement_tests {
    use super::*;

    fn input(run_id: &str, message_id: &str) -> PendingInput {
        PendingInput {
            message_id: message_id.into(),
            run_id: RunId(run_id.into()),
            thread_id: ThreadId("thread".into()),
            correlation_id: "correlation".into(),
            available_at_ms: None,
            result: ResumeResult::allow(),
            context_messages: Vec::new(),
        }
    }

    #[test]
    fn settlement_plan_owns_the_exact_pending_deletion_set() {
        // Cause/effect graph: C1 phase is Leased/other; C2 epoch is exact/stale;
        // C3 physical slot is empty/occupied/corrupt; C4 outcome is Awaiting/Done;
        // C5 input Run matches; C6 message id is named. Effects: E1 a fenced
        // combination removes nothing; E2 admitted Awaiting removes exactly C6;
        // E3 admitted Done removes C5 or C6. Constraint: all causes pass through
        // the production DispatchTransition::settle owner before this predicate.
        //
        // | Rule | C1+C2+C3 | C4       | C5 | C6 | Effect |
        // | R1   | rejected | any      | any| any| E1     |
        // | R2   | admitted | Awaiting | any| T  | E2     |
        // | R3   | admitted | Awaiting | any| F  | retain |
        // | R4   | admitted | Done     | T  | any| E3     |
        // | R5   | admitted | Done     | F  | T  | E3     |
        // | R6   | admitted | Done     | F  | F  | retain |
        let settled_run = RunId("run-current".into());
        let consumed = vec!["message-consumed".to_string()];
        for state in [
            DispatchState::Reserved,
            DispatchState::ReservationLeased,
            DispatchState::Pending,
            DispatchState::Leased,
            DispatchState::Awaiting,
            DispatchState::DeadLetter,
            DispatchState::Superseded,
        ] {
            for epoch in [6, 7] {
                for slot in [
                    PhysicalAttemptSlot::Empty,
                    PhysicalAttemptSlot::ExactClaim,
                    PhysicalAttemptSlot::OtherClaim,
                    PhysicalAttemptSlot::Corrupt,
                ] {
                    for outcome in [DispatchOutcome::Awaiting, DispatchOutcome::Done] {
                        for run_id in ["run-current", "run-other"] {
                            for message_id in ["message-consumed", "message-other"] {
                                let candidate = input(run_id, message_id);
                                let plan = pending_input_settlement_plan(
                                    DispatchTransition {
                                        state,
                                        lease_epoch: 7,
                                        cancellation_requested: false,
                                    },
                                    epoch,
                                    outcome,
                                    slot,
                                    &settled_run,
                                    &consumed,
                                );
                                let admitted = state == DispatchState::Leased
                                    && epoch == 7
                                    && slot == PhysicalAttemptSlot::Empty;
                                let expected = admitted
                                    && (candidate.message_id == consumed[0]
                                        || (outcome == DispatchOutcome::Done
                                            && candidate.run_id == settled_run));
                                assert_eq!(
                                    plan.removes(&candidate.run_id, &candidate.message_id),
                                    expected
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(kani)]
/// @obligation ingress.pending_input_exactly_once
#[kani::proof]
fn pending_settlement_plan_is_the_exact_dispatch_projection() {
    // C1-C4/E1-E3 from the native table: quantify every production Dispatch
    // phase/slot/outcome and full stored/claim u64 epochs. Fixed identities keep
    // the independent deletion-set theorem out of this SAT problem. The harness
    // invokes the actual planner and its actual DispatchTransition::settle call;
    // it does not reproduce either transition relation.
    let state = match kani::any::<u8>() % 7 {
        0 => DispatchState::Reserved,
        1 => DispatchState::ReservationLeased,
        2 => DispatchState::Pending,
        3 => DispatchState::Leased,
        4 => DispatchState::Awaiting,
        5 => DispatchState::DeadLetter,
        _ => DispatchState::Superseded,
    };
    let slot = match kani::any::<u8>() % 4 {
        0 => PhysicalAttemptSlot::Empty,
        1 => PhysicalAttemptSlot::ExactClaim,
        2 => PhysicalAttemptSlot::OtherClaim,
        _ => PhysicalAttemptSlot::Corrupt,
    };
    let stored_epoch: u64 = kani::any();
    let claim_epoch: u64 = kani::any();
    let outcome = if kani::any() {
        DispatchOutcome::Done
    } else {
        DispatchOutcome::Awaiting
    };
    let settled_run = RunId(String::new());
    let consumed: [String; 0] = [];
    let cancellation_requested: bool = kani::any();
    let plan = pending_input_settlement_plan(
        DispatchTransition {
            state,
            lease_epoch: stored_epoch,
            cancellation_requested,
        },
        claim_epoch,
        outcome,
        slot,
        &settled_run,
        &consumed,
    );
    let admitted = state == DispatchState::Leased
        && stored_epoch == claim_epoch
        && slot == PhysicalAttemptSlot::Empty;
    if !admitted {
        assert!(matches!(plan, PendingInputSettlementPlan::Fenced));
    } else if outcome == DispatchOutcome::Done {
        assert!(matches!(plan, PendingInputSettlementPlan::Done { .. }));
    } else {
        let PendingInputSettlementPlan::Awaiting { next, .. } = plan else {
            panic!("admitted Awaiting must retain the dispatch")
        };
        assert_eq!(next.state, DispatchState::Awaiting);
        assert_eq!(next.lease_epoch, stored_epoch);
        assert_eq!(next.cancellation_requested, cancellation_requested);
    }
}
