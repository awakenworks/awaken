//! Interval openings and budget-reach anchors over committed evidence.
//!
//! Consumers: the timeline's aggregate projection (which Running edge opens an
//! interval, where its usage/idle pair closes) and the canonical order.

use std::collections::{HashMap, HashSet};

use awaken_agent_contract::{RunLifecycleCursor, RunLifecycleEvent, RunLifecycleEventKind};
use awaken_runtime_contract::runs::Pending;
use awaken_session_contract::{
    PersistedSession, SessionRuntimeInterval, SessionRuntimeIntervalObservation,
    SessionRuntimeIntervalStart,
};
use awaken_session_contract::{SessionAgentRunBoundary, SessionEventBatch, SessionEventCommand};

/// The latest committed observation of an interval: where it closed.
#[must_use]
pub fn interval_close_cursor(interval: &SessionRuntimeInterval) -> Option<u64> {
    interval
        .observations
        .iter()
        .map(|observation| observation.source_commit_cursor)
        .max()
}

/// The live and closed forms use one opening selector. Closing an interval
/// adds terminal evidence; it cannot replace its already-issued opening with
/// a later Running commit of the same resumed Run.
#[must_use]
pub fn interval_lifecycle_open_event<'a>(
    session_id: &str,
    interval: &SessionRuntimeInterval,
    prior_close: u64,
    batches: &[SessionEventBatch],
    lifecycle_events: &'a [RunLifecycleEvent],
) -> Option<&'a RunLifecycleEvent> {
    select_interval_opening(
        session_id,
        interval.opened_revision,
        interval.activity_epoch,
        &interval.observations,
        (prior_close, interval_close_cursor(interval)),
        batches,
        lifecycle_events,
    )
}

/// Resolve the exact lifecycle event already proven to open a live Session
/// interval. At the first-lineage cutover, a retained batch is still required
/// so a cold replica never mistakes an older lifecycle for the new opening.
#[must_use]
pub fn running_interval_lifecycle_open_event<'a>(
    session_id: &str,
    interval: &SessionRuntimeIntervalStart,
    prior_close: u64,
    batches: &[SessionEventBatch],
    lifecycle_events: &'a [RunLifecycleEvent],
) -> Option<&'a RunLifecycleEvent> {
    select_interval_opening(
        session_id,
        interval.opened_revision,
        interval.activity_epoch,
        &interval.observations,
        (prior_close, None),
        batches,
        lifecycle_events,
    )
}

/// Whether a lifecycle commit opens (or reopens) Run execution.
#[must_use]
pub fn is_opening(kind: RunLifecycleEventKind) -> bool {
    matches!(
        kind,
        RunLifecycleEventKind::Running
            | RunLifecycleEventKind::Resumed
            | RunLifecycleEventKind::Rescheduled
    )
}

/// The opening that began the execution period `opening` belongs to: the
/// first opening of the same Run on the same Thread after that Run's latest
/// boundary before `opening`, and after `floor`. A `Rescheduled` or repeated
/// `Running` commit continues its period; only an Awaiting or terminal
/// boundary ends one, so a `Resumed` after it begins the next.
///
/// The aggregate opens once per period, at this commit. Selecting the latest
/// opening instead made the aggregate key depend on how much of the period a
/// reader happened to see: a warm reader keyed it at `Running`, a cold one at
/// the later `Rescheduled`, and a warm reader that saw both published it twice.
#[must_use]
pub fn period_opening<'a>(
    opening: &'a RunLifecycleEvent,
    floor: u64,
    lifecycle_events: &'a [RunLifecycleEvent],
) -> &'a RunLifecycleEvent {
    let same_run = |event: &&'a RunLifecycleEvent| {
        event.thread_id == opening.thread_id && event.run_id == opening.run_id
    };
    let last_boundary = lifecycle_events
        .iter()
        .filter(same_run)
        .filter(|event| SessionAgentRunBoundary::is_boundary(event.kind))
        .filter(|event| lifecycle_opening_order(event, &opening).is_lt())
        .max_by(lifecycle_opening_order);
    lifecycle_events
        .iter()
        .filter(same_run)
        .filter(|event| is_opening(event.kind) && event.source_commit_cursor > floor)
        .filter(|event| lifecycle_opening_order(event, &opening).is_le())
        .filter(|event| {
            last_boundary.is_none_or(|boundary| lifecycle_opening_order(event, &boundary).is_gt())
        })
        .min_by(lifecycle_opening_order)
        .unwrap_or(opening)
}

fn select_interval_opening<'a>(
    session_id: &str,
    opened_revision: awaken_session_types::SessionRevision,
    activity_epoch: u64,
    observations: &[SessionRuntimeIntervalObservation],
    (prior_close, close): (u64, Option<u64>),
    batches: &[SessionEventBatch],
    lifecycle_events: &'a [RunLifecycleEvent],
) -> Option<&'a RunLifecycleEvent> {
    select_latest_period_opening(
        session_id,
        opened_revision,
        activity_epoch,
        observations,
        (prior_close, close),
        batches,
        lifecycle_events,
    )
    .map(|opening| period_opening(opening, prior_close, lifecycle_events))
}

fn select_latest_period_opening<'a>(
    session_id: &str,
    opened_revision: awaken_session_types::SessionRevision,
    activity_epoch: u64,
    observations: &[SessionRuntimeIntervalObservation],
    (prior_close, close): (u64, Option<u64>),
    batches: &[SessionEventBatch],
    lifecycle_events: &'a [RunLifecycleEvent],
) -> Option<&'a RunLifecycleEvent> {
    let candidates = lifecycle_events
        .iter()
        .filter(|event| close.is_none_or(|close| event.source_commit_cursor <= close))
        .filter(|event| is_opening(event.kind))
        .collect::<Vec<_>>();
    let owners = interval_owner_openings(opened_revision, batches);
    if !owners.is_empty() {
        return owners
            .into_iter()
            .filter_map(|owner| {
                let matching = candidates
                    .iter()
                    .copied()
                    .filter(|event| owner.matches(session_id, event));
                match owner.anchor {
                    Some(IntervalOwnerAnchor::ReplyTerminal(anchor)) => matching
                        .filter(|event| event.source_commit_cursor <= anchor)
                        .max_by(lifecycle_opening_order),
                    Some(IntervalOwnerAnchor::ReplyAfterAwaiting(anchor)) => matching
                        .filter(|event| event.source_commit_cursor > anchor)
                        .min_by(lifecycle_opening_order),
                    None => matching.min_by(lifecycle_opening_order),
                }
            })
            .min_by(lifecycle_opening_order);
    }
    let matching = candidates.into_iter().filter(|event| {
        event.source_commit_cursor > prior_close
            && (observations.is_empty()
                || observations.iter().any(|observation| {
                    event.thread_id == observation.thread_id
                        && event.run_id == observation.run_id
                        && event.source_commit_cursor <= observation.source_commit_cursor
                }))
    });
    if prior_close != 0 {
        // A retained predecessor closes before this interval. Subsequent
        // Running checkpoints enrich that interval, never move its opening.
        return matching.min_by(lifecycle_opening_order);
    }
    // Pre-cutover first intervals have no retained admission revision. Keep
    // their existing latest matching reopen floor; an unanchored live interval
    // must still withhold its opening instead of selecting a legacy Run.
    let observation = observations
        .iter()
        .find(|observation| observation.activity_epoch == activity_epoch);
    matching
        .filter(|event| {
            observation.is_none_or(|observation| {
                event.thread_id == observation.thread_id && event.run_id == observation.run_id
            })
        })
        .filter(|_| !observations.is_empty())
        .max_by(lifecycle_opening_order)
}

#[derive(Clone, Copy)]
struct IntervalOwnerOpening<'a> {
    run_id: &'a awaken_agent_contract::agent::run::Id,
    target: IntervalOwnerTarget<'a>,
    anchor: Option<IntervalOwnerAnchor>,
}

impl IntervalOwnerOpening<'_> {
    fn matches(&self, session_id: &str, event: &RunLifecycleEvent) -> bool {
        &event.run_id == self.run_id
            && match self.target {
                IntervalOwnerTarget::Primary => event.thread_id.0 == session_id,
                IntervalOwnerTarget::Child(thread_id) => event.thread_id == *thread_id,
            }
    }
}

#[derive(Clone, Copy)]
enum IntervalOwnerTarget<'a> {
    Primary,
    Child(&'a awaken_agent_contract::agent::thread::Id),
}

#[derive(Clone, Copy)]
enum IntervalOwnerAnchor {
    /// A ToolReply projection anchor observes the resumed Run's complete reply
    /// prefix, so its opening is the latest one no later than that fence.
    ReplyTerminal(u64),
    /// Current ToolReply rows freeze the Awaiting commit before delivery; the
    /// resumed interval opens at the first later Running lifecycle commit.
    ReplyAfterAwaiting(u64),
}

fn lifecycle_opening_order(
    left: &&RunLifecycleEvent,
    right: &&RunLifecycleEvent,
) -> std::cmp::Ordering {
    left.source_commit_cursor
        .cmp(&right.source_commit_cursor)
        .then_with(|| left.cursor.cmp(&right.cursor))
}

/// The admission fence of the retained batch that owns an interval: every
/// effect of that batch, the interval's opening included, commits after it.
/// `None` for an interval no retained batch owns (a Run admitted without a
/// user Event).
#[must_use]
pub(crate) fn interval_admission_fence(
    opened_revision: awaken_session_types::SessionRevision,
    batches: &[SessionEventBatch],
) -> Option<u64> {
    let owning_revision = batches
        .iter()
        .filter(|batch| batch.admitted_revision <= opened_revision)
        .map(|batch| batch.admitted_revision)
        .max()?;
    batches
        .iter()
        .filter(|batch| batch.admitted_revision == owning_revision)
        .filter_map(|batch| batch.admission_projection_anchor)
        .map(|anchor| anchor.source_commit_cursor)
        .max()
}

fn interval_owner_openings(
    opened_revision: awaken_session_types::SessionRevision,
    batches: &[SessionEventBatch],
) -> Vec<IntervalOwnerOpening<'_>> {
    let Some(owning_revision) = batches
        .iter()
        .filter(|batch| batch.admitted_revision <= opened_revision)
        .map(|batch| batch.admitted_revision)
        .max()
    else {
        return Vec::new();
    };
    batches
        .iter()
        .filter(|batch| batch.admitted_revision == owning_revision)
        .flat_map(|batch| &batch.events)
        .filter_map(|entry| match &entry.event {
            SessionEventCommand::UserMessage { run_id, .. } => Some(IntervalOwnerOpening {
                run_id,
                target: IntervalOwnerTarget::Primary,
                // This command owns a unique Run, so its first lifecycle
                // opening is authoritative. The public observation may be an
                // input commit OR a fast Run's post-Dispatch terminal fallback;
                // neither is another execution-start fence. In particular a
                // terminal observation must not hide an earlier real opening.
                anchor: None,
            }),
            SessionEventCommand::ToolReply { reply, .. } => Some(IntervalOwnerOpening {
                run_id: &reply.expected_run_id,
                target: match &reply.target {
                    awaken_session_contract::SessionThreadTarget::Primary => {
                        IntervalOwnerTarget::Primary
                    }
                    awaken_session_contract::SessionThreadTarget::Child(thread_id) => {
                        IntervalOwnerTarget::Child(thread_id)
                    }
                },
                anchor: reply
                    .answered_pending_commit_cursor
                    .map(IntervalOwnerAnchor::ReplyAfterAwaiting)
                    .or_else(|| {
                        entry.projection_anchor.map(|anchor| {
                            IntervalOwnerAnchor::ReplyTerminal(anchor.source_commit_cursor)
                        })
                    }),
            }),
            _ => None,
        })
        .collect()
}

/// Find the first append-only interval whose cumulative usage reached this
/// budget generation. A still-open crossing has no immutable public coordinate
/// and remains withheld until the owning interval closes.
#[must_use]
pub fn budget_reach_close_cursor(
    persisted: &PersistedSession,
    transition: &awaken_session_contract::BudgetReachTransition,
) -> Option<u64> {
    persisted
        .closed_runtime_intervals
        .iter()
        .find(|interval| interval.usage.is_at_least(&transition.usage_cursor))
        .and_then(interval_close_cursor)
}

/// Anchor a durable budget transition to the public pause which actually
/// exposes it. Tool approval has priority over an already-reached budget, so
/// the crossing interval can close at `requires_action` and a later interval
/// closes at `budget_reached` without consuming more tokens. In that case the
/// latter boundary owns the one public Usage/Idle pair. Runs which terminate
/// without an explicit budget Awaiting fact retain the crossing fallback.
#[must_use]
pub fn budget_reach_projection_close_cursor(
    persisted: &PersistedSession,
    transition: &awaken_session_contract::BudgetReachTransition,
    lifecycle_events: &[RunLifecycleEvent],
    historical_pending: &HashMap<RunLifecycleCursor, Pending>,
) -> Option<u64> {
    let crossing = budget_reach_close_cursor(persisted, transition)?;
    let explicit_budget_pause = persisted
        .closed_runtime_intervals
        .iter()
        .filter(|interval| {
            interval_close_cursor(interval).is_some_and(|close| close >= crossing)
                && interval.usage.is_at_least(&transition.usage_cursor)
        })
        .find(|interval| {
            interval.observations.iter().rev().any(|observation| {
                lifecycle_events.iter().any(|event| {
                    event.cursor == observation.lifecycle_cursor
                        && event.thread_id == observation.thread_id
                        && event.run_id == observation.run_id
                        && event.await_reason.as_ref()
                            == Some(
                                &awaken_agent_contract::agent::awaiting::AwaitReason::BudgetReached,
                            )
                })
            })
        })
        .and_then(interval_close_cursor);
    if explicit_budget_pause.is_some() {
        return explicit_budget_pause;
    }
    // An answerable wait at the crossing close owns requires_action. Its
    // ticket can be consumed long before a later cold rebuild, so use the
    // committed lifecycle and exact historical targets as of this cursor,
    // never the current ticket. A later explicit BudgetReached boundary above
    // takes over once it actually commits.
    let mut answerable = HashSet::new();
    for event in lifecycle_events
        .iter()
        .filter(|event| event.source_commit_cursor <= crossing)
    {
        if is_opening(event.kind) {
            answerable.remove(&event.thread_id);
        } else if SessionAgentRunBoundary::is_boundary(event.kind) {
            if event.kind == RunLifecycleEventKind::Awaiting
                && historical_pending.contains_key(&event.cursor)
            {
                answerable.insert(&event.thread_id);
            } else {
                answerable.remove(&event.thread_id);
            }
        }
    }
    answerable.is_empty().then_some(crossing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::content::ContentBlock;

    #[test]
    fn user_observation_cannot_move_the_exact_run_opening() {
        // Decision table: U1 no observation; U2 input observed at opening; U3 first
        // observation is the later terminal fallback; U4 still later receipt. For
        // every U1-U4, a present exact opening wins unchanged, absent opening stays
        // absent. Foreign Run/Thread, a same-Run later checkpoint, array order and
        // a future resumed interval cannot create or replace that opening. This
        // exercises the production collection selector, not a scalar reimplementation.
        use awaken_agent_contract::agent::run::{Id as RunId, RunState};
        use awaken_agent_contract::agent::thread::Id as ThreadId;
        use awaken_session_contract::{SessionEventInput, SessionEventProjectionAnchor};
        use awaken_session_types::SessionRevision;
        let mut batch = SessionEventBatch::compile(
            &awaken_session_types::SessionId::from("session"),
            "batch",
            vec![SessionEventInput::UserMessage {
                content: vec![ContentBlock::text("explain")],
            }],
        )
        .unwrap();
        batch.admitted_revision = SessionRevision(1);
        let SessionEventCommand::UserMessage { run_id, .. } = &batch.events[0].event else {
            unreachable!()
        };
        let run_id = run_id.clone();
        let event = |cursor, thread: &str, run| RunLifecycleEvent {
            cursor: awaken_agent_contract::encode_run_lifecycle_cursor(cursor, 0).unwrap(),
            source_commit_cursor: cursor,
            thread_id: ThreadId(thread.into()),
            run_id: run,
            kind: RunLifecycleEventKind::Running,
            state: RunState::Running,
            await_reason: None,
        };
        for anchor in [None, Some(1), Some(3), Some(9)] {
            batch.events[0].projection_anchor =
                anchor.map(|source_commit_cursor| SessionEventProjectionAnchor {
                    source_commit_cursor,
                });
            for present in [false, true] {
                let mut events = vec![
                    event(4, "session", run_id.clone()),
                    event(1, "foreign", run_id.clone()),
                    event(1, "session", RunId("foreign".into())),
                ];
                if present {
                    events.extend([
                        event(2, "session", run_id.clone()),
                        event(1, "session", run_id.clone()),
                    ]);
                }
                for _ in 0..2 {
                    let selected = select_interval_opening(
                        "session",
                        SessionRevision(1),
                        1,
                        &[],
                        (0, Some(3)),
                        std::slice::from_ref(&batch),
                        &events,
                    );
                    assert_eq!(
                        selected.map(|event| event.source_commit_cursor),
                        present.then_some(1),
                        "anchor={anchor:?}, present={present}"
                    );
                    events.reverse();
                }
            }
        }
    }

    #[test]
    fn period_opening_is_the_first_opening_since_the_runs_last_boundary() {
        // Decision table over one Run's lifecycle, starting from its latest
        // opening. P1 a lone Running is its own period; P2 a Rescheduled
        // continues the Running period; P3 an Awaiting boundary ends a period,
        // so the Resumed after it (and its Rescheduled) open the next; P4 the
        // floor excludes a predecessor interval's commits; P5 another Run or
        // Thread never extends the period; P6 array order is irrelevant.
        use awaken_agent_contract::agent::run::{Id as RunId, RunState};
        use awaken_agent_contract::agent::thread::Id as ThreadId;
        let event = |cursor, thread: &str, run: &str, kind| RunLifecycleEvent {
            cursor: awaken_agent_contract::encode_run_lifecycle_cursor(cursor, 0).unwrap(),
            source_commit_cursor: cursor,
            thread_id: ThreadId(thread.into()),
            run_id: RunId(run.into()),
            kind,
            state: RunState::Running,
            await_reason: None,
        };
        use RunLifecycleEventKind::{Awaiting, Rescheduled, Resumed, Running};
        let cases: [(&str, Vec<RunLifecycleEvent>, u64, u64, u64); 6] = [
            ("P1", vec![event(1, "s", "r", Running)], 0, 1, 1),
            (
                "P2",
                vec![event(1, "s", "r", Running), event(2, "s", "r", Rescheduled)],
                0,
                2,
                1,
            ),
            (
                "P3",
                vec![
                    event(1, "s", "r", Running),
                    event(2, "s", "r", Awaiting),
                    event(3, "s", "r", Resumed),
                    event(4, "s", "r", Rescheduled),
                ],
                0,
                4,
                3,
            ),
            (
                "P4",
                vec![event(1, "s", "r", Running), event(2, "s", "r", Rescheduled)],
                1,
                2,
                2,
            ),
            (
                "P5",
                vec![
                    event(1, "s", "other", Running),
                    event(1, "child", "r", Running),
                    event(2, "s", "r", Running),
                    event(3, "s", "r", Rescheduled),
                ],
                0,
                3,
                2,
            ),
            (
                "P6",
                vec![
                    event(4, "s", "r", Rescheduled),
                    event(3, "s", "r", Resumed),
                    event(2, "s", "r", Awaiting),
                    event(1, "s", "r", Running),
                ],
                0,
                4,
                3,
            ),
        ];
        for (rule, events, floor, from, expected) in cases {
            let latest = events
                .iter()
                .find(|event| event.source_commit_cursor == from && event.thread_id.0 == "s")
                .unwrap();
            assert_eq!(
                period_opening(latest, floor, &events).source_commit_cursor,
                expected,
                "{rule}"
            );
        }
    }
}
