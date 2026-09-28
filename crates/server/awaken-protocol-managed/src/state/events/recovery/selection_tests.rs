//! Cross-check the production reply anchor selector with independent input facts.

use super::*;
use awaken_agent_contract::{RunId, ThreadId};

#[derive(Clone, Copy)]
struct Input {
    same_thread: bool,
    same_run: bool,
    kind: u8,
    commit: u64,
    cursor: u64,
}

fn event(input: Input) -> RunLifecycleEvent {
    let kind = match input.kind {
        0 => RunLifecycleEventKind::Running,
        1 => RunLifecycleEventKind::Awaiting,
        2 => RunLifecycleEventKind::Resumed,
        3 => RunLifecycleEventKind::Rescheduled,
        4 => RunLifecycleEventKind::Completed,
        5 => RunLifecycleEventKind::Failed,
        6 => RunLifecycleEventKind::Cancelled,
        _ => unreachable!("closed lifecycle kind domain"),
    };
    RunLifecycleEvent {
        cursor: RunLifecycleCursor(input.cursor),
        source_commit_cursor: input.commit,
        thread_id: ThreadId(if input.same_thread { "t" } else { "x" }.into()),
        run_id: RunId(if input.same_run { "r" } else { "x" }.into()),
        kind,
        // The selector consumes the committed kind, not current Run state or
        // an independently fetched Awaiting cause. These fields are unobserved.
        state: awaken_agent_contract::agent::run::RunState::Running,
        await_reason: None,
    }
}

fn check(inputs: &[Input], events: &[RunLifecycleEvent], fence: u64) {
    let result = ManagedState::answered_pending_commit_cursor(
        &ThreadId("t".into()),
        &RunId("r".into()),
        fence,
        events,
    );
    // Oracle uses construction facts, never the production String comparisons,
    // event filter, max_by comparator or lifecycle cursor tie breaker.
    let mut expected = None;
    for input in inputs {
        if input.same_thread
            && input.same_run
            && input.kind == 1
            && input.commit > 0
            && input.commit <= fence
            && expected.is_none_or(|best| input.commit > best)
        {
            expected = Some(input.commit);
        }
    }
    match (expected, result) {
        (Some(expected), Ok(actual)) => {
            assert_eq!(actual, expected, "exact greatest eligible commit")
        }
        (None, Err(StateError::Run(error))) => {
            assert_eq!(
                error.kind,
                awaken_session_error_contract::RunErrorKind::Unavailable
            );
            assert_eq!(error.code, "unavailable");
        }
        _ => panic!("anchor exists exactly when the fenced prefix contains eligible evidence"),
    }
}

#[test]
fn reply_anchor_selection_covers_every_admission_cause_and_competing_order() {
    // C1 exact Thread; C2 exact Run; C3 Awaiting kind (all seven kinds);
    // C4 nonzero commit; C5 commit <= recovery fence. E1 eligible evidence
    // returns its greatest commit; E2 no eligible evidence is retryable
    // Unavailable. Decision R1 C1..C5 -> E1; R2 any false -> E2.
    // R3 multiple eligible entries -> greatest commit regardless of input or
    // lifecycle cursor order; R4 duplicate commits cannot change the anchor.
    // Exhaust the single-entry cause table, then all 0..3-entry words over
    // the adverse classes below. Scalars 0/1/fence/fence+1/MAX expose both
    // inclusivity boundaries; independent lifecycle cursors expose false order.
    for same_thread in [false, true] {
        for same_run in [false, true] {
            for kind in 0..7 {
                for commit in [0, 1, 9, 10, 11, u64::MAX] {
                    let input = Input {
                        same_thread,
                        same_run,
                        kind,
                        commit,
                        cursor: u64::MAX,
                    };
                    check(&[input], &[event(input)], 10);
                }
            }
        }
    }
    let valid = Input {
        same_thread: true,
        same_run: true,
        kind: 1,
        commit: 1,
        cursor: u64::MAX,
    };
    let classes = [
        valid,
        Input {
            commit: 10,
            cursor: 0,
            ..valid
        },
        Input {
            commit: 10,
            cursor: 1,
            ..valid
        },
        Input { commit: 0, ..valid },
        Input {
            commit: 11,
            ..valid
        },
        Input {
            commit: u64::MAX,
            ..valid
        },
        Input {
            same_thread: false,
            commit: 10,
            ..valid
        },
        Input {
            same_run: false,
            commit: 10,
            ..valid
        },
        Input {
            kind: 0,
            commit: 10,
            ..valid
        },
    ];
    check(&[], &[], 10);
    for a in classes {
        check(&[a], &[event(a)], 10);
        for b in classes {
            check(&[a, b], &[event(a), event(b)], 10);
            for c in classes {
                let inputs = [a, b, c];
                let events = [event(a), event(b), event(c)];
                check(&inputs, &events, 10);
            }
        }
    }
    let upper = Input {
        commit: u64::MAX,
        ..valid
    };
    check(&[upper], &[event(upper)], u64::MAX);
    check(&[valid], &[event(valid)], 0);
}

// The production reply selector folds every lifecycle fact through a
// heap-free one-step induction kernel; prefix collection stays at the edge.
#[cfg(kani)]
/// @obligation session.runtime_projection_distinguishes_pending_resolving_and_processed_tools_and_fails_closed
/// @obligation tool_result.pending_commit_coordinate_is_exact
/// @kani profile=solver-cadical-v1
#[kani::proof]
fn managed_reply_anchor_selects_exact_fenced_commit() {
    // Same R1..R4 table as the native selector test. One symbolic fold step
    // covers every Boolean eligibility partition and every u64 current,
    // candidate and fence. The equality below is the induction step for an
    // arbitrary lifecycle prefix: ineligible evidence stutters; eligible
    // evidence replaces the accumulator exactly when it is greater. No
    // assumptions, heap values or bounded list length are needed.
    let selected = if kani::any() {
        Some(kani::any::<u64>())
    } else {
        None
    };
    let same_thread = kani::any::<bool>();
    let same_run = kani::any::<bool>();
    let awaiting = kani::any::<bool>();
    let commit = kani::any::<u64>();
    let fence = kani::any::<u64>();
    let eligible = same_thread && same_run && awaiting && commit != 0 && commit <= fence;
    let expected = if eligible && selected.is_none_or(|current| commit > current) {
        Some(commit)
    } else {
        selected
    };
    assert_eq!(
        advance_answered_pending_commit(selected, same_thread, same_run, awaiting, commit, fence,),
        expected
    );
}

fn selected_run_state(kind: u8) -> Option<awaken_agent_contract::agent::run::RunState> {
    use awaken_agent_contract::agent::run::{EndCause, Failure, RunState};
    Some(match kind {
        0 => RunState::Running,
        1 => RunState::Awaiting,
        2 => RunState::Ended(EndCause::NaturalEnd),
        3 => RunState::Ended(EndCause::MaxSteps),
        4 => RunState::Ended(EndCause::Cancelled),
        5 => RunState::Ended(EndCause::Stopped(String::new())),
        6 => RunState::Ended(EndCause::Error(Failure::Inference {
            code: String::new(),
            message: String::new(),
        })),
        7 => RunState::Ended(EndCause::Error(Failure::CapabilityBound)),
        8 => RunState::Ended(EndCause::Error(Failure::StateConflict)),
        9 => RunState::Ended(EndCause::Indeterminate),
        10 => return None,
        _ => unreachable!("closed Run state and failure domain"),
    })
}

// This fixture belongs to the existing strict pending selector. It introduces
// no second production lifecycle or candidate-admission rule.
#[cfg(test)]
fn check_terminal_ticket_selection(kind: u8, latest: bool, reverse: bool, tickets: u8) {
    use awaken_agent_contract::agent::awaiting::{
        AwaitTarget, PendingTool, ResumeTicket, ToolAwaitReason, ToolCallAuthority,
    };
    use awaken_agent_contract::agent::run::{Record, RunState};
    use awaken_agent_contract::thread::read::recovery::{RunRecoverySnapshot, RunResumeTicket};

    let state = selected_run_state(kind).expect("native snapshot has explicit current Run truth");
    let current = Record {
        id: RunId("r".into()),
        thread_id: ThreadId("t".into()),
        state,
    };
    let old = Record {
        id: RunId("o".into()),
        thread_id: ThreadId("t".into()),
        state: RunState::Awaiting,
    };
    let runs = if reverse {
        vec![old, current]
    } else {
        vec![current, old]
    };
    let ticket = |id: &str| RunResumeTicket {
        run_id: RunId(id.into()),
        ticket: ResumeTicket::new(
            "c",
            RunId(id.into()),
            ThreadId("t".into()),
            "s",
            "f",
            AwaitTarget::ToolCall {
                reason: ToolAwaitReason::ClientExecution,
                call_id: "a".into(),
                tool: PendingTool {
                    tool_id: "x".into(),
                    arguments: serde_json::Value::Null,
                },
                authority: ToolCallAuthority::RuntimeToolBatch,
            },
        ),
    };
    let mut resume_tickets = Vec::new();
    if tickets & 1 != 0 {
        resume_tickets.push(ticket("r"));
    }
    if tickets & 2 != 0 {
        resume_tickets.push(ticket("o"));
    }
    let snapshot = RunRecoverySnapshot {
        thread_id: ThreadId("t".into()),
        // A populated latest coordinate must override the older claimed Run.
        claimed_run_id: RunId(if latest { "o" } else { "r" }.into()),
        latest_run_id: latest.then(|| RunId("r".into())),
        runs,
        resume_tickets,
        messages: Vec::new(),
        message_commit_cursors: Vec::new(),
        commit_timestamps: Default::default(),
        state: Vec::new(),
        state_commit_cursors: Vec::new(),
        events: Vec::new(),
        thread_version: 2,
        store_cursor: 2,
        next_commit_ordinal: 1,
    };
    let result = ManagedState::pending_ticket_from_recovery_snapshot(&snapshot);
    // Independent construction facts, not RunState::is_terminal or a second
    // copy of the production predicate, select the expected decision-table row.
    if !latest {
        // R0: committed Runs without their latest coordinate are a damaged
        // prefix; the claimed Run is never promoted to current.
        assert!(
            result
                .as_ref()
                .is_err_and(StateError::is_projection_recovery_required),
            "R0 omitted latest coordinate fails closed"
        );
    } else if kind != 1 {
        assert!(
            matches!(result, Ok(None)),
            "R1/R2 no candidate for a closed or running Run"
        );
    } else if tickets & 1 == 0 {
        assert!(
            result.is_err(),
            "R3 positive Awaiting without its ticket is damaged authority"
        );
    } else {
        let (run, correlation, pending) = result.expect("R4 healthy wait").expect("R4 candidate");
        assert_eq!(run.0, "r");
        assert_eq!(correlation, "c");
        assert_eq!(pending.tool_use_id, "a");
        assert_eq!(pending.name, "x");
        assert!(pending.client_executed);
        assert_eq!(pending.input, serde_json::Value::Null);
    }
}

#[test]
fn terminal_pending_selection_covers_current_run_and_residual_ticket_table() {
    // Causes: C1 current Run is Running/Awaiting/one of all eight Ended failure
    // classes; C2 latest Run overrides an old claimed Awaiting Run, or the
    // latest coordinate is absent while Runs are committed; C3 current/old
    // tickets are each absent or present; C4 Run vector order is reversed.
    // Effects: E1 no candidate; E2 damaged-wait error; E3 exact current
    // candidate. Decision table:
    // R0 latest absent + any C1/C3/C4 => E2 (no claimed-Run fallback).
    // R1 Ended (every cause) + latest present + any C3/C4 => E1, even with
    // residual tickets.
    // R2 Running + latest present + any C3/C4 => E1.
    // R3 Awaiting + current ticket absent => E2 (old ticket cannot substitute).
    // R4 Awaiting + current ticket present => E3 (positive non-vacuity control).
    // Exhaust 10*2*2*4 = 160 rows. The immutable snapshot is the read boundary;
    // async root admission/CAS, executor effects and SQL consistency are not
    // claimed by this selector test. Exact retained-receipt replay remains
    // covered by exact_event_batch_replay_precedes_cold_projection_recovery.
    for kind in 0..10 {
        for latest in [false, true] {
            for reverse in [false, true] {
                for tickets in 0..4 {
                    check_terminal_ticket_selection(kind, latest, reverse, tickets);
                }
            }
        }
    }
}

fn check_ticket_admission(kind: u8, shape: u8, run_matches: bool, thread_matches: bool) {
    use awaken_agent_contract::agent::awaiting::{
        AwaitTarget, PendingTool, ResumeTicket, ToolAwaitReason, ToolCallAuthority,
    };
    let state = selected_run_state(kind);
    let run = RunId("r".into());
    let thread = ThreadId("t".into());
    let ticket = ResumeTicket::new(
        "c",
        RunId(if run_matches { "r" } else { "x" }.into()),
        ThreadId(if thread_matches { "t" } else { "x" }.into()),
        "s",
        "f",
        AwaitTarget::ToolCall {
            reason: ToolAwaitReason::ClientExecution,
            call_id: "a".into(),
            tool: PendingTool {
                tool_id: "x".into(),
                arguments: serde_json::Value::Null,
            },
            authority: ToolCallAuthority::RuntimeToolBatch,
        },
    );
    let observation = match shape {
        0 => PendingTicketObservation::Absent,
        1 => PendingTicketObservation::Multiple,
        2 => PendingTicketObservation::One(&ticket),
        _ => unreachable!("closed ticket observation domain"),
    };
    let result = pending_ticket_admission(state.as_ref(), &run, &thread, observation);
    // Independent fixture indices choose the oracle, not a second RunState
    // match or the production identity validator's result.
    let expected = if kind == 0 || (2..=9).contains(&kind) {
        PendingTicketAdmission::Inactive
    } else if shape == 0 {
        PendingTicketAdmission::Missing {
            positive_awaiting: kind == 1,
        }
    } else if shape == 1 {
        PendingTicketAdmission::Multiple
    } else if run_matches && thread_matches {
        PendingTicketAdmission::Admit
    } else {
        PendingTicketAdmission::Inconsistent
    };
    assert_eq!(result, expected);
}

#[test]
fn ticket_admission_covers_all_states_cardinalities_and_identity_classes() {
    // Causes: C1 eleven lifecycle classes (Running, Awaiting, all eight closed
    // EndCause/Failure classes, unknown legacy); C2 absent/multiple/one ticket;
    // C3 Run and Thread identities each exact/foreign. Effects/decision table:
    // A1 Running or Ended + any C2/C3 => Inactive before every corruption rule.
    // A2 Awaiting/unknown + absent => Missing, positive_awaiting only for Awaiting.
    // A3 Awaiting/unknown + multiple => Multiple before identity validation.
    // A4 Awaiting/unknown + one + both identities exact => Admit (positive control).
    // A5 Awaiting/unknown + one + either identity foreign => Inconsistent.
    // Exhaust 11*3*2*2 = 132 classes. The separate 160-row native snapshot table
    // owns latest/claimed, vector order and old/current ticket selection; the
    // 16-row send_events test owns new-key rejection and exact-replay effect
    // frames. Neither adapter boundary is replaced by this local kernel proof.
    for kind in 0..11 {
        for shape in 0..3 {
            for run_matches in [false, true] {
                for thread_matches in [false, true] {
                    check_ticket_admission(kind, shape, run_matches, thread_matches);
                }
            }
        }
    }
}

#[cfg(kani)]
/// @obligation tool_result.terminal_run_rejects_late_result
/// @kani profile=solver-cadical-v1
#[kani::proof]
#[kani::unwind(16)]
fn managed_ticket_admission_preserves_terminal_and_identity_priority() {
    // Same A1..A5 132-class table as native. Total modulo maps every u8 input;
    // there is no input-pruning assume. Real RunState, owned identity Strings,
    // a real borrowed ResumeTicket and the existing contract identity validator
    // remain in this proof. The tool payload is Null and end-detail Strings are
    // empty. Native snapshot/admission tests, not this kernel, cover lookup,
    // historical audit decoding, Pending materialization and async root effects.
    check_ticket_admission(
        kani::any::<u8>() % 11,
        kani::any::<u8>() % 3,
        kani::any(),
        kani::any(),
    );
}
