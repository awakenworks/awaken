//! Interval openings and budget-reach anchors over committed evidence.
//!
//! Consumers: the timeline's aggregate projection (which Running edge opens an
//! interval, where its usage/idle pair closes) and the canonical order.

use awaken_agent_contract::{RunLifecycleEvent, RunLifecycleEventKind};
use awaken_session_event_contract::{SessionEventBatch, SessionEventCommand};
use awaken_session_state_contract::{
    PersistedSession, SessionRuntimeInterval, SessionRuntimeIntervalObservation,
    SessionRuntimeIntervalStart,
};

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

fn select_interval_opening<'a>(
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
                    awaken_session_execution_contract::SessionThreadTarget::Primary => {
                        IntervalOwnerTarget::Primary
                    }
                    awaken_session_execution_contract::SessionThreadTarget::Child(thread_id) => {
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
    transition: &awaken_session_budget_contract::BudgetReachTransition,
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
    transition: &awaken_session_budget_contract::BudgetReachTransition,
    lifecycle_events: &[RunLifecycleEvent],
) -> Option<u64> {
    let crossing = budget_reach_close_cursor(persisted, transition)?;
    persisted
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
        .and_then(interval_close_cursor)
        .or(Some(crossing))
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
        use awaken_session_event_contract::{SessionEventInput, SessionEventProjectionAnchor};
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
}
