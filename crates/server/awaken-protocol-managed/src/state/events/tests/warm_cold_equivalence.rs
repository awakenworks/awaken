//! P1, schedule independence: every refresh schedule of a committed history
//! publishes what a cold replica builds from the same prefix.
//!
//! Each scenario enumerates every subset of its commit boundaries as the
//! points where the warm replica refreshed (exhaustive for at most 8 steps),
//! and a property test draws longer histories with one sampled schedule each.
//! A history that still diverges is listed with the timeline class that makes
//! it diverge and asserted to diverge; fixing the class fails that assertion
//! until the scenario pins its sequence and leaves the list.
//!
//! Consumer: the timeline's schedule-independence guarantee.

use super::timeline_scenarios::*;

/// What a scenario's every-schedule check must produce.
enum Expect {
    /// Every schedule equals its cold rebuild and ends on this sequence.
    Sequence(&'static [&'static str]),
    /// The history still diverges through this timeline class.
    KnownDivergence(&'static str),
}

async fn check<T: Timeline>(name: &str, steps: &[T::Step], expect: Expect) {
    let result = every_schedule_equivalent::<T>(steps).await;
    match expect {
        Expect::Sequence(expected) => assert_eq!(
            result,
            Ok(expected
                .iter()
                .map(|kind| (*kind).to_string())
                .collect::<Vec<_>>()),
            "{name}"
        ),
        Expect::KnownDivergence(class) => assert!(
            result.is_err(),
            "{name} no longer diverges (class {class}): pin its sequence {result:?} and remove \
             it from the known divergences"
        ),
    }
}

macro_rules! scenario {
    ($name:ident, $timeline:ty, $steps:expr, $expect:expr $(,)?) => {
        #[tokio::test]
        async fn $name() {
            check::<$timeline>(stringify!($name), $steps, $expect).await;
        }
    };
}

const RUN_WITH_EXECUTED_TOOL: [&str; 10] = [
    "user.message",
    "session.status_running",
    "session.thread_status_running",
    "agent.message",
    "agent.tool_use",
    "agent.tool_result",
    "agent.message",
    "session.thread_status_idle",
    "session.usage",
    "session.status_idle",
];

const RESCHEDULED_RUN_SEQUENCE: [&str; 9] = [
    "session.status_running",
    "session.thread_status_running",
    "session.status_rescheduled",
    "session.thread_status_rescheduled",
    "session.thread_status_running",
    "agent.message",
    "session.thread_status_idle",
    "session.usage",
    "session.status_idle",
];

// C1 Run-port input without Managed admission; C2 all eight refresh schedules;
// C3 fresh replica. E1 one stable customer input before assistant output;
// E2 complete history and terminal outcome are independent of the schedule.
scenario!(
    external_customer_input_survives_every_refresh_schedule_and_cold_rebuild,
    RootTimeline,
    EXTERNAL_CUSTOMER_INPUT,
    Expect::Sequence(&[
        "session.status_running",
        "session.thread_status_running",
        "user.message",
        "agent.message",
        "agent.message",
        "session.thread_status_idle",
        "session.usage",
        "session.status_idle",
    ]),
);

// One Run with an executed tool (16 schedules).
scenario!(
    one_run,
    RootTimeline,
    ONE_RUN,
    Expect::Sequence(&RUN_WITH_EXECUTED_TOOL),
);

// Two consecutive admitted intervals; a refresh may see the first closed and
// the second opened in one read (128 schedules).
scenario!(
    consecutive_runs,
    RootTimeline,
    CONSECUTIVE_RUNS,
    Expect::Sequence(&[
        "user.message",
        "session.status_running",
        "session.thread_status_running",
        "agent.message",
        "agent.tool_use",
        "agent.tool_result",
        "agent.message",
        "session.thread_status_idle",
        "session.usage",
        "session.status_idle",
        "user.message",
        "session.status_running",
        "session.thread_status_running",
        "agent.message",
        "agent.message",
        "session.thread_status_idle",
        "session.usage",
        "session.status_idle",
    ]),
);

// A permission pause whose ticket is audited: the cold replica rebuilds the
// ask and the Thread pause from the audited target alone.
scenario!(
    audited_permission_pause,
    RootTimeline,
    AUDITED_PERMISSION_PAUSE,
    Expect::Sequence(&[
        "user.message",
        "session.status_running",
        "session.thread_status_running",
        "agent.tool_use",
        "session.thread_status_idle",
        "session.thread_status_running",
        "agent.tool_result",
        "agent.message",
        "session.thread_status_idle",
        "session.usage",
        "session.status_idle",
    ]),
);

// A Run with no retained user Event reschedules inside its one interval.
scenario!(
    rescheduled_run,
    RootTimeline,
    RESCHEDULED_RUN,
    Expect::Sequence(&RESCHEDULED_RUN_SEQUENCE),
);

// The same, still open at the final read.
scenario!(
    open_rescheduled_interval,
    RootTimeline,
    OPEN_RESCHEDULED_INTERVAL,
    Expect::Sequence(&[
        "session.status_running",
        "session.thread_status_running",
        "session.status_rescheduled",
        "session.thread_status_rescheduled",
        "session.thread_status_running",
    ]),
);

// A user-Event Run observed only while its interval is open.
scenario!(
    open_user_event_interval,
    RootTimeline,
    OPEN_USER_EVENT_INTERVAL,
    Expect::Sequence(&[
        "user.message",
        "session.status_running",
        "session.thread_status_running",
        "agent.message",
        "agent.tool_use",
        "agent.tool_result",
    ]),
);

// The second of two intervals reschedules: two opening commits after the
// first interval's close.
scenario!(
    rescheduled_second_interval,
    RootTimeline,
    RESCHEDULED_SECOND_INTERVAL,
    Expect::Sequence(&[
        "session.status_running",
        "session.thread_status_running",
        "agent.message",
        "session.thread_status_idle",
        "session.usage",
        "session.status_idle",
        "session.status_running",
        "session.thread_status_running",
        "session.status_rescheduled",
        "session.thread_status_rescheduled",
        "session.thread_status_running",
        "agent.message",
        "session.thread_status_idle",
        "session.usage",
        "session.status_idle",
    ]),
);

// One commit holds a response whose tool call is its twelfth block, the
// call's result and another response.
scenario!(
    one_commit_of_several_messages,
    RootTimeline,
    ONE_COMMIT_OF_SEVERAL_MESSAGES,
    Expect::Sequence(&[
        "user.message",
        "session.status_running",
        "session.thread_status_running",
        "agent.thinking",
        "agent.tool_use",
        "agent.tool_result",
        "agent.message",
        "agent.message",
        "session.thread_status_idle",
        "session.usage",
        "session.status_idle",
    ]),
);

// A Run without Session activity closes; with one boundary, the aggregate
// pair keyed by that boundary is the same for every schedule.
scenario!(
    run_without_activity,
    RootTimeline,
    RUN_WITHOUT_ACTIVITY,
    Expect::Sequence(&[
        "session.status_running",
        "session.thread_status_running",
        "agent.message",
        "agent.message",
        "session.thread_status_idle",
        "session.usage",
        "session.status_idle",
    ]),
);

// C (latent bug): a Run without Session activity pauses and resumes. A reader
// that saw the pause and the resume in one read keyed the aggregate opening of
// the pause's period at the resume, the Run's latest opening, not bounded by
// the pause that ends the period, so the opening followed that period's idle.
// Each aggregate period opens at its own first opening.
scenario!(
    pause_then_resume_without_activity,
    RootTimeline,
    &[Step::Legacy(0), Step::Ask(0), Step::Reply(0)],
    Expect::Sequence(&[
        "session.status_running",
        "session.thread_status_running",
        "agent.tool_use",
        "session.thread_status_idle",
        "session.usage",
        "session.status_idle",
        "session.status_running",
        "session.thread_status_running",
        "agent.tool_result",
    ]),
);

// A/C: two Runs without Session activity; a cold replica that first reads
// after both sees two openings and two boundaries in one prefix, and opens
// and closes the aggregate once per Run as a replica that read each did.
scenario!(
    consecutive_runs_without_activity,
    RootTimeline,
    CONSECUTIVE_RUNS_WITHOUT_ACTIVITY,
    Expect::Sequence(&[
        "session.status_running",
        "session.thread_status_running",
        "agent.message",
        "session.thread_status_idle",
        "session.usage",
        "session.status_idle",
        "session.status_running",
        "session.thread_status_running",
        "agent.message",
        "agent.message",
        "session.thread_status_idle",
        "session.usage",
        "session.status_idle",
    ]),
);

// D: the ticket is set and consumed, but its Awaiting commit left no audit.
scenario!(
    unaudited_permission_pause,
    RootTimeline,
    UNAUDITED_PERMISSION_PAUSE,
    Expect::KnownDivergence("D"),
);

// F: a scheduled action pauses the Run while its Session activity stays
// admitted, then resumes autonomously and completes. The interval's span
// starts after the Runtime cursor its admission recorded, so every reader
// holds the pause inside it and the aggregate stays open through it.
scenario!(
    scheduled_autonomous_resume,
    RootTimeline,
    SCHEDULED_AUTONOMOUS_RESUME,
    Expect::Sequence(&[
        "session.status_running",
        "session.thread_status_running",
        "agent.tool_use",
        "session.thread_status_idle",
        "session.thread_status_running",
        "agent.tool_result",
        "agent.message",
        "session.thread_status_idle",
        "session.usage",
        "session.status_idle",
    ]),
);

// C: one Run without Session activity reaches three boundaries; each
// aggregate period opens at its own first opening.
scenario!(
    run_terminating_several_times,
    RootTimeline,
    RUN_TERMINATING_SEVERAL_TIMES,
    Expect::Sequence(&[
        "session.status_running",
        "session.thread_status_running",
        "agent.tool_use",
        "session.thread_status_idle",
        "session.usage",
        "session.status_idle",
        "session.status_running",
        "session.thread_status_running",
        "agent.tool_result",
        "agent.tool_use",
        "session.thread_status_idle",
        "session.usage",
        "session.status_idle",
        "session.status_running",
        "session.thread_status_running",
        "agent.tool_result",
        "agent.message",
        "session.thread_status_idle",
        "session.usage",
        "session.status_idle",
    ]),
);

// E: the Run pauses, then is cancelled.
scenario!(
    cancelled_pause,
    RootTimeline,
    CANCELLED_PAUSE,
    Expect::KnownDivergence("E"),
);

// E: as above for a Run without Session activity.
scenario!(
    cancelled_pause_without_activity,
    RootTimeline,
    CANCELLED_PAUSE_WITHOUT_ACTIVITY,
    Expect::KnownDivergence("E"),
);

// B: a child pauses on an audited ticket, is answered and completes. The
// pause and its aggregate close come from the pause's own audit. C: the
// aggregate opens at the root's opening and again at the child's resumption
// after the pause closed it (it used to open only where a root terminal's Run
// opened, so a child-driven period closed without having opened).
scenario!(
    child_audited_pause,
    ChildTimeline,
    CHILD_AUDITED_PAUSE,
    Expect::Sequence(&[
        "session.status_running",
        "agent.tool_use",
        "agent.tool_result",
        "session.thread_created",
        "session.thread_status_running",
        "agent.thread_message_sent",
        "agent.tool_use",
        "session.thread_status_idle",
        "session.usage",
        "session.status_idle",
        "session.status_running",
        "session.thread_status_running",
        "session.thread_status_idle",
        "session.usage",
        "session.status_idle",
    ]),
);

// B/E: the same pause left no audit of its target.
scenario!(
    child_unaudited_pause,
    ChildTimeline,
    CHILD_UNAUDITED_PAUSE,
    Expect::KnownDivergence("B/E"),
);

// B/E: the child pauses twice before completing; every pause and every
// resumption is published once. C: each resumption reopens the aggregate.
scenario!(
    child_pausing_twice,
    ChildTimeline,
    CHILD_PAUSING_TWICE,
    Expect::Sequence(&[
        "session.status_running",
        "agent.tool_use",
        "agent.tool_result",
        "session.thread_created",
        "session.thread_status_running",
        "agent.thread_message_sent",
        "agent.tool_use",
        "session.thread_status_idle",
        "session.usage",
        "session.status_idle",
        "session.status_running",
        "session.thread_status_running",
        "agent.tool_use",
        "session.thread_status_idle",
        "session.usage",
        "session.status_idle",
        "session.status_running",
        "session.thread_status_running",
        "session.thread_status_idle",
        "session.usage",
        "session.status_idle",
    ]),
);

// A/E: the paused child is cancelled; the aggregate closes again at the
// cancellation. C: the aggregate opens at the root's opening.
scenario!(
    child_cancelled_pause,
    ChildTimeline,
    CHILD_CANCELLED_PAUSE,
    Expect::Sequence(&[
        "session.status_running",
        "agent.tool_use",
        "agent.tool_result",
        "session.thread_created",
        "session.thread_status_running",
        "agent.thread_message_sent",
        "agent.tool_use",
        "session.thread_status_idle",
        "session.usage",
        "session.status_idle",
        "session.thread_status_idle",
        "session.usage",
        "session.status_idle",
    ]),
);

/// Step kinds whose histories may still diverge; a drawn history that
/// diverges without one of them fails the property. The list only shrinks.
const KNOWN_DIVERGENT_KINDS: &[&str] = &["AskUnaudited", "Cancel"];

fn kind(step: Step) -> &'static str {
    match step {
        Step::Input(_) => "Input",
        Step::Begin(_) => "Begin",
        Step::ExternalInput(_) => "ExternalInput",
        Step::Legacy(_) => "Legacy",
        Step::Say(_) => "Say",
        Step::Tool(_) => "Tool",
        Step::Burst(_) => "Burst",
        Step::Ask(_) => "Ask",
        Step::AskUnaudited(_) => "AskUnaudited",
        Step::Schedule(_) => "Schedule",
        Step::Reply(_) => "Reply",
        Step::Reschedule(_) => "Reschedule",
        Step::Cancel(_) => "Cancel",
        Step::Close(_) => "Close",
    }
}

/// One Run's commits: a start, a body, and an end (the last Run may stay
/// open).
fn run_strategy(index: usize, last: bool) -> impl proptest::strategy::Strategy<Value = Vec<Step>> {
    use proptest::prelude::*;
    let start = prop_oneof![
        Just(Step::Input(index)),
        Just(Step::Begin(index)),
        Just(Step::Legacy(index)),
    ];
    let body = proptest::collection::vec(
        prop_oneof![
            Just(vec![Step::Say(index)]),
            Just(vec![Step::Tool(index)]),
            Just(vec![Step::Burst(index)]),
            Just(vec![Step::Reschedule(index)]),
            Just(vec![Step::Ask(index), Step::Reply(index)]),
            Just(vec![Step::AskUnaudited(index), Step::Reply(index)]),
            Just(vec![Step::Schedule(index), Step::Reply(index)]),
        ],
        0..3,
    );
    let end = prop_oneof![
        Just(vec![Step::Close(index)]),
        Just(vec![Step::Ask(index), Step::Cancel(index)]),
        Just(vec![Step::AskUnaudited(index), Step::Cancel(index)]),
        Just(if last {
            Vec::new()
        } else {
            vec![Step::Close(index)]
        }),
    ];
    (start, body, end).prop_map(|(start, body, end)| {
        std::iter::once(start)
            .chain(body.into_iter().flatten())
            .chain(end)
            .collect()
    })
}

fn history_strategy() -> impl proptest::strategy::Strategy<Value = (Vec<Step>, u64)> {
    use proptest::prelude::*;
    (run_strategy(0, false), run_strategy(1, true), any::<u64>()).prop_filter_map(
        "histories hold at most 12 commits",
        |(first, second, schedule)| {
            let steps = first.into_iter().chain(second).collect::<Vec<_>>();
            (steps.len() <= 12).then_some((steps, schedule))
        },
    )
}

#[test]
fn sampled_long_histories_equal_their_cold_rebuild_under_a_sampled_schedule() {
    // Causes: C1 two Runs of up to 12 commits drawn from every step kind; C2
    // one sampled refresh schedule. Effects: E1 warm equals cold at every
    // refreshed point and at the end; E2 a divergence is allowed only when
    // the history contains a step kind still listed as known divergent.
    // Shrinking reports the smallest history and schedule that break E1/E2.
    use proptest::test_runner::{Config, RngSeed, TestRunner};
    let mut runner = TestRunner::new(Config {
        cases: 256,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(0x7_1be_11e),
        ..Config::default()
    });
    runner
        .run(&history_strategy(), |(steps, schedule)| {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(warm_equals_cold::<RootTimeline>(&steps, schedule));
            if let Err(divergence) = result {
                proptest::prop_assert!(
                    steps
                        .iter()
                        .any(|step| KNOWN_DIVERGENT_KINDS.contains(&kind(*step))),
                    "{steps:?}: {divergence}"
                );
            }
            Ok(())
        })
        .unwrap();
}
