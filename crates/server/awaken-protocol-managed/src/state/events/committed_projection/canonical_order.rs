//! Canonical ordering and time projection for the disposable Managed event vector.

use super::*;

mod coordination_order;
mod message_index;

#[derive(Clone, Copy, Debug)]
pub(super) struct CanonicalEventOrder {
    pub(super) source_commit_cursor: u64,
    pub(super) phase: u16,
    pub(super) ordinal: usize,
    pub(super) processed_at_unix_ms: Option<u64>,
}

const HISTORICAL_PENDING_PHASE: u16 = 50;
const LIFECYCLE_TERMINAL_PHASE: u16 = 80;

/// Bind a synthetic historical pending event to its committed Awaiting
/// coordinate. If a later terminal for the same Run is observed behind that
/// coordinate, withhold the pending order so the private projection fails
/// closed instead of publishing terminal history first.
fn historical_pending_projection_order(
    awaiting_source_commit_cursor: u64,
    next_terminal_source_commit_cursor: Option<u64>,
    processed_at_unix_ms: Option<u64>,
) -> Option<CanonicalEventOrder> {
    if next_terminal_source_commit_cursor
        .is_some_and(|terminal| terminal < awaiting_source_commit_cursor)
    {
        return None;
    }
    Some(CanonicalEventOrder {
        source_commit_cursor: awaiting_source_commit_cursor,
        phase: HISTORICAL_PENDING_PHASE,
        ordinal: 0,
        processed_at_unix_ms,
    })
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
    let (left, right) = match (left, right) {
        (None, None) => return true,
        (Some(left), Some(right)) => (left, right),
        _ => return false,
    };
    // Exhaustive destructuring makes a new recovery field require an explicit
    // comparison decision. Only the backend-wide diagnostic cursor is ignored;
    // equal source versions alone never establish equal retained facts.
    let awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
        thread_id,
        claimed_run_id,
        runs,
        latest_run_id,
        messages,
        message_commit_cursors,
        commit_timestamps,
        state,
        state_commit_cursors,
        events,
        resume_tickets,
        thread_version,
        store_cursor: _,
        next_commit_ordinal,
    } = left;
    thread_id == &right.thread_id
        && claimed_run_id == &right.claimed_run_id
        && runs == &right.runs
        && latest_run_id == &right.latest_run_id
        && messages == &right.messages
        && message_commit_cursors == &right.message_commit_cursors
        && commit_timestamps == &right.commit_timestamps
        && state == &right.state
        && state_commit_cursors == &right.state_commit_cursors
        && events == &right.events
        && resume_tickets == &right.resume_tickets
        && thread_version == &right.thread_version
        && next_commit_ordinal == &right.next_commit_ordinal
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
        ("root-lifecycle-status-idle", LIFECYCLE_TERMINAL_PHASE),
        ("lifecycle-status-idle", LIFECYCLE_TERMINAL_PHASE),
        (
            "failed-lifecycle-status-terminated",
            LIFECYCLE_TERMINAL_PHASE,
        ),
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
        (prior_close, interval_close_cursor(interval)),
        batches,
        lifecycle_events,
    )
}

fn select_interval_opening<'a>(
    session_id: &str,
    opened_revision: awaken_session_contract::SessionRevision,
    activity_epoch: u64,
    observations: &[awaken_session_contract::SessionRuntimeIntervalObservation],
    (prior_close, close): (u64, Option<u64>),
    batches: &[awaken_session_contract::SessionEventBatch],
    lifecycle_events: &'a [RunLifecycleEvent],
) -> Option<&'a RunLifecycleEvent> {
    let candidates = lifecycle_events
        .iter()
        .filter(|event| close.is_none_or(|close| event.source_commit_cursor <= close))
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
        (prior_close, None),
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

/// Attach each live overlay to its explicit retained predecessor, or the
/// nearest earlier retained durable event. Consume the issued order once;
/// leading/unanchored overlays must not repeatedly rescan their whole prefix.
fn group_live_overlays<'a>(
    events: impl IntoIterator<Item = &'a Event>,
    durable_ids: &std::collections::HashSet<&str>,
    hints: &std::collections::HashMap<String, String>,
) -> std::collections::HashMap<Option<String>, Vec<Event>> {
    let mut result = std::collections::HashMap::<Option<String>, Vec<Event>>::new();
    let mut predecessor = None;
    for event in events {
        if !is_transient_event_id(&event.id) {
            if durable_ids.contains(event.id.as_str()) {
                predecessor = Some(event.id.as_str());
            }
            continue;
        }
        let anchor = hints
            .get(&event.id)
            .filter(|candidate| durable_ids.contains(candidate.as_str()))
            .map(String::as_str)
            .or(predecessor)
            .map(str::to_owned);
        result.entry(anchor).or_default().push(event.clone());
    }
    result
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
                let next_terminal_source_commit_cursor = lifecycle_events
                    .iter()
                    .filter(|event| {
                        event.thread_id == awaiting.thread_id
                            && event.run_id == awaiting.run_id
                            && event.cursor > awaiting.cursor
                            && matches!(
                                event.kind,
                                RunLifecycleEventKind::Awaiting
                                    | RunLifecycleEventKind::Completed
                                    | RunLifecycleEventKind::Failed
                                    | RunLifecycleEventKind::Cancelled
                            )
                    })
                    .min_by_key(|event| event.cursor)
                    .map(|event| event.source_commit_cursor);
                let public_owner_thread_id =
                    public_thread_id(&record.session.id, &awaiting.thread_id.0);
                if let Some(order) = historical_pending_projection_order(
                    awaiting.source_commit_cursor,
                    next_terminal_source_commit_cursor,
                    time_for(&awaiting.thread_id.0, awaiting.source_commit_cursor),
                ) {
                    retain_earliest_order(
                        &mut orders,
                        managed_tool_event_id(
                            &public_owner_thread_id,
                            &awaiting.run_id.0,
                            &pending.tool_use_id,
                        ),
                        order,
                    );
                }
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

        let message_event_index = message_index::MessageEventIndex::new(&record.events);
        if let Some(snapshot) = root_snapshot {
            message_event_index.index_snapshot(
                &record.session.id,
                &record.session.id,
                snapshot,
                &mut orders,
            );
        }
        for (thread_id, snapshot) in child_snapshots {
            message_event_index.index_snapshot(
                &record.session.id,
                thread_id,
                snapshot,
                &mut orders,
            );
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

        coordination_order::index_coordination_orders(
            record,
            links,
            coordination_order::CoordinationSnapshots {
                root: root_snapshot,
                children: child_snapshots,
            },
            lifecycle_events,
            &message_event_index,
            &mut orders,
            &time_for,
        );

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
                                time_for(persisted.session_id.as_str(), source_commit_cursor)
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
                                        persisted.session_id.as_str(),
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
                                persisted.session_id.as_str(),
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
                        processed_at_unix_ms: time_for(
                            persisted.session_id.as_str(),
                            source_commit_cursor,
                        ),
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
                        .or_else(|| time_for(persisted.session_id.as_str(), terminal_source)),
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
        let mut overlays = group_live_overlays(
            original_events.iter(),
            &durable_ids,
            &record.overlay.anchors,
        );
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

#[cfg(kani)]
#[kani::proof]
fn historical_pending_order_precedes_every_admitted_terminal() {
    // Causes: C1 arbitrary Awaiting source cursor; C2 a later terminal is
    // absent/present at an arbitrary source cursor; C3 wall time is absent or
    // arbitrary. Effects: E1 no terminal retains pending; E2 terminal at/after
    // Awaiting admits pending strictly before terminal by canonical phase;
    // E3 a terminal behind Awaiting withholds the pending order, causing the
    // private canonicalization transaction to fail closed. This is the exact
    // production order constructor; lifecycle collection remains TCB.
    let awaiting: u64 = kani::any();
    let has_terminal: bool = kani::any();
    let terminal: u64 = kani::any();
    let observed_time = kani::any::<bool>().then(kani::any::<u64>);
    let planned = historical_pending_projection_order(
        awaiting,
        has_terminal.then_some(terminal),
        observed_time,
    );

    assert_eq!(planned.is_some(), !has_terminal || terminal >= awaiting);
    if let (Some(pending), true) = (planned, has_terminal) {
        let terminal = CanonicalEventOrder {
            source_commit_cursor: terminal,
            phase: LIFECYCLE_TERMINAL_PHASE,
            ordinal: 0,
            processed_at_unix_ms: kani::any::<bool>().then(kani::any::<u64>),
        };
        assert!(pending < terminal);
    }
}

#[cfg(test)]
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
    use awaken_session_contract::{
        SessionEventBatch, SessionEventInput, SessionEventProjectionAnchor, SessionRevision,
    };
    let mut batch = SessionEventBatch::compile(
        "session",
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

#[cfg(test)]
#[test]
fn historical_pending_order_decision_table() {
    // Cause/effect table: H1 no terminal=>publish pending; H2 same-coordinate
    // terminal=>pending phase first; H3 later terminal=>pending cursor first;
    // H4 terminal behind Awaiting=>withhold so canonicalization fails closed.
    // Wall-time is intentionally varied and never changes any decision/order.
    for (rule, terminal, expected) in [
        ("H1", None, true),
        ("H2", Some(10), true),
        ("H3", Some(11), true),
        ("H4", Some(9), false),
    ] {
        let planned = historical_pending_projection_order(10, terminal, Some(u64::MAX));
        assert_eq!(planned.is_some(), expected, "{rule}");
        if let (Some(pending), Some(terminal)) = (planned, terminal) {
            let terminal = CanonicalEventOrder {
                source_commit_cursor: terminal,
                phase: LIFECYCLE_TERMINAL_PHASE,
                ordinal: 0,
                processed_at_unix_ms: Some(0),
            };
            assert!(pending < terminal, "{rule}");
        }
    }
}

#[cfg(test)]
mod prefix_comparison_tests {
    use super::*;
    use awaken_agent_contract::agent::{
        awaiting::{AwaitTarget, PauseReason, ResumeTicket},
        message::{Id as MessageId, Message, Role},
        run::{Id as RunId, Record, RunState},
        state::{Command, MergePolicy, Scope},
        thread::Id as ThreadId,
    };
    use awaken_agent_contract::thread::read::recovery::{RunRecoverySnapshot, RunResumeTicket};

    #[test]
    fn borrowed_prefix_comparison_covers_every_serialized_recovery_field() {
        // C1 either prefix absent; C2 any retained field differs; C3 only the
        // backend-wide store cursor advances. R1 both absent => same; R2 one
        // absent => changed; R3 each C2 dimension => changed, even at equal
        // versions; R4 C3 => same. Check the field inventory against Serialize
        // so adding a field cannot silently escape this decision table.
        let thread = ThreadId("thread".into());
        let run = RunId("run".into());
        let original = RunRecoverySnapshot {
            thread_id: thread.clone(),
            claimed_run_id: run.clone(),
            runs: vec![],
            latest_run_id: None,
            messages: vec![],
            message_commit_cursors: vec![],
            commit_timestamps: [(1, 1)].into(),
            state: vec![],
            state_commit_cursors: vec![],
            events: vec![],
            resume_tickets: vec![],
            thread_version: 1,
            store_cursor: 1,
            next_commit_ordinal: 0,
        };
        let fields = serde_json::json!({
            "thread_id":"other", "claimed_run_id":"other",
            "runs":[Record { id: run.clone(), thread_id: thread.clone(), state: RunState::Running }],
            "latest_run_id":"run",
            "messages":[Message::text(MessageId("message".into()), Role::User, "value")],
            "message_commit_cursors":[1], "commit_timestamps":{"1":2},
            "state":[Command::set(Scope::Run, MergePolicy::Disjoint, "key", serde_json::json!(1))],
            "state_commit_cursors":[1],
            "events":[awaken_agent_contract::audit::record::Record {
                sequence: 1, run_id: run.clone(),
                kind: awaken_agent_contract::audit::kind::Kind::PermissionDecided,
                payload: serde_json::json!({}),
            }],
            "resume_tickets":[RunResumeTicket { run_id: run.clone(), ticket: ResumeTicket::new(
                "correlation", run.clone(), thread.clone(), "snapshot", "catalog",
                AwaitTarget::Pause(PauseReason::Manual),
            )}],
            "thread_version":2, "store_cursor":2, "next_commit_ordinal":1,
        });
        let baseline = serde_json::to_value(&original).unwrap();
        assert_eq!(
            baseline
                .as_object()
                .unwrap()
                .keys()
                .collect::<std::collections::BTreeSet<_>>(),
            fields
                .as_object()
                .unwrap()
                .keys()
                .collect::<std::collections::BTreeSet<_>>()
        );
        assert!(same_thread_projection_prefix(None, None));
        assert!(!same_thread_projection_prefix(Some(&original), None));
        assert!(!same_thread_projection_prefix(None, Some(&original)));
        assert!(same_thread_projection_prefix(
            Some(&original),
            Some(&original)
        ));
        for (field, value) in fields.as_object().unwrap() {
            let mut changed = baseline.clone();
            changed[field] = value.clone();
            let changed: RunRecoverySnapshot = serde_json::from_value(changed).unwrap();
            let expected = field == "store_cursor";
            assert_eq!(
                same_thread_projection_prefix(Some(&original), Some(&changed)),
                expected,
                "{field}"
            );
            assert_eq!(
                same_thread_projection_prefix(Some(&changed), Some(&original)),
                expected,
                "{field} reverse"
            );
        }
    }
}

#[cfg(test)]
mod overlay_anchor_tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    fn event(id: impl Into<String>) -> Event {
        Event {
            id: id.into(),
            kind: OutboundKind::AgentThinking {},
            processed_at: None,
        }
    }
    #[test]
    fn one_pass_overlay_assignment_preserves_explicit_and_fallback_predecessors() {
        // C1 explicit hint names a retained durable event; C2 hint is missing
        // or no longer retained; C3 a prior retained durable event exists.
        // R1 C1 => explicit hint wins, including a later retained event.
        // R2 !C1+C2+C3 => nearest prior durable; R3 !C1+!C3 => head group.
        // Empty history and a run of 1000 leading overlays consume exactly one
        // input traversal; order within each anchor group remains issued order.
        let events = [
            event("evt_0"),
            event("durable-a"),
            event("evt_1"),
            event("evt_2"),
            event("durable-b"),
            event("evt_3"),
        ];
        let durable = HashSet::from(["durable-a", "durable-b"]);
        let hints = HashMap::from([
            ("evt_1".into(), "durable-b".into()),
            ("evt_2".into(), "gone".into()),
            ("evt_3".into(), "durable-a".into()),
        ]);
        let result = group_live_overlays(events.iter(), &durable, &hints);
        let ids = |key: Option<&str>| {
            result
                .get(&key.map(str::to_owned))
                .unwrap()
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(None), ["evt_0"]);
        assert_eq!(ids(Some("durable-a")), ["evt_2", "evt_3"]);
        assert_eq!(ids(Some("durable-b")), ["evt_1"]);
        for count in [0, 1, 1000] {
            let events = (0..count)
                .map(|i| event(format!("evt_{i}")))
                .collect::<Vec<_>>();
            let inspected = std::cell::Cell::new(0);
            let result = group_live_overlays(
                events
                    .iter()
                    .inspect(|_| inspected.set(inspected.get() + 1)),
                &HashSet::new(),
                &HashMap::new(),
            );
            assert_eq!(inspected.get(), count);
            assert_eq!(result.get(&None).map(Vec::len).unwrap_or_default(), count);
            if count > 0 {
                assert_eq!(result[&None], events);
            }
        }
    }
}
