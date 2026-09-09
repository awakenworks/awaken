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
                awaken_session_contract::RunErrorKind::Unavailable
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

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(5)]
fn managed_reply_anchor_selects_exact_fenced_commit() {
    // Same R1..R4 cause/effect table as the native test. All Thread/Run equality
    // classes, seven lifecycle kinds, u64 commit/cursor/fence values and 0..3
    // entries are symbolic. The finite identity alphabet represents exact
    // equality only; arbitrary String codecs, async prefix collection and
    // durable admission/publication are outside this direct selector proof.
    let input = || {
        let kind: u8 = kani::any();
        kani::assume(kind < 7);
        Input {
            same_thread: kani::any(),
            same_run: kani::any(),
            kind,
            commit: kani::any(),
            cursor: kani::any(),
        }
    };
    let inputs = [input(), input(), input()];
    let events = [event(inputs[0]), event(inputs[1]), event(inputs[2])];
    let length: usize = kani::any();
    kani::assume(length <= 3);
    check(&inputs[..length], &events[..length], kani::any());
}
