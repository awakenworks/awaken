// @formal evidence session_execution.activity_receipts_are_unique_durable_and_exactly_settled
use super::*;
use crate::SessionRevision;
use crate::session_repo::{PersistedSession, mutation_tests::session};

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

mod model_refinement {
    //! The two-admission Kani proofs run the reducer over `TwoSlotMembers`
    //! instead of the real `BTreeSet<u64>`. This is the evidence that the
    //! substitution changes nothing on the traces those proofs cover: every
    //! step of a random trace runs the same reducer twice, once per set, and
    //! the two must agree on the returned value, the high-water, the execution
    //! state and the exact member set.
    //!
    //! Cause/effect rules, one trace per case:
    //! R1 two admissions, each Next or Exact at any u64 (boundaries weighted),
    //!    from any seed: same result and same complete state after each call.
    //! R2 then up to two settlements naming an admitted member, the current
    //!    high-water or any u64, from Idle or Running: same result and state,
    //!    including the empty-set legacy branch that reads execution.
    //! R3 then optionally a terminal transition: same result, and both sets
    //!    are cleared or both kept.
    //! Every trait method the reducer or the proofs use is therefore exercised
    //! on both sets; `contains` and `len` are the comparison itself.
    use super::super::model::TwoSlotMembers;
    use super::super::{ActivityMembers, ActivityState};
    use super::SessionExecutionState;
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    #[derive(Debug, Clone, Copy)]
    enum SettleTarget {
        Admitted(usize),
        HighWater,
        Any(u64),
    }

    fn epoch() -> impl Strategy<Value = u64> {
        prop_oneof![
            Just(0),
            Just(1),
            Just(2),
            Just(u64::MAX - 2),
            Just(u64::MAX - 1),
            Just(u64::MAX),
            any::<u64>(),
        ]
    }

    fn settle_target() -> impl Strategy<Value = SettleTarget> {
        prop_oneof![
            (0usize..2).prop_map(SettleTarget::Admitted),
            Just(SettleTarget::HighWater),
            epoch().prop_map(SettleTarget::Any),
        ]
    }

    struct Pair {
        real_epoch: u64,
        real: BTreeSet<u64>,
        real_execution: SessionExecutionState,
        model_epoch: u64,
        model: TwoSlotMembers,
        model_execution: SessionExecutionState,
    }

    impl Pair {
        fn new(seed: u64, execution: SessionExecutionState) -> Self {
            Self {
                real_epoch: seed,
                real: BTreeSet::new(),
                real_execution: execution,
                model_epoch: seed,
                model: TwoSlotMembers::default(),
                model_execution: execution,
            }
        }

        /// Run one reducer step on both sets and require the same answer.
        fn step<T: PartialEq + std::fmt::Debug>(
            &mut self,
            rule: &str,
            mut operation: impl FnMut(&mut dyn ReducerStep) -> T,
        ) -> T {
            let real = operation(&mut ActivityState {
                epoch: &mut self.real_epoch,
                active: &mut self.real,
                execution: &mut self.real_execution,
            });
            let model = operation(&mut ActivityState {
                epoch: &mut self.model_epoch,
                active: &mut self.model,
                execution: &mut self.model_execution,
            });
            assert_eq!(
                model, real,
                "{rule}: the model returns what the real set returns"
            );
            self.assert_same_state(rule);
            real
        }

        fn assert_same_state(&self, rule: &str) {
            assert_eq!(self.model_epoch, self.real_epoch, "{rule}: high-water");
            assert_eq!(
                self.model_execution, self.real_execution,
                "{rule}: execution"
            );
            assert_eq!(
                ActivityMembers::len(&self.model),
                self.real.len(),
                "{rule}: member count"
            );
            for member in &self.real {
                assert!(
                    ActivityMembers::contains(&self.model, member),
                    "{rule}: the model holds every real member ({member})"
                );
            }
        }
    }

    /// The reducer steps a trace uses, object-safe so one closure drives both
    /// member types.
    trait ReducerStep {
        fn next(&mut self) -> Option<u64>;
        fn exact(&mut self, epoch: u64) -> bool;
        fn settle(&mut self, epoch: u64) -> Option<bool>;
        fn terminal(&mut self, to: SessionExecutionState) -> bool;
        fn high_water(&self) -> u64;
    }

    impl<Members: ActivityMembers> ReducerStep for ActivityState<'_, Members> {
        fn next(&mut self) -> Option<u64> {
            self.begin_activity_epoch()
        }

        fn exact(&mut self, epoch: u64) -> bool {
            self.begin_activity_epoch_at(epoch)
        }

        fn settle(&mut self, epoch: u64) -> Option<bool> {
            self.settle_activity_epoch(epoch)
        }

        fn terminal(&mut self, to: SessionExecutionState) -> bool {
            self.transition_execution(to).is_ok()
        }

        fn high_water(&self) -> u64 {
            *self.epoch
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4096))]

        #[test]
        fn two_slot_model_agrees_with_the_real_btreeset_on_two_admission_traces(
            seed in epoch(),
            requests in [prop::option::of(epoch()), prop::option::of(epoch())],
            running in any::<bool>(),
            settles in prop::collection::vec(settle_target(), 0..=2),
            terminal in prop::option::of(prop_oneof![
                Just(SessionExecutionState::Terminated),
                Just(SessionExecutionState::ActivationFailed),
            ]),
        ) {
            let execution = if running {
                SessionExecutionState::Running
            } else {
                SessionExecutionState::Idle
            };
            let mut pair = Pair::new(seed, execution);
            let mut admitted = [None; 2];
            for (index, request) in requests.into_iter().enumerate() {
                admitted[index] = match request {
                    None => pair.step("R1 Next", |state| state.next()),
                    Some(epoch) => pair
                        .step("R1 Exact", |state| state.exact(epoch))
                        .then_some(epoch),
                };
            }
            for target in settles {
                let epoch = match target {
                    SettleTarget::Admitted(index) => admitted[index].unwrap_or(seed),
                    SettleTarget::HighWater => pair.real_epoch,
                    SettleTarget::Any(epoch) => epoch,
                };
                pair.step("R2 settle", |state| state.settle(epoch));
            }
            if let Some(to) = terminal {
                pair.step("R3 terminal", |state| state.terminal(to));
                prop_assert_eq!(pair.step("R3 high-water", |state| state.high_water()), pair.real_epoch);
            }
        }
    }
}
