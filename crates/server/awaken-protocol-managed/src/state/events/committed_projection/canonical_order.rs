//! Deterministic coordinates for the disposable Managed committed-event vector.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct CanonicalEventOrder {
    pub(super) source_commit_cursor: u64,
    pub(super) phase: u16,
    pub(super) ordinal: usize,
}

/// Compare one Thread's committed recovery prefix while ignoring the
/// backend-wide diagnostic high-water, which unrelated Threads may advance.
pub(super) fn same_thread_projection_prefix(
    left: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    right: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
) -> bool {
    let mut left = left.cloned();
    let mut right = right.cloned();
    if let Some(snapshot) = &mut left {
        snapshot.store_cursor = 0;
    }
    if let Some(snapshot) = &mut right {
        snapshot.store_cursor = 0;
    }
    left == right
}

pub(super) fn index_lifecycle_orders(
    orders: &mut std::collections::HashMap<String, CanonicalEventOrder>,
    session_id: &str,
    lifecycle_events: &[RunLifecycleEvent],
) {
    let roles = [
        ("root-lifecycle-status-running", 20),
        ("root-lifecycle-session-status-rescheduled", 20),
        ("root-lifecycle-status-rescheduled", 21),
        ("root-rescheduled-lifecycle-status-running", 22),
        ("root-terminal-status-running", 20),
        ("lifecycle-status-running", 20),
        ("lifecycle-status-rescheduled", 21),
        ("rescheduled-lifecycle-status-running", 22),
        ("root-lifecycle-error", 70),
        ("lifecycle-error", 70),
        ("root-lifecycle-status-idle", 80),
        ("lifecycle-status-idle", 80),
        ("failed-lifecycle-status-terminated", 80),
        ("advisor-lifecycle-status-terminated", 81),
    ];
    for lifecycle in lifecycle_events {
        let order = |phase| CanonicalEventOrder {
            source_commit_cursor: lifecycle.source_commit_cursor,
            phase,
            ordinal: 0,
        };
        if matches!(
            lifecycle.kind,
            RunLifecycleEventKind::Running
                | RunLifecycleEventKind::Resumed
                | RunLifecycleEventKind::Rescheduled
        ) {
            retain_earliest_order(
                orders,
                managed_multiagent_event_id(
                    session_id,
                    session_id,
                    "aggregate-status-running",
                    ManagedMultiagentEventProvenance::Lifecycle {
                        cursor: lifecycle.cursor.0,
                    },
                ),
                order(10),
            );
        }
        for (role, phase) in roles {
            retain_earliest_order(
                orders,
                managed_multiagent_event_id(
                    session_id,
                    &lifecycle.thread_id.0,
                    role,
                    ManagedMultiagentEventProvenance::Lifecycle {
                        cursor: lifecycle.cursor.0,
                    },
                ),
                order(phase),
            );
        }
        for (role, phase) in [("aggregate-usage", 90), ("aggregate-status-idle", 100)] {
            retain_earliest_order(
                orders,
                managed_multiagent_event_id(
                    session_id,
                    session_id,
                    role,
                    ManagedMultiagentEventProvenance::LifecyclePrefix {
                        cursor: lifecycle.cursor.0,
                    },
                ),
                order(phase),
            );
        }
    }
}

pub(super) fn retain_earliest_order(
    orders: &mut std::collections::HashMap<String, CanonicalEventOrder>,
    id: String,
    order: CanonicalEventOrder,
) {
    orders
        .entry(id)
        .and_modify(|current| *current = (*current).min(order))
        .or_insert(order);
}

pub(super) fn interval_close_cursor(
    interval: &awaken_session_contract::SessionRuntimeInterval,
) -> Option<u64> {
    interval
        .observations
        .iter()
        .map(|observation| observation.source_commit_cursor)
        .max()
}

/// Resolve the opening already fixed when each observed Run crossed its
/// terminal boundary. Selecting the latest opening for that exact Run avoids
/// reaching back into a legacy interval when the same Run later resumes.
/// Resolve the exact lifecycle event that opened one closed Session interval.
/// Its opaque lifecycle cursor owns the public Running identity, while the
/// source commit cursor owns canonical ordering.
pub(super) fn interval_lifecycle_open_event<'a>(
    session_id: &str,
    interval: &awaken_session_contract::SessionRuntimeInterval,
    prior_close: u64,
    batches: &[awaken_session_contract::SessionEventBatch],
    lifecycle_events: &'a [RunLifecycleEvent],
) -> Option<&'a RunLifecycleEvent> {
    let owners = interval_owner_openings(interval.opened_revision, batches);
    let candidates = lifecycle_events
        .iter()
        .filter(|event| {
            matches!(
                event.kind,
                RunLifecycleEventKind::Running
                    | RunLifecycleEventKind::Resumed
                    | RunLifecycleEventKind::Rescheduled
            ) && if interval.observations.is_empty() {
                owners.iter().any(|owner| &event.run_id == owner.run_id)
            } else {
                interval.observations.iter().any(|observation| {
                    event.thread_id == observation.thread_id
                        && event.run_id == observation.run_id
                        && event.source_commit_cursor > prior_close
                        && event.source_commit_cursor <= observation.source_commit_cursor
                })
            }
        })
        .collect::<Vec<_>>();
    if let Some(observation) = interval
        .observations
        .iter()
        .find(|observation| observation.activity_epoch == interval.activity_epoch)
    {
        return candidates
            .into_iter()
            .filter(|event| {
                event.thread_id == observation.thread_id && event.run_id == observation.run_id
            })
            .max_by(lifecycle_opening_order);
    }

    if prior_close != 0 {
        return candidates.into_iter().min_by(|left, right| {
            left.source_commit_cursor
                .cmp(&right.source_commit_cursor)
                .then_with(|| left.cursor.cmp(&right.cursor))
        });
    }

    if owners.is_empty() {
        // Pre-cutover first intervals have no retained admission revision. Their
        // latest matching reopen is the only safe boundary; choosing an older
        // legacy Running would rewrite an already-issued prefix.
        return candidates.into_iter().max_by(|left, right| {
            left.source_commit_cursor
                .cmp(&right.source_commit_cursor)
                .then_with(|| left.cursor.cmp(&right.cursor))
        });
    }
    owners
        .into_iter()
        .filter_map(|owner| {
            let matching = candidates
                .iter()
                .copied()
                .filter(|event| owner.matches(session_id, event));
            match owner.anchor {
                Some(IntervalOwnerAnchor::Input(anchor)) => matching
                    .filter(|event| event.source_commit_cursor >= anchor)
                    .min_by(lifecycle_opening_order),
                Some(IntervalOwnerAnchor::ReplyTerminal(anchor)) => matching
                    .filter(|event| event.source_commit_cursor <= anchor)
                    .max_by(lifecycle_opening_order),
                Some(IntervalOwnerAnchor::ReplyAfterAwaiting(anchor)) => matching
                    .filter(|event| event.source_commit_cursor > anchor)
                    .min_by(lifecycle_opening_order),
                None => matching.min_by(lifecycle_opening_order),
            }
        })
        .min_by(|left, right| {
            left.source_commit_cursor
                .cmp(&right.source_commit_cursor)
                .then_with(|| left.cursor.cmp(&right.cursor))
        })
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
    /// The retained User input is committed before or with its Run opening.
    Input(u64),
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
    opened_revision: awaken_session_contract::SessionRevision,
    batches: &[awaken_session_contract::SessionEventBatch],
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
                anchor: entry
                    .projection_anchor
                    .map(|anchor| IntervalOwnerAnchor::Input(anchor.source_commit_cursor)),
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

/// Resolve the exact lifecycle event already proven to open a live Session
/// interval. At the first-lineage cutover, a retained batch is still required
/// so a cold replica never mistakes an older lifecycle for the new opening.
pub(super) fn running_interval_lifecycle_open_event<'a>(
    session_id: &str,
    interval: &awaken_session_contract::SessionRuntimeIntervalStart,
    prior_close: u64,
    batches: &[awaken_session_contract::SessionEventBatch],
    lifecycle_events: &'a [RunLifecycleEvent],
) -> Option<&'a RunLifecycleEvent> {
    let is_opening = |event: &&RunLifecycleEvent| {
        matches!(
            event.kind,
            RunLifecycleEventKind::Running
                | RunLifecycleEventKind::Resumed
                | RunLifecycleEventKind::Rescheduled
        )
    };
    if let Some(observation) = interval
        .observations
        .iter()
        .find(|observation| observation.activity_epoch == interval.activity_epoch)
    {
        return lifecycle_events
            .iter()
            .filter(is_opening)
            .filter(|event| event.source_commit_cursor > prior_close)
            .filter(|event| {
                event.thread_id == observation.thread_id && event.run_id == observation.run_id
            })
            .filter(|event| event.source_commit_cursor <= observation.source_commit_cursor)
            .max_by(lifecycle_opening_order);
    }
    if prior_close != 0 {
        return lifecycle_events
            .iter()
            .filter(is_opening)
            .filter(|event| event.source_commit_cursor > prior_close)
            .min_by(|left, right| {
                left.source_commit_cursor
                    .cmp(&right.source_commit_cursor)
                    .then_with(|| left.cursor.cmp(&right.cursor))
            });
    }
    interval_owner_openings(interval.opened_revision, batches)
        .into_iter()
        .filter_map(|owner| {
            let matching = lifecycle_events
                .iter()
                .filter(is_opening)
                .filter(|event| owner.matches(session_id, event));
            match owner.anchor {
                Some(IntervalOwnerAnchor::Input(anchor)) => matching
                    .filter(|event| event.source_commit_cursor >= anchor)
                    .min_by(lifecycle_opening_order),
                Some(IntervalOwnerAnchor::ReplyTerminal(anchor)) => matching
                    .filter(|event| event.source_commit_cursor <= anchor)
                    .max_by(lifecycle_opening_order),
                Some(IntervalOwnerAnchor::ReplyAfterAwaiting(anchor)) => matching
                    .filter(|event| event.source_commit_cursor > anchor)
                    .min_by(lifecycle_opening_order),
                None => matching.min_by(lifecycle_opening_order),
            }
        })
        .min_by(|left, right| {
            left.source_commit_cursor
                .cmp(&right.source_commit_cursor)
                .then_with(|| left.cursor.cmp(&right.cursor))
        })
}

/// Find the first append-only interval whose cumulative usage reached this
/// budget generation. A still-open crossing has no immutable public coordinate
/// and remains withheld until the owning interval closes.
pub(in crate::state::events) fn budget_reach_close_cursor(
    persisted: &awaken_session_contract::PersistedSession,
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
pub(in crate::state::events) fn budget_reach_projection_close_cursor(
    persisted: &awaken_session_contract::PersistedSession,
    transition: &awaken_session_contract::BudgetReachTransition,
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

/// Exact commit boundary of the classifier-owned report messages for one Run.
/// The vector alignment is part of the recovery contract; an older snapshot
/// without cursors must withhold the derived cross-Thread event.
pub(super) fn report_commit_cursor(
    snapshot: &awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
    run_id: &awaken_agent_contract::agent::run::Id,
) -> Option<u64> {
    if snapshot.messages.len() != snapshot.message_commit_cursors.len() {
        return None;
    }
    let report_ids = session_agent_report_messages(&snapshot.messages, run_id)
        .into_iter()
        .map(|message| message.id.0.as_str())
        .collect::<std::collections::HashSet<_>>();
    snapshot
        .messages
        .iter()
        .zip(snapshot.message_commit_cursors.iter().copied())
        .filter(|(message, _)| report_ids.contains(message.id.0.as_str()))
        .map(|(_, cursor)| cursor)
        .max()
}

/// First committed visibility of one accepted `send_to_agent` cross-post. The
/// ToolUse declares intent, but only its non-error accepted ToolResult binds a
/// concrete coordinated Thread.
pub(super) fn accepted_coordination_order(
    record: &SessionRecord,
    orders: &std::collections::HashMap<String, CanonicalEventOrder>,
    source: &Event,
    thread_id: &str,
) -> Option<CanonicalEventOrder> {
    if ManagedState::accepted_coordination_thread(record, &source.id).as_deref() != Some(thread_id)
    {
        return None;
    }
    let source_order = orders.get(&source.id).copied()?;
    let result_order = record.events.iter().find_map(|event| match &event.kind {
        OutboundKind::AgentToolResult {
            tool_use_id,
            is_error,
            ..
        } if tool_use_id == &source.id && *is_error != Some(true) => orders.get(&event.id).copied(),
        _ => None,
    })?;
    Some(source_order.max(result_order))
}
