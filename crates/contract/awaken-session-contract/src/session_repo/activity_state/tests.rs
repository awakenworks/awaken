use super::*;
use crate::session_repo::{PersistedSession, SessionRevision, mutation_tests::session};

fn assert_frame(
    actual: &PersistedSession,
    mut expected: PersistedSession,
    epoch: u64,
    active: &[Option<u64>; 2],
    execution: SessionExecutionState,
    rule: &str,
) {
    expected.activity_epoch = epoch;
    expected.active_activity_epochs = active.iter().flatten().copied().collect();
    expected.execution = execution;
    assert_eq!(actual, &expected, "{rule}: complete aggregate frame");
}

#[test]
fn aggregate_activity_decision_table_preserves_exact_members_and_complete_frame() {
    // Causes: C1 historical scalar/revision at zero, one, or the u64 ceiling;
    // C2 each of two admissions is Next or Exact(0/1/MAX); C3 each of two
    // completions is an old/current/future/zero boundary; C4 either terminal
    // state is committed; C5 every later execution target is requested.
    // Rules A: use a u128 successor oracle; accepted epochs alone join the set,
    // rejected requests preserve the complete aggregate, and scalar never falls.
    // S: nonempty membership removes only the named member, returning last iff
    // exactly one member existed; unknown/duplicate requests preserve all fields.
    // L: empty Running + exact nonzero scalar is the legacy singleton. Repeated
    // L can return Some(true) before the application commits Idle: this reducer
    // does NOT alone prove durable exactly-once settlement or choose that commit.
    // T: actual terminal transition clears every member without rewinding the
    // scalar; exact terminal replay stutters and every different target fails.
    // Each step compares the complete production PersistedSession to its prior
    // snapshot with ONLY these expected fields changed. The same existing Session
    // fixture is reused. Five seeds x 4^2 admissions x 7^2 completions x 2 terminal
    // targets = 7,840 rows; all seven terminal follow-up targets run in each row.
    let seeds = [0, 1, u64::MAX - 2, u64::MAX - 1, u64::MAX];
    let admissions = [None, Some(0), Some(1), Some(u64::MAX)];
    let completions = [0, 1, 2, 3, u64::MAX - 2, u64::MAX - 1, u64::MAX];
    for seed in seeds {
        for first in admissions {
            for second in admissions {
                for finish_first in completions {
                    for finish_second in completions {
                        for terminal in [
                            SessionExecutionState::Terminated,
                            SessionExecutionState::ActivationFailed,
                        ] {
                            let mut value = session("activity-table", SessionRevision(seed));
                            value.activity_epoch = seed;
                            let mut high = seed;
                            let mut members = [None; 2];
                            for (index, request) in [first, second].into_iter().enumerate() {
                                let before = value.clone();
                                let requested =
                                    request.map(u128::from).unwrap_or(u128::from(high) + 1);
                                let admitted =
                                    u64::try_from(requested).ok().filter(|epoch| *epoch > high);
                                let actual = match request {
                                    Some(epoch) => {
                                        value.begin_activity_epoch_at(epoch).then_some(epoch)
                                    }
                                    None => value.begin_activity_epoch(),
                                };
                                assert_eq!(
                                    actual, admitted,
                                    "A: exact/nonwrapping admission result"
                                );
                                if let Some(epoch) = admitted {
                                    high = epoch;
                                    members[index] = Some(epoch);
                                }
                                assert_frame(
                                    &value,
                                    before,
                                    high,
                                    &members,
                                    SessionExecutionState::Idle,
                                    "A",
                                );
                            }
                            let before = value.clone();
                            assert_eq!(
                                value.transition_execution(SessionExecutionState::Running),
                                Ok(true)
                            );
                            assert_frame(
                                &value,
                                before,
                                high,
                                &members,
                                SessionExecutionState::Running,
                                "R",
                            );
                            for completion in [finish_first, finish_second] {
                                let before = value.clone();
                                let count = members.iter().flatten().count();
                                let present = members.contains(&Some(completion));
                                let expected = if count == 0 {
                                    (high > 0 && completion == high).then_some(true)
                                } else {
                                    present.then_some(count == 1)
                                };
                                assert_eq!(
                                    value.settle_activity_epoch(completion),
                                    expected,
                                    "S/L: exact settlement result"
                                );
                                for member in &mut members {
                                    if *member == Some(completion) {
                                        *member = None;
                                    }
                                }
                                assert_frame(
                                    &value,
                                    before,
                                    high,
                                    &members,
                                    SessionExecutionState::Running,
                                    "S/L",
                                );
                            }
                            let before = value.clone();
                            assert_eq!(
                                value.transition_execution(terminal),
                                Ok(true),
                                "T: terminal transition"
                            );
                            members = [None; 2];
                            assert_frame(&value, before, high, &members, terminal, "T");
                            for next in [
                                SessionExecutionState::Preparing,
                                SessionExecutionState::Activating,
                                SessionExecutionState::ActivationFailed,
                                SessionExecutionState::Running,
                                SessionExecutionState::Rescheduling,
                                SessionExecutionState::Idle,
                                SessionExecutionState::Terminated,
                            ] {
                                let before = value.clone();
                                let expected = if next == terminal {
                                    Ok(false)
                                } else {
                                    Err(SessionExecutionTransitionError {
                                        from: terminal,
                                        to: next,
                                    })
                                };
                                assert_eq!(
                                    value.transition_execution(next),
                                    expected,
                                    "T: terminal absorption"
                                );
                                assert_eq!(value, before, "T: terminal replay/rejection frame");
                            }
                        }
                    }
                }
            }
        }
    }
}
