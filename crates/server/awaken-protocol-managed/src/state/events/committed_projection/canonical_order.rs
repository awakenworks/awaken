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
    interval: &awaken_session_contract::SessionRuntimeInterval,
    prior_close: u64,
    lifecycle_events: &'a [RunLifecycleEvent],
) -> Option<&'a RunLifecycleEvent> {
    interval
        .observations
        .iter()
        .filter_map(|observation| {
            lifecycle_events
                .iter()
                .filter(|event| {
                    event.thread_id == observation.thread_id
                        && event.run_id == observation.run_id
                        && event.source_commit_cursor > prior_close
                        && event.source_commit_cursor <= observation.source_commit_cursor
                        && matches!(
                            event.kind,
                            RunLifecycleEventKind::Running
                                | RunLifecycleEventKind::Resumed
                                | RunLifecycleEventKind::Rescheduled
                        )
                })
                .max_by(|left, right| {
                    left.source_commit_cursor
                        .cmp(&right.source_commit_cursor)
                        .then_with(|| left.cursor.cmp(&right.cursor))
                })
        })
        .min_by(|left, right| {
            left.source_commit_cursor
                .cmp(&right.source_commit_cursor)
                .then_with(|| left.cursor.cmp(&right.cursor))
        })
}

/// Resolve the exact lifecycle event already proven to open a live Session
/// interval. At the first-lineage cutover, a retained batch is still required
/// so a cold replica never mistakes an older lifecycle for the new opening.
pub(super) fn running_interval_lifecycle_open_event<'a>(
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
    let owning_revision = batches
        .iter()
        .filter(|batch| batch.admitted_revision <= interval.opened_revision)
        .map(|batch| batch.admitted_revision)
        .max()?;
    batches
        .iter()
        .filter(|batch| batch.admitted_revision == owning_revision)
        .flat_map(|batch| batch.events.iter())
        .filter_map(|entry| match &entry.event {
            SessionEventCommand::UserMessage { run_id, .. } => Some(run_id),
            SessionEventCommand::ToolReply { reply, .. } => Some(&reply.expected_run_id),
            _ => None,
        })
        .filter_map(|run_id| {
            lifecycle_events
                .iter()
                .filter(is_opening)
                .filter(|event| event.run_id == *run_id)
                .max_by(|left, right| {
                    left.source_commit_cursor
                        .cmp(&right.source_commit_cursor)
                        .then_with(|| left.cursor.cmp(&right.cursor))
                })
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
