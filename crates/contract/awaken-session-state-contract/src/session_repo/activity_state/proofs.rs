use super::model::TwoSlotMembers;
use super::*;

// A0/A1 partition: any historical u64 scalar, one real Next admission into the
// empty persisted activity set. Exhaustion stutters; otherwise exactly the
// mathematical successor becomes the sole member, with execution unchanged.
// BTreeSet insertion, lookup and Drop are real and all unwinding checks remain.
// The pair and terminal proofs below extend this base case. Collection removal,
// SQL durability and application exactly-once close remain separate gaps.
/// @obligation session_execution.activity_receipts_are_unique_durable_and_exactly_settled
/// @kani profile=activity-admission-height-v1
#[kani::proof]
#[kani::unwind(4)]
fn activity_next_epoch_inserts_exact_checked_successor() {
    let seed: u64 = kani::any();
    let mut epoch = seed;
    let mut active = BTreeSet::new();
    let mut execution = SessionExecutionState::Idle;
    let actual = ActivityState {
        epoch: &mut epoch,
        active: &mut active,
        execution: &mut execution,
    }
    .begin_activity_epoch();
    if seed == u64::MAX {
        assert!(
            actual.is_none() && epoch == seed && active.is_empty(),
            "N1 exhausted activity is an exact stutter"
        );
    } else {
        let successor = seed + 1;
        assert!(
            actual == Some(successor) && epoch == successor,
            "N2 admission returns and stores the successor"
        );
        assert!(
            active.len() == 1 && active.contains(&successor),
            "N3 admission inserts exactly its successor"
        );
    }
    assert!(
        execution == SessionExecutionState::Idle,
        "N4 admission preserves execution"
    );
}

// E0/E1 partition: arbitrary historical and requested u64 epochs with an empty
// real set. A request is admitted iff strictly above the high-water (therefore
// positive); admission stores/inserts that exact value, rejection stutters.
// This complements the Next base case without assuming uniqueness or replacing
// the real BTreeSet. The four pair proofs below establish two-call admission;
// real collection settlement remains a separate gap.
/// @obligation session_execution.activity_receipts_are_unique_durable_and_exactly_settled
/// @kani profile=activity-admission-height-v1
#[kani::proof]
#[kani::unwind(4)]
fn activity_exact_epoch_inserts_only_fresh_positive_member() {
    let seed: u64 = kani::any();
    let requested: u64 = kani::any();
    let mut epoch = seed;
    let mut active = BTreeSet::new();
    let mut execution = SessionExecutionState::Idle;
    let actual = ActivityState {
        epoch: &mut epoch,
        active: &mut active,
        execution: &mut execution,
    }
    .begin_activity_epoch_at(requested);
    assert!(
        actual == (requested > seed),
        "E1 exact admission requires a fresh positive epoch"
    );
    if requested > seed {
        assert!(
            epoch == requested && active.len() == 1 && active.contains(&requested),
            "E2 exact admission stores only its requested member"
        );
    } else {
        assert!(
            epoch == seed && active.is_empty(),
            "E3 rejected exact admission stutters"
        );
    }
    assert!(
        execution == SessionExecutionState::Idle,
        "E4 exact admission preserves execution"
    );
}

// T partition on the real BTreeSet, with ONE real admission: a real member is
// present, the actual terminal target clears it without rewinding the scalar,
// replay is idempotent and a subsequent Running transition is rejected. One
// admission is what fits: two real B-tree insertions plus the clear exceeded a
// 40 GiB cap on the pinned toolchain (measured on this harness's own code,
// unchanged, on the same host), the same wall the two-admission proofs hit.
// The two-member case is covered by the model proof below, and the model is
// held to the real set by the property test in tests.rs.
/// @obligation session_execution.activity_receipts_are_unique_durable_and_exactly_settled
/// @kani profile=activity-terminal-height-v1
#[kani::proof]
#[kani::unwind(4)]
fn activity_terminal_transition_clears_one_real_member() {
    let mut epoch = kani::any::<u64>();
    let mut active = BTreeSet::new();
    let mut execution = SessionExecutionState::Running;
    let mut state = ActivityState {
        epoch: &mut epoch,
        active: &mut active,
        execution: &mut execution,
    };
    let admitted = state.begin_activity_epoch();
    let high = *state.epoch;
    assert!(
        state.active.len() == usize::from(admitted.is_some()),
        "T0 the real set holds exactly the admitted member"
    );
    check_terminal_from_admitted(&mut state, high);
}

// The same terminal table after TWO admissions, over the two-slot model.
/// @obligation session_execution.activity_receipts_are_unique_durable_and_exactly_settled
#[kani::proof]
#[kani::unwind(4)]
fn activity_two_slot_terminal_transition_clears_members() {
    let mut epoch = kani::any::<u64>();
    let mut active = TwoSlotMembers::default();
    let mut execution = SessionExecutionState::Running;
    let mut state = ActivityState {
        epoch: &mut epoch,
        active: &mut active,
        execution: &mut execution,
    };
    let _ = state.begin_activity_epoch();
    let _ = state.begin_activity_epoch();
    let high = *state.epoch;
    check_terminal_from_admitted(&mut state, high);
}

/// The terminal decision table both proofs share, from a Running state holding
/// whatever the admissions above produced.
fn check_terminal_from_admitted<Members: ActivityMembers>(
    state: &mut ActivityState<'_, Members>,
    high: u64,
) {
    let terminal = if kani::any() {
        SessionExecutionState::Terminated
    } else {
        SessionExecutionState::ActivationFailed
    };
    assert!(
        state.transition_execution(terminal) == Ok(true),
        "T1 actual terminal transition accepted"
    );
    assert!(
        state.active.is_empty(),
        "T2 terminal clears every actual activity"
    );
    assert!(
        *state.epoch == high && *state.execution == terminal,
        "T3 terminal scalar execution frame"
    );
    assert!(
        state.transition_execution(terminal) == Ok(false),
        "T4 terminal replay stutters"
    );
    assert!(
        state
            .transition_execution(SessionExecutionState::Running)
            .is_err(),
        "T5 terminal rejects Running"
    );
    assert!(
        state.active.is_empty() && *state.epoch == high && *state.execution == terminal,
        "T6 replay rejection frame"
    );
}

fn check_members(active: &impl ActivityMembers, members: [Option<u64>; 2]) {
    let count = usize::from(members[0].is_some()) + usize::from(members[1].is_some());
    assert!(active.len() == count, "M1 exact activity cardinality");
    for member in members.into_iter().flatten() {
        assert!(
            active.contains(&member),
            "M2 every admitted unsettled member survives"
        );
    }
}

// A1-A4 partition the four Next/Exact call-mode pairs, without restricting any
// historical/requested u64 value or assuming admission, membership or uniqueness.
// Each mode calls the same production reducer twice. The u128/order oracle and
// exact member/cardinality checks apply after both calls, including all rejection
// and exhaustion combinations. Their union is the original two-call domain.
//
// The member set is `TwoSlotMembers`, not the real `BTreeSet`. The reducer is
// generic over `ActivityMembers`, so this is the same production code with a
// different set behind it. Symbolically executing the real B-tree twice does
// not fit in memory: exact/exact reached 54.9 GiB and ran out on a 60 GiB host,
// and bounding all three symbols to 0..=5 (58 GiB) or enumerating 64 concrete
// triples (56 GiB) did not help, because the cost is the B-tree code, not the
// value range. What stays on the real set: both one-admission bases and the
// terminal proof above, and the property test in `tests.rs` that checks the
// model against `BTreeSet<u64>` on two-admission traces.
fn check_admission<const EXACT: bool, Members: ActivityMembers>(
    state: &mut ActivityState<'_, Members>,
    members: &mut [Option<u64>; 2],
    index: usize,
) {
    let high = *state.epoch;
    let requested = if EXACT {
        u128::from(kani::any::<u64>())
    } else {
        u128::from(high) + 1
    };
    let admitted = u64::try_from(requested).ok().filter(|value| *value > high);
    let actual = if EXACT {
        state
            .begin_activity_epoch_at(requested as u64)
            .then_some(requested as u64)
    } else {
        state.begin_activity_epoch()
    };
    assert!(actual == admitted, "A1 exact nonwrapping admission result");
    members[index] = admitted;
    assert!(
        *state.epoch == admitted.unwrap_or(high),
        "A2 admission high-water frame"
    );
    assert!(
        *state.execution == SessionExecutionState::Idle,
        "A3 admission execution frame"
    );
    check_members(state.active, *members);
}

fn check_admission_pair<const FIRST_EXACT: bool, const SECOND_EXACT: bool>() {
    let mut epoch: u64 = kani::any();
    let mut active = TwoSlotMembers::default();
    let mut execution = SessionExecutionState::Idle;
    let mut state = ActivityState {
        epoch: &mut epoch,
        active: &mut active,
        execution: &mut execution,
    };
    let mut members = [None; 2];
    check_admission::<FIRST_EXACT, _>(&mut state, &mut members, 0);
    check_admission::<SECOND_EXACT, _>(&mut state, &mut members, 1);
}

// The two-admission partitions run the reducer over the two-slot member
// model: no B-tree loops, so no checked loop profile applies to them.
/// @obligation session_execution.activity_receipts_are_unique_durable_and_exactly_settled
#[kani::proof]
#[kani::unwind(4)]
fn activity_two_slot_next_next_admissions_preserve_exact_members() {
    check_admission_pair::<false, false>();
}

/// @obligation session_execution.activity_receipts_are_unique_durable_and_exactly_settled
#[kani::proof]
#[kani::unwind(4)]
fn activity_two_slot_next_exact_admissions_preserve_exact_members() {
    check_admission_pair::<false, true>();
}

/// @obligation session_execution.activity_receipts_are_unique_durable_and_exactly_settled
#[kani::proof]
#[kani::unwind(4)]
fn activity_two_slot_exact_next_admissions_preserve_exact_members() {
    check_admission_pair::<true, false>();
}

/// @obligation session_execution.activity_receipts_are_unique_durable_and_exactly_settled
#[kani::proof]
#[kani::unwind(4)]
fn activity_two_slot_exact_exact_admissions_preserve_exact_members() {
    check_admission_pair::<true, true>();
}
