//! Canonical ordering and time projection for the disposable Managed event vector.

use super::*;

#[derive(Clone, Copy, Debug)]
pub(super) struct CanonicalEventOrder {
    pub(super) source_commit_cursor: u64,
    pub(super) phase: u16,
    pub(super) ordinal: usize,
    pub(super) processed_at_unix_ms: Option<u64>,
}

// Wall time is evidence, never ordering/identity; clock rollback cannot move events.
impl PartialEq for CanonicalEventOrder {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for CanonicalEventOrder {}
impl PartialOrd for CanonicalEventOrder {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for CanonicalEventOrder {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.source_commit_cursor, self.phase, self.ordinal).cmp(&(
            other.source_commit_cursor,
            other.phase,
            other.ordinal,
        ))
    }
}

/// Maximum number of wire events that the canonical one-Message encoder can
/// emit. An Assistant message contributes at most one thinking marker, one
/// visible message, and one tool event per content block; every other role
/// contributes no more than one event per block. Keeping this bound beside the
/// canonicalizer prevents recovery from probing every Session event ordinal
/// for every source message.
pub(in crate::state::events) fn message_projection_ordinal_bound(
    message: &awaken_agent_contract::agent::message::Message,
) -> usize {
    message.content.len().saturating_add(2)
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
    time_for: impl Fn(&str, u64) -> Option<u64>,
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
            processed_at_unix_ms: time_for(&lifecycle.thread_id.0, lifecycle.source_commit_cursor),
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
        if lifecycle.thread_id.0 == session_id
            && matches!(
                lifecycle.kind,
                RunLifecycleEventKind::Awaiting
                    | RunLifecycleEventKind::Completed
                    | RunLifecycleEventKind::Failed
                    | RunLifecycleEventKind::Cancelled
            )
        {
            retain_earliest_order(
                orders,
                managed_multiagent_event_id(
                    session_id,
                    session_id,
                    "aggregate-terminal-status-running",
                    ManagedMultiagentEventProvenance::LifecyclePrefix {
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

/// The live and closed forms use one opening selector. Closing an interval
/// adds terminal evidence; it cannot replace its already-issued opening with
/// a later Running commit of the same resumed Run.
pub(super) fn interval_lifecycle_open_event<'a>(
    session_id: &str,
    interval: &awaken_session_contract::SessionRuntimeInterval,
    prior_close: u64,
    batches: &[awaken_session_contract::SessionEventBatch],
    lifecycle_events: &'a [RunLifecycleEvent],
) -> Option<&'a RunLifecycleEvent> {
    select_interval_opening(
        session_id,
        interval.opened_revision,
        interval.activity_epoch,
        &interval.observations,
        prior_close,
        batches,
        lifecycle_events,
    )
}

fn select_interval_opening<'a>(
    session_id: &str,
    opened_revision: awaken_session_contract::SessionRevision,
    activity_epoch: u64,
    observations: &[awaken_session_contract::SessionRuntimeIntervalObservation],
    prior_close: u64,
    batches: &[awaken_session_contract::SessionEventBatch],
    lifecycle_events: &'a [RunLifecycleEvent],
) -> Option<&'a RunLifecycleEvent> {
    let candidates = lifecycle_events
        .iter()
        .filter(|event| {
            matches!(
                event.kind,
                RunLifecycleEventKind::Running
                    | RunLifecycleEventKind::Resumed
                    | RunLifecycleEventKind::Rescheduled
            )
        })
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
    select_interval_opening(
        session_id,
        interval.opened_revision,
        interval.activity_epoch,
        &interval.observations,
        prior_close,
        batches,
        lifecycle_events,
    )
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

/// First committed visibility of one accepted `send_message` cross-post. The
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

impl ManagedState {
    pub(super) fn canonicalize_committed_events(
        record: &mut SessionRecord,
        evidence: CanonicalizationEvidence<'_>,
    ) -> Result<(), StateError> {
        let CanonicalizationEvidence {
            persisted,
            lifecycle_events,
            root_snapshot,
            outcome_projections,
            delegation: delegation_evidence,
            root_pending,
            child_pending,
        } = evidence;
        let DelegationProjectionEvidence {
            links,
            snapshots: child_snapshots,
            historical_pending,
            ..
        } = delegation_evidence;
        let time_for = |thread_id: &str, cursor: u64| {
            let snapshot = if thread_id == persisted.session_id {
                root_snapshot
            } else {
                child_snapshots.get(thread_id)
            };
            snapshot.and_then(|snapshot| snapshot.commit_timestamps.get(&cursor).copied())
        };
        let mut orders = std::collections::HashMap::<String, CanonicalEventOrder>::new();
        let batch_stride = persisted
            .event_batches
            .iter()
            .map(|batch| batch.events.len())
            .max()
            .unwrap_or_default()
            .saturating_add(1);
        for (batch_index, batch) in persisted.event_batches.iter().enumerate() {
            for (ordinal, entry) in batch.events.iter().enumerate() {
                let Some((source_commit_cursor, phase)) =
                    inbound_admission_order(batch.admission_projection_anchor)
                else {
                    continue;
                };
                retain_earliest_order(
                    &mut orders,
                    durable_inbound_event_id(&record.session.id, entry.event.operation_id()),
                    CanonicalEventOrder {
                        processed_at_unix_ms: entry.processed_at_unix_ms,
                        source_commit_cursor,
                        phase,
                        ordinal: batch_index
                            .saturating_mul(batch_stride)
                            .saturating_add(ordinal),
                    },
                );
            }
        }

        index_lifecycle_orders(&mut orders, &record.session.id, lifecycle_events, time_for);

        // Synthetic pending-tool cause/effect table: C1 the Runtime has one
        // committed ResumeTicket; C2 its owning Awaiting lifecycle fact is
        // committed; C3 the ToolUse transcript block has not arrived yet.
        // C1+C2+C3 projects one client-answerable tool event whose immutable
        // order is the Awaiting fact; C1+!C2 remains unindexed and therefore
        // fails the canonicalization guard below. A later transcript-owned
        // occurrence has a message-qualified id and keeps its own message cursor.
        for (cursor, pending) in historical_pending {
            if let Some(awaiting) = lifecycle_events.iter().find(|event| {
                event.cursor == *cursor && event.kind == RunLifecycleEventKind::Awaiting
            }) {
                let public_owner_thread_id =
                    public_thread_id(&record.session.id, &awaiting.thread_id.0);
                retain_earliest_order(
                    &mut orders,
                    managed_tool_event_id(
                        &public_owner_thread_id,
                        &awaiting.run_id.0,
                        &pending.tool_use_id,
                    ),
                    CanonicalEventOrder {
                        processed_at_unix_ms: time_for(
                            &awaiting.thread_id.0,
                            awaiting.source_commit_cursor,
                        ),
                        source_commit_cursor: awaiting.source_commit_cursor,
                        phase: 50,
                        ordinal: 0,
                    },
                );
            }
        }
        for (thread_id, pending) in std::iter::once((record.session.id.as_str(), root_pending))
            .chain(
                child_pending
                    .iter()
                    .map(|(thread_id, pending)| (thread_id.as_str(), Some(pending))),
            )
            .filter_map(|(thread_id, pending)| pending.map(|pending| (thread_id, pending)))
        {
            if let Some(awaiting) = lifecycle_events.iter().rev().find(|event| {
                event.thread_id.0 == thread_id
                    && event.kind == RunLifecycleEventKind::Awaiting
                    && historical_pending.get(&event.cursor) != Some(pending)
            }) {
                let public_owner_thread_id =
                    public_thread_id(&record.session.id, &awaiting.thread_id.0);
                retain_earliest_order(
                    &mut orders,
                    managed_tool_event_id(
                        &public_owner_thread_id,
                        &awaiting.run_id.0,
                        &pending.tool_use_id,
                    ),
                    CanonicalEventOrder {
                        processed_at_unix_ms: time_for(
                            &awaiting.thread_id.0,
                            awaiting.source_commit_cursor,
                        ),
                        source_commit_cursor: awaiting.source_commit_cursor,
                        phase: 50,
                        ordinal: 0,
                    },
                );
            }
        }

        for interval in &persisted.closed_runtime_intervals {
            let Some(close) = interval_close_cursor(interval) else {
                continue;
            };
            for (role, phase) in [("aggregate-usage", 90), ("aggregate-status-idle", 100)] {
                retain_earliest_order(
                    &mut orders,
                    managed_multiagent_event_id(
                        &record.session.id,
                        &record.session.id,
                        role,
                        ManagedMultiagentEventProvenance::RuntimeInterval {
                            interval_id: &interval.interval_id,
                        },
                    ),
                    CanonicalEventOrder {
                        processed_at_unix_ms: Some(interval.ended_at_unix_ms),
                        source_commit_cursor: close,
                        phase,
                        ordinal: 0,
                    },
                );
            }
        }

        let mut index_messages = |thread_id: &str,
                                  snapshot: &awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot| {
            if snapshot.message_commit_cursors.len() != snapshot.messages.len() {
                return;
            }
            let run_ids = snapshot.runs.iter().map(|run| run.id.clone()).collect::<Vec<_>>();
            let response_coordinates = Self::assistant_response_coordinates(&snapshot.messages, &run_ids);
            let public_owner_thread_id = public_thread_id(&record.session.id, thread_id);
            let event_count = record.events.len();
            let event_roles = record
                .events
                .iter()
                .map(|event| event.kind.type_str())
                .collect::<std::collections::HashSet<_>>();
            for (message_index, (message, source_commit_cursor)) in snapshot
                .messages
                .iter()
                .zip(snapshot.message_commit_cursors.iter().copied())
                .enumerate()
            {
                // Generic ids are opaque hashes, but their source ordinal is
                // bounded by this one Message's encoder output, not by the
                // Session-wide event count. Build the small reverse index once
                // and use O(1) lookups below. The former event-by-event
                // `0..=event_count` search was cubic across a growing transcript
                // and held the shared Session projection lock long enough to
                // starve unrelated Worker heartbeats.
                let ordinal_bound = message_projection_ordinal_bound(message);
                let mut generic_source_ordinals =
                    std::collections::HashMap::with_capacity(
                        ordinal_bound.saturating_mul(event_roles.len()),
                    );
                for role in &event_roles {
                    for ordinal in 0..ordinal_bound {
                        generic_source_ordinals.insert(
                            managed_multiagent_event_id(
                                &record.session.id,
                                thread_id,
                                role,
                                ManagedMultiagentEventProvenance::Message {
                                    message_id: &message.id.0,
                                    ordinal,
                                },
                            ),
                            ordinal,
                        );
                    }
                }
                for event in &record.events {
                    let tool_source_ordinal = decode_managed_tool_event_id(&event.id)
                        .filter(|identity| {
                            identity.thread_id == public_owner_thread_id.as_str()
                                && identity.source_id == message.id.0
                        })
                        .and_then(|identity| {
                            message.content.iter().position(|block| {
                                matches!(block, ContentBlock::ToolUse { id, .. } if id == identity.call_id)
                                    || matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == identity.call_id)
                            })
                        });
                    let generic_source_ordinal =
                        generic_source_ordinals.get(&event.id).copied();
                    let assistant_source_ordinal = response_coordinates
                        .get(&message.id.0)
                        .and_then(|(run_id, step, response)| {
                            (event.id
                                == managed_assistant_event_id(
                                    &record.session.id,
                                    thread_id,
                                    run_id,
                                    *step,
                                    *response,
                                    event.kind.type_str(),
                                ))
                            .then(|| match &event.kind {
                                OutboundKind::AgentThinking {} => 0,
                                OutboundKind::AgentMessage { .. } => usize::from(
                                    message
                                        .content
                                        .iter()
                                        .any(|block| matches!(block, ContentBlock::Thinking { .. })),
                                ),
                                _ => 0,
                            })
                        });
                    if tool_source_ordinal.is_some()
                        || generic_source_ordinal.is_some()
                        || assistant_source_ordinal.is_some()
                    {
                        retain_earliest_order(
                            &mut orders,
                            event.id.clone(),
                            CanonicalEventOrder {
                        processed_at_unix_ms: snapshot
                            .commit_timestamps
                            .get(&source_commit_cursor)
                            .copied(),
                                source_commit_cursor,
                                phase: 50,
                                ordinal: message_index
                                    .saturating_mul(event_count.saturating_add(1))
                                    .saturating_add(
                                        tool_source_ordinal
                                            .or(generic_source_ordinal)
                                            .or(assistant_source_ordinal)
                                            .unwrap_or_default(),
                                    ),
                            },
                        );
                    }
                }
            }
        };
        if let Some(snapshot) = root_snapshot {
            index_messages(&record.session.id, snapshot);
        }
        for (thread_id, snapshot) in child_snapshots {
            index_messages(thread_id, snapshot);
        }

        let mut index_snapshot_facts = |thread_id: &str,
                                        snapshot: &awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot| {
            for audit in &snapshot.events {
                let source_commit_cursor = awaken_agent_contract::decode_run_lifecycle_cursor(
                    awaken_agent_contract::RunLifecycleCursor(audit.sequence),
                )
                .0;
                for (role, phase) in [
                    ("model-request-start", 40),
                    ("model-request-end", 41),
                ] {
                    retain_earliest_order(
                        &mut orders,
                        managed_multiagent_event_id(
                            &record.session.id,
                            thread_id,
                            role,
                            ManagedMultiagentEventProvenance::Audit {
                                run_id: &audit.run_id.0,
                                sequence: audit.sequence,
                            },
                        ),
                        CanonicalEventOrder {
                        processed_at_unix_ms: snapshot
                            .commit_timestamps
                            .get(&source_commit_cursor)
                            .copied(),
                            source_commit_cursor,
                            phase,
                            ordinal: usize::try_from(audit.sequence).unwrap_or(usize::MAX),
                        },
                    );
                }
            }
            for run in &snapshot.runs {
                if let Some(source_commit_cursor) =
                    awaken_runtime_contract::compaction::RunCompactionMarker::recorded_commit_cursor(
                        &snapshot.state,
                        &snapshot.state_commit_cursors,
                        &run.id.0,
                    )
                {
                    retain_earliest_order(
                        &mut orders,
                        managed_multiagent_event_id(
                            &record.session.id,
                            thread_id,
                            "run-context-compacted",
                            ManagedMultiagentEventProvenance::RunState {
                                run_id: &run.id.0,
                            },
                        ),
                        CanonicalEventOrder {
                        processed_at_unix_ms: snapshot
                            .commit_timestamps
                            .get(&source_commit_cursor)
                            .copied(),
                            source_commit_cursor,
                            phase: 42,
                            ordinal: 0,
                        },
                    );
                }
            }
            if let Some(source_commit_cursor) =
                awaken_agent_contract::archived_thread_commit_cursor(
                    &snapshot.state,
                    &snapshot.state_commit_cursors,
                )
            {
                retain_earliest_order(
                    &mut orders,
                    managed_multiagent_event_id(
                        &record.session.id,
                        thread_id,
                        "archived-status-terminated",
                        ManagedMultiagentEventProvenance::ArchivedDisposition,
                    ),
                    CanonicalEventOrder {
                        processed_at_unix_ms: snapshot
                            .commit_timestamps
                            .get(&source_commit_cursor)
                            .copied(),
                        source_commit_cursor,
                        phase: 81,
                        ordinal: 0,
                    },
                );
            }
        };
        if let Some(snapshot) = root_snapshot {
            index_snapshot_facts(&record.session.id, snapshot);
        }
        for (thread_id, snapshot) in child_snapshots {
            index_snapshot_facts(thread_id, snapshot);
        }

        let mut tool_operation_by_event_id = std::collections::HashMap::<String, String>::new();
        let mut advisor_operation_orders =
            std::collections::HashMap::<String, CanonicalEventOrder>::new();
        let mut index_tool_operations =
            |snapshot: &awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot| {
                if snapshot.messages.len() != snapshot.message_commit_cursors.len() {
                    return;
                }
                let run_ids = snapshot
                    .runs
                    .iter()
                    .map(|run| run.id.clone())
                    .collect::<Vec<_>>();
                let coordinates =
                    Self::assistant_response_coordinates(&snapshot.messages, &run_ids);
                for (message_index, (message, source_commit_cursor)) in snapshot
                    .messages
                    .iter()
                    .zip(snapshot.message_commit_cursors.iter().copied())
                    .enumerate()
                {
                    let Some((run_id, step, _)) = coordinates.get(&message.id.0) else {
                        continue;
                    };
                    for (block_ordinal, block) in message.content.iter().enumerate() {
                        let ContentBlock::ToolUse {
                            id: call_id, name, ..
                        } = block
                        else {
                            continue;
                        };
                        if name != awaken_runtime_contract::resolved::ADVISOR_TOOL_ID {
                            continue;
                        }
                        let operation_id =
                            awaken_runtime_contract::tool_batch::ToolBatch::operation_id_for_step(
                                &awaken_agent_contract::agent::run::Id(run_id.clone()),
                                *step,
                                call_id,
                            );
                        let order = CanonicalEventOrder {
                            processed_at_unix_ms: snapshot
                                .commit_timestamps
                                .get(&source_commit_cursor)
                                .copied(),
                            source_commit_cursor,
                            phase: 50,
                            ordinal: message_index
                                .saturating_mul(message.content.len().saturating_add(1))
                                .saturating_add(block_ordinal),
                        };
                        advisor_operation_orders
                            .entry(operation_id)
                            .and_modify(|current| *current = (*current).min(order))
                            .or_insert(order);
                    }
                }
                for event in &record.events {
                    let Some(identity) = decode_managed_tool_event_id(&event.id) else {
                        continue;
                    };
                    let Some((run_id, step, _)) = coordinates.get(identity.source_id) else {
                        continue;
                    };
                    tool_operation_by_event_id.insert(
                        event.id.clone(),
                        awaken_runtime_contract::tool_batch::ToolBatch::operation_id_for_step(
                            &awaken_agent_contract::agent::run::Id(run_id.clone()),
                            *step,
                            identity.call_id,
                        ),
                    );
                }
            };
        if let Some(snapshot) = root_snapshot {
            index_tool_operations(snapshot);
        }
        for snapshot in child_snapshots.values() {
            index_tool_operations(snapshot);
        }

        for link in links {
            let source = record.events.iter().find(|event| {
                matches!(event.kind, OutboundKind::AgentToolUse { .. })
                    && tool_operation_by_event_id.get(&event.id)
                        == Some(&link.created_by_operation_id)
            });
            let source_order = match &link.target {
                CoordinatedThreadTarget::Agent { .. } => source.and_then(|source| {
                    accepted_coordination_order(record, &orders, source, &link.thread_id.0)
                }),
                CoordinatedThreadTarget::Advisor { .. } => advisor_operation_orders
                    .get(&link.created_by_operation_id)
                    .copied(),
            };
            if let Some(source_order) = source_order {
                for (role, phase) in [("link-thread-created", 51), ("link-status-running", 52)] {
                    retain_earliest_order(
                        &mut orders,
                        managed_multiagent_event_id(
                            &record.session.id,
                            &link.thread_id.0,
                            role,
                            ManagedMultiagentEventProvenance::LinkOperation {
                                operation_id: &link.created_by_operation_id,
                            },
                        ),
                        CanonicalEventOrder {
                            processed_at_unix_ms: source_order.processed_at_unix_ms,
                            source_commit_cursor: source_order.source_commit_cursor,
                            phase,
                            ordinal: source_order.ordinal,
                        },
                    );
                }
            }
            for source in record.events.iter().filter(|event| {
                matches!(
                    &event.kind,
                    OutboundKind::AgentToolUse { name, .. } if name == SEND_MESSAGE
                )
            }) {
                let Some(source_order) =
                    accepted_coordination_order(record, &orders, source, &link.thread_id.0)
                else {
                    continue;
                };
                for (role, phase) in [
                    ("coordination-status-running", 54),
                    ("coordination-message-sent", 55),
                ] {
                    retain_earliest_order(
                        &mut orders,
                        managed_multiagent_event_id(
                            &record.session.id,
                            &link.thread_id.0,
                            role,
                            ManagedMultiagentEventProvenance::CoordinationCall {
                                event_id: &source.id,
                            },
                        ),
                        CanonicalEventOrder {
                            processed_at_unix_ms: source_order.processed_at_unix_ms,
                            source_commit_cursor: source_order.source_commit_cursor,
                            phase,
                            ordinal: source_order.ordinal,
                        },
                    );
                }
            }
            for terminal in lifecycle_events.iter().filter(|event| {
                event.thread_id == link.thread_id
                    && matches!(
                        event.kind,
                        RunLifecycleEventKind::Completed
                            | RunLifecycleEventKind::Failed
                            | RunLifecycleEventKind::Cancelled
                    )
            }) {
                let report_source = child_snapshots
                    .get(&link.thread_id.0)
                    .and_then(|snapshot| report_commit_cursor(snapshot, &terminal.run_id));
                let Some(source_commit_cursor) = report_source else {
                    continue;
                };
                for role in ["child-message-received", "advisor-message-received"] {
                    retain_earliest_order(
                        &mut orders,
                        managed_multiagent_event_id(
                            &record.session.id,
                            &link.thread_id.0,
                            role,
                            ManagedMultiagentEventProvenance::AgentReport {
                                run_id: &terminal.run_id.0,
                            },
                        ),
                        CanonicalEventOrder {
                            processed_at_unix_ms: time_for(
                                &terminal.thread_id.0,
                                terminal.source_commit_cursor,
                            ),
                            source_commit_cursor: source_commit_cursor
                                .max(terminal.source_commit_cursor),
                            phase: 55,
                            ordinal: 0,
                        },
                    );
                }
            }
        }

        for (batch_index, projection) in outcome_projections.iter().enumerate() {
            match &projection.terminal {
                CommittedOutcomeProjection::Completed(report) => {
                    for iteration in &report.iterations {
                        let Some(source_commit_cursor) = projection
                            .evaluation_commit_cursors
                            .get(&iteration.iteration)
                            .copied()
                        else {
                            continue;
                        };
                        if let Some(evaluation) = record
                            .session
                            .outcome_evaluations
                            .iter_mut()
                            .find(|evaluation| {
                                evaluation.outcome_id == projection.outcome_id
                                    && evaluation.iteration == iteration.iteration
                            })
                            && evaluation.completed_at.is_some()
                        {
                            evaluation.completed_at =
                                time_for(&persisted.session_id, source_commit_cursor)
                                    .map(awaken_session_contract::epoch_millis_to_rfc3339);
                        }
                        for (role, phase) in [
                            ("outcome-evaluation-start", 60),
                            ("outcome-evaluation-ongoing", 61),
                            ("outcome-evaluation-end", 62),
                        ] {
                            retain_earliest_order(
                                &mut orders,
                                managed_multiagent_event_id(
                                    &record.session.id,
                                    &record.session.id,
                                    role,
                                    ManagedMultiagentEventProvenance::OutcomeEvaluation {
                                        outcome_id: &projection.outcome_id,
                                        iteration: iteration.iteration,
                                    },
                                ),
                                CanonicalEventOrder {
                                    processed_at_unix_ms: time_for(
                                        &persisted.session_id,
                                        source_commit_cursor,
                                    ),
                                    source_commit_cursor,
                                    phase,
                                    ordinal: batch_index
                                        .saturating_mul(batch_stride)
                                        .saturating_add(
                                            usize::try_from(iteration.iteration)
                                                .unwrap_or(usize::MAX),
                                        ),
                                },
                            );
                        }
                    }
                }
                CommittedOutcomeProjection::Errored(_) => {
                    retain_earliest_order(
                        &mut orders,
                        managed_multiagent_event_id(
                            &record.session.id,
                            &record.session.id,
                            "outcome-error",
                            ManagedMultiagentEventProvenance::Outcome {
                                outcome_id: &projection.outcome_id,
                            },
                        ),
                        CanonicalEventOrder {
                            processed_at_unix_ms: time_for(
                                &persisted.session_id,
                                projection.terminal_commit_cursor,
                            ),
                            source_commit_cursor: projection.terminal_commit_cursor,
                            phase: 63,
                            ordinal: batch_index.saturating_mul(batch_stride),
                        },
                    );
                }
            }
        }

        for (index, transition) in persisted.budget.reach_transitions().iter().enumerate() {
            let Some(source_commit_cursor) =
                budget_reach_projection_close_cursor(persisted, transition, lifecycle_events)
            else {
                continue;
            };
            for (role, phase) in [
                ("budget-reach-usage", 90),
                ("budget-reach-status-idle", 100),
            ] {
                retain_earliest_order(
                    &mut orders,
                    managed_multiagent_event_id(
                        &record.session.id,
                        &record.session.id,
                        role,
                        ManagedMultiagentEventProvenance::BudgetReach {
                            generation: transition.generation,
                        },
                    ),
                    CanonicalEventOrder {
                        processed_at_unix_ms: time_for(&persisted.session_id, source_commit_cursor),
                        source_commit_cursor,
                        phase,
                        ordinal: index,
                    },
                );
            }
        }

        let cleanup_terminal_source = persisted.terminal_cleanup.runtime_commit_cursor();
        // Initial activation can fail before Runtime owns a cleanup cursor. In
        // that case terminality follows the complete immutable prefix already
        // indexed above. The derived maximum is disposable ordering, never a
        // second persisted cursor or Session state.
        let parent_terminal_source = cleanup_terminal_source.or_else(|| {
            (persisted.execution
                == awaken_session_contract::SessionExecutionState::ActivationFailed)
                .then(|| {
                    orders
                        .values()
                        .map(|order| order.source_commit_cursor)
                        .max()
                        .unwrap_or_default()
                })
        });
        if persisted.execution == awaken_session_contract::SessionExecutionState::ActivationFailed
            && let Some(source_commit_cursor) = parent_terminal_source
        {
            retain_earliest_order(
                &mut orders,
                managed_multiagent_event_id(
                    &record.session.id,
                    &record.session.id,
                    "activation-failed-error",
                    ManagedMultiagentEventProvenance::ParentTerminal,
                ),
                CanonicalEventOrder {
                    processed_at_unix_ms: persisted.realization_progress.failed_at_unix_ms,
                    source_commit_cursor,
                    phase: 109,
                    ordinal: 0,
                },
            );
        }
        for (terminal_source, thread_id, role, phase) in parent_terminal_source
            .into_iter()
            .flat_map(|terminal_source| {
                links
                    .iter()
                    .map(|link| {
                        (
                            link.thread_id.0.as_str(),
                            "parent-terminal-child-status-terminated",
                            110,
                        )
                    })
                    .chain([
                        (
                            record.session.id.as_str(),
                            "parent-terminal-primary-status-terminated",
                            111,
                        ),
                        (
                            record.session.id.as_str(),
                            "parent-terminal-session-status-terminated",
                            112,
                        ),
                    ])
                    .map(move |(thread_id, role, phase)| (terminal_source, thread_id, role, phase))
            })
        {
            retain_earliest_order(
                &mut orders,
                managed_multiagent_event_id(
                    &record.session.id,
                    thread_id,
                    role,
                    ManagedMultiagentEventProvenance::ParentTerminal,
                ),
                CanonicalEventOrder {
                    processed_at_unix_ms: persisted
                        .archived_at()
                        .and_then(awaken_session_contract::rfc3339_to_epoch_millis)
                        .or_else(|| time_for(&persisted.session_id, terminal_source)),
                    source_commit_cursor: terminal_source,
                    phase,
                    ordinal: 0,
                },
            );
        }

        let visible_durable_ids = record
            .events
            .iter()
            .filter(|event| !is_transient_event_id(&event.id))
            .map(|event| event.id.clone())
            .collect::<std::collections::HashSet<_>>();
        let mut still_pending = Vec::new();
        for (event, predecessor) in std::mem::take(&mut record.overlay.pending_events) {
            if visible_durable_ids.contains(&predecessor) {
                record.events.push(event);
            } else {
                still_pending.push((event, predecessor));
            }
        }
        record.overlay.pending_events = still_pending;

        // Canonicalization is a disposable projection transaction: validate
        // every immutable anchor against a snapshot of the issued prefix, then
        // replace the cache once. A late/malformed fact must fail closed without
        // erasing receipts and cursors that callers already observed.
        let original_events = record.events.clone();
        let mut durable = Vec::new();
        for mut event in original_events.iter().cloned() {
            if is_transient_event_id(&event.id) {
                continue;
            } else {
                let order = orders.get(&event.id).copied().ok_or_else(|| {
                    StateError::Run(RunError::internal(format!(
                        "committed Managed event {} ({}) has no immutable projection anchor",
                        event.id,
                        event.type_str(),
                    )))
                })?;
                event.processed_at = event.processed_at.or_else(|| {
                    order
                        .processed_at_unix_ms
                        .map(awaken_session_contract::epoch_millis_to_rfc3339)
                });
                durable.push((order, event));
            }
        }
        durable.sort_by(|(left_order, left), (right_order, right)| {
            left_order
                .cmp(right_order)
                .then_with(|| left.id.cmp(&right.id))
        });
        let durable_ids = durable
            .iter()
            .map(|(_, event)| event.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        let mut overlays = std::collections::HashMap::<Option<String>, Vec<Event>>::new();
        for (index, event) in original_events
            .iter()
            .enumerate()
            .filter(|(_, event)| is_transient_event_id(&event.id))
        {
            let anchor = record
                .overlay
                .anchors
                .get(&event.id)
                .filter(|candidate| durable_ids.contains(candidate.as_str()))
                .cloned()
                .or_else(|| {
                    original_events[..index]
                        .iter()
                        .rev()
                        .find(|candidate| durable_ids.contains(candidate.id.as_str()))
                        .map(|candidate| candidate.id.clone())
                });
            overlays.entry(anchor).or_default().push(event.clone());
        }
        let mut canonical = overlays.remove(&None).unwrap_or_default();
        for (_, event) in durable {
            let id = event.id.clone();
            canonical.push(event);
            if let Some(mut anchored) = overlays.remove(&Some(id)) {
                canonical.append(&mut anchored);
            }
        }
        debug_assert!(
            overlays.is_empty(),
            "every transient overlay has a durable anchor"
        );
        for child in &mut record.child_threads {
            let times = canonical
                .iter()
                .filter(|event| {
                    Self::project_event_for_thread(
                        &record.session.id,
                        &child.id,
                        (*event).clone(),
                        record
                            .event_thread_owners
                            .get(&event.id)
                            .map(String::as_str),
                        false,
                    )
                    .is_some()
                })
                .filter_map(|event| event.processed_at.as_ref())
                .collect::<Vec<_>>();
            // Creation/terminal time comes from the exact lifecycle event;
            // a prior broadcast interrupt or later cleanup cannot replace it.
            if let Some(at) = canonical.iter().find_map(|event| match &event.kind {
                OutboundKind::SessionThreadCreated {
                    session_thread_id, ..
                } if session_thread_id == &child.id => event.processed_at.clone(),
                _ => None,
            }) {
                child.created_at = at;
            }
            if let Some(last) = times.last() {
                child.updated_at = (*last).clone();
            }
            if child.status == SessionThreadStatus::Terminated
                && let Some(at) = canonical.iter().find_map(|event| match &event.kind {
                    OutboundKind::SessionThreadStatusTerminated {
                        session_thread_id, ..
                    } if session_thread_id == &child.id => event.processed_at.clone(),
                    _ => None,
                })
            {
                child.archived_at = Some(at);
            }
        }
        record.events = canonical;
        Ok(())
    }
}

#[cfg(kani)]
#[kani::proof]
fn canonical_event_order_never_depends_on_wall_time() {
    // O1 all order-key axes and optional wall-time observations are arbitrary.
    // O2 same key/different time remains equal. Production Ord/Eq are invoked
    // directly; evidence timestamps cannot reorder or split an identity.
    let left = CanonicalEventOrder {
        source_commit_cursor: kani::any(),
        phase: kani::any(),
        ordinal: kani::any(),
        processed_at_unix_ms: kani::any::<bool>().then(kani::any::<u64>),
    };
    let right = CanonicalEventOrder {
        source_commit_cursor: kani::any(),
        phase: kani::any(),
        ordinal: kani::any(),
        processed_at_unix_ms: kani::any::<bool>().then(kani::any::<u64>),
    };
    assert_eq!(
        left.cmp(&right),
        (left.source_commit_cursor, left.phase, left.ordinal).cmp(&(
            right.source_commit_cursor,
            right.phase,
            right.ordinal
        ))
    );
    assert_eq!(
        left,
        CanonicalEventOrder {
            processed_at_unix_ms: right.processed_at_unix_ms,
            ..left
        }
    );
}

#[cfg(test)]
#[test]
fn canonical_order_time_metamorphism_covers_every_sort_axis() {
    // O1/O2 keys independently vary cursor/phase/ordinal; clocks independently
    // vary missing/zero/max/backward. Changing only time must preserve both
    // equality and strict ordering; each axis is a mandatory tie-breaker.
    for left_key in [(0, 0, 0), (1, 0, 0), (1, 1, 0), (1, 1, 1)] {
        for right_key in [(0, 0, 0), (1, 0, 0), (1, 1, 0), (1, 1, 1)] {
            for left_time in [None, Some(0), Some(u64::MAX)] {
                for right_time in [None, Some(0), Some(u64::MAX)] {
                    let left = CanonicalEventOrder {
                        source_commit_cursor: left_key.0,
                        phase: left_key.1,
                        ordinal: left_key.2,
                        processed_at_unix_ms: left_time,
                    };
                    let right = CanonicalEventOrder {
                        source_commit_cursor: right_key.0,
                        phase: right_key.1,
                        ordinal: right_key.2,
                        processed_at_unix_ms: right_time,
                    };
                    assert_eq!(left.cmp(&right), left_key.cmp(&right_key));
                    assert_eq!(left == right, left_key == right_key);
                }
            }
        }
    }
}
