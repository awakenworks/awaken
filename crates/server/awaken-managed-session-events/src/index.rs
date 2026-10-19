// @formal entrypoint session.runtime_projection_historical_pending_precedes_terminal_projection
// @formal entrypoint session.runtime_projection_success_advances_result_and_checkpoint_to_the_observed_source_prefix
// @formal entrypoint session_time.event_order_is_independent_of_wall_time
// @formal evidence session_event_batch.idempotent_receipt_is_admission_stable
//! The canonical order of every fact, resolved from its key over one prefix.
//!
//! The order of a fact is a function of its key and the committed evidence:
//! a lifecycle role sits at its lifecycle commit, a transcript fold at its
//! Message's commit, and an Outcome evaluation at the terminal report commit
//! that first makes its span listable. Its public time still comes from the
//! evaluation command. The protocol projector used to rebuild every candidate public id from
//! the evidence and look its events up by hash; resolving the key directly
//! removes that second spelling of each fact's identity.
//!
//! Consumer: the protocol adapter's canonical sort of one candidate.

#[cfg(test)]
mod terminal_time_tests;
mod tool_results;
#[cfg(test)]
pub(crate) use tool_results::snapshot;

use std::collections::{HashMap, HashSet};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot;
use awaken_agent_contract::{RunLifecycleCursor, RunLifecycleEvent, RunLifecycleEventKind};
use awaken_session_application_contract::Pending;
use awaken_session_event_contract::SessionAgentRunBoundary;
use awaken_session_event_contract::SessionEventBatch;
use awaken_session_execution_contract::{CommittedOutcomeProjection, CoordinatedThreadLink};
use awaken_session_state_contract::{
    PersistedSession, SessionExecutionState, SessionRuntimeInterval,
};

use crate::interval::{budget_reach_projection_close_cursor, interval_close_cursor, is_opening};
use crate::order::historical_pending_projection_order;
use crate::role::{TranscriptRole, phase};
use crate::transcript::{
    CommittedTranscriptEvidence, assistant_response_coordinates, committed_transcript_evidence,
};
use crate::{
    AnchoredOutcome, EventIdentity, EventPosition, FactProvenance, ManagedEventType, Ordinal,
    ProjectionError,
};

/// The committed evidence one candidate is ordered over, all borrowed.
///
/// Consumer: [`OrderIndex::new`].
#[derive(Clone, Copy)]
pub struct OrderEvidence<'a> {
    pub session_id: &'a str,
    /// Retained inbound batches, in root-revision order.
    pub updates: Option<&'a awaken_session_state_contract::SessionUpdateHistory>,
    pub batches: &'a [SessionEventBatch],
    /// Closed Session runtime intervals, in root-revision order.
    pub closed_intervals: &'a [SessionRuntimeInterval],
    /// Every budget-reach transition, in transition order.
    pub budget_reach: &'a [BudgetReachAnchor],
    pub parent_terminal: ParentTerminalEvidence,
    /// The accepted lifecycle prefix of the candidate.
    pub lifecycle: &'a [RunLifecycleEvent],
    pub root: Option<&'a RunRecoverySnapshot>,
    pub children: &'a HashMap<String, RunRecoverySnapshot>,
    pub links: &'a [CoordinatedThreadLink],
    pub outcomes: &'a [AnchoredOutcome<'a>],
    pub historical_pending: &'a HashMap<RunLifecycleCursor, Pending>,
}

/// One budget-reach transition and the commit of the pause that exposes it,
/// when its owning interval has closed.
///
/// Consumer: the order of budget-reach usage/idle facts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BudgetReachAnchor {
    pub generation: u64,
    pub close_cursor: Option<u64>,
}

impl BudgetReachAnchor {
    /// Anchor every budget-reach transition of `persisted` over `lifecycle`.
    #[must_use]
    pub fn of(
        persisted: &PersistedSession,
        lifecycle: &[RunLifecycleEvent],
        historical_pending: &HashMap<RunLifecycleCursor, Pending>,
    ) -> Vec<Self> {
        persisted
            .budget
            .reach_transitions()
            .iter()
            .map(|transition| Self {
                generation: transition.generation,
                close_cursor: budget_reach_projection_close_cursor(
                    persisted,
                    transition,
                    lifecycle,
                    historical_pending,
                ),
            })
            .collect()
    }
}

/// The Session-root facts that place the absorbing parent terminal.
///
/// Consumer: the order of activation-failure and parent-terminal facts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ParentTerminalEvidence {
    /// The Runtime commit of terminal cleanup, once one exists.
    pub cleanup_commit_cursor: Option<u64>,
    /// Initial activation failed durably.
    pub activation_failed: bool,
    /// When the activation failure was recorded.
    pub failed_at_unix_ms: Option<u64>,
    /// When the Session was archived.
    pub archived_at_unix_ms: Option<u64>,
}

impl ParentTerminalEvidence {
    #[must_use]
    pub fn of(persisted: &PersistedSession) -> Self {
        Self {
            cleanup_commit_cursor: persisted.terminal_cleanup.runtime_commit_cursor(),
            activation_failed: persisted.execution == SessionExecutionState::ActivationFailed,
            failed_at_unix_ms: persisted.realization_progress().failed_at_unix_ms(),
            archived_at_unix_ms: persisted
                .archived_at()
                .and_then(awaken_session_application_contract::rfc3339_to_epoch_millis),
        }
    }
}

/// Per-Thread lookups over one committed prefix.
struct ThreadIndex<'a> {
    snapshot: &'a RunRecoverySnapshot,
    /// Only a prefix whose Messages carry their commit cursors can order them.
    aligned: bool,
    messages_by_id: HashMap<&'a str, Vec<usize>>,
    messages_by_response: HashMap<(String, usize, usize), Vec<usize>>,
    transcript: CommittedTranscriptEvidence,
}

impl<'a> ThreadIndex<'a> {
    fn new(snapshot: &'a RunRecoverySnapshot) -> Self {
        let aligned = snapshot.message_commit_cursors.len() == snapshot.messages.len();
        let mut messages_by_id = HashMap::<&str, Vec<usize>>::new();
        for (index, message) in snapshot.messages.iter().enumerate() {
            messages_by_id.entry(&message.id.0).or_default().push(index);
        }
        let run_ids = snapshot
            .runs
            .iter()
            .map(|run| run.id.clone())
            .collect::<Vec<_>>();
        let coordinates = assistant_response_coordinates(&snapshot.messages, &run_ids);
        let mut messages_by_response = HashMap::<(String, usize, usize), Vec<usize>>::new();
        for (index, message) in snapshot.messages.iter().enumerate() {
            if let Some(coordinate) = coordinates.get(&message.id.0) {
                messages_by_response
                    .entry(coordinate.clone())
                    .or_default()
                    .push(index);
            }
        }
        Self {
            snapshot,
            aligned,
            messages_by_id,
            messages_by_response,
            transcript: committed_transcript_evidence(&snapshot.messages),
        }
    }

    /// A Message's facts sit at its index in the Thread, then at their own
    /// position inside the Message's fold.
    fn message_order(&self, index: usize, ordinal: usize) -> Option<EventPosition> {
        let cursor = *self.snapshot.message_commit_cursors.get(index)?;
        Some(EventPosition {
            processed_at_unix_ms: self.snapshot.commit_timestamps.get(&cursor).copied(),
            source_commit_cursor: cursor,
            phase: phase::TRANSCRIPT,
            ordinal: Ordinal::within(index, ordinal),
        })
    }
}

/// Maximum number of wire events that the canonical one-Message encoder can
/// emit. An Assistant message contributes at most one thinking marker, one
/// visible message, and one tool event per content block; every other role
/// contributes no more than one event per block.
#[must_use]
pub fn message_projection_ordinal_bound(
    message: &awaken_agent_contract::agent::message::Message,
) -> usize {
    message.content.len().saturating_add(2)
}

/// Exact commit boundary of the classifier-owned report messages for one Run.
/// The vector alignment is part of the recovery contract; an older snapshot
/// without cursors must withhold the derived cross-Thread event.
#[must_use]
pub fn report_commit_cursor(
    snapshot: &RunRecoverySnapshot,
    run_id: &awaken_agent_contract::agent::run::Id,
) -> Option<u64> {
    if snapshot.messages.len() != snapshot.message_commit_cursors.len() {
        return None;
    }
    let report_ids =
        awaken_session_execution_contract::session_agent_report_messages(&snapshot.messages, run_id)
            .into_iter()
            .map(|message| message.id.0.as_str())
            .collect::<HashSet<_>>();
    snapshot
        .messages
        .iter()
        .zip(snapshot.message_commit_cursors.iter().copied())
        .filter(|(message, _)| report_ids.contains(message.id.0.as_str()))
        .map(|(_, cursor)| cursor)
        .max()
}

/// The resolved canonical order of one candidate's facts.
pub struct OrderIndex<'a> {
    evidence: OrderEvidence<'a>,
    lifecycle_by_cursor: HashMap<RunLifecycleCursor, &'a RunLifecycleEvent>,
    threads: HashMap<&'a str, ThreadIndex<'a>>,
    /// Answerable pause events keyed by (Thread, Run, call).
    pending_orders: HashMap<(String, String, String), EventPosition>,
}

impl<'a> OrderIndex<'a> {
    #[must_use]
    pub fn new(evidence: OrderEvidence<'a>) -> Self {
        let lifecycle_by_cursor = evidence
            .lifecycle
            .iter()
            .map(|event| (event.cursor, event))
            .collect::<HashMap<_, _>>();
        let mut threads = HashMap::new();
        if let Some(root) = evidence.root {
            threads.insert(evidence.session_id, ThreadIndex::new(root));
        }
        for (thread_id, snapshot) in evidence.children {
            threads.insert(thread_id.as_str(), ThreadIndex::new(snapshot));
        }
        let mut index = Self {
            evidence,
            lifecycle_by_cursor,
            threads,
            pending_orders: HashMap::new(),
        };
        index.pending_orders = index.index_pending_orders();
        index
    }

    /// Commit wall time of one Thread commit, when its prefix retained it.
    #[must_use]
    pub fn time_for(&self, thread_id: &str, cursor: u64) -> Option<u64> {
        self.threads
            .get(thread_id)
            .and_then(|thread| thread.snapshot.commit_timestamps.get(&cursor).copied())
    }

    /// The exact occurrence classifier already built for this Thread's
    /// committed snapshot. Coordination reuses it instead of refolding the
    /// same ToolUse/ToolResult prefix behind the ordering index.
    /// Every accepted Awaiting target of this prefix that needs its
    /// synthetic tool (see [`crate::synthetic_awaiting_tools`]).
    pub fn synthetic_awaiting_tools(
        &self,
    ) -> Result<Vec<crate::SyntheticAwaitingTool>, crate::ProjectionError> {
        crate::synthetic_awaiting_tools(
            self.evidence.lifecycle,
            self.evidence.historical_pending,
            |thread_id| self.threads.get(thread_id).map(|thread| thread.snapshot),
        )
    }

    pub fn transcript_evidence(&self, thread_id: &str) -> Option<&CommittedTranscriptEvidence> {
        self.threads.get(thread_id).map(|thread| &thread.transcript)
    }

    pub(crate) fn permission_snapshots(&self) -> impl Iterator<Item = &RunRecoverySnapshot> {
        self.threads.values().map(|thread| thread.snapshot)
    }

    pub(crate) fn permission_call_sources(&self, thread_id: &str, call_id: &str) -> &[usize] {
        self.transcript_evidence(thread_id)
            .map_or(&[], |transcript| transcript.call_message_indices(call_id))
    }

    fn retain_earliest<K: std::hash::Hash + Eq>(
        orders: &mut HashMap<K, EventPosition>,
        key: K,
        order: EventPosition,
    ) {
        orders
            .entry(key)
            .and_modify(|current| *current = (*current).min(order))
            .or_insert(order);
    }

    // Synthetic pending-tool cause/effect table: C1 an accepted Awaiting
    // lifecycle fact has its exact committed audit target; C2 no matching
    // ToolUse Message has committed yet. C1+C2 orders the one answerable event
    // at that Awaiting fact. A later transcript-owned occurrence has a
    // message-qualified id and keeps its own Message cursor. A current ticket
    // cannot re-anchor a historical call to an earlier unaudited wait.
    fn index_pending_orders(&self) -> HashMap<(String, String, String), EventPosition> {
        let lifecycle = self.evidence.lifecycle;
        let mut orders = HashMap::new();
        for (cursor, pending) in self.evidence.historical_pending {
            let Some(awaiting) = lifecycle.iter().find(|event| {
                event.cursor == *cursor && event.kind == RunLifecycleEventKind::Awaiting
            }) else {
                continue;
            };
            let next_terminal_source_commit_cursor = self
                .next_run_boundary(awaiting)
                .map(|event| event.source_commit_cursor);
            if let Some(order) = historical_pending_projection_order(
                awaiting.source_commit_cursor,
                next_terminal_source_commit_cursor,
                self.time_for(&awaiting.thread_id.0, awaiting.source_commit_cursor),
            ) {
                Self::retain_earliest(
                    &mut orders,
                    (
                        awaiting.thread_id.0.clone(),
                        awaiting.run_id.0.clone(),
                        pending.tool_use_id.clone(),
                    ),
                    order,
                );
            }
        }
        orders
    }

    fn next_run_boundary(&self, awaiting: &RunLifecycleEvent) -> Option<&RunLifecycleEvent> {
        self.evidence
            .lifecycle
            .iter()
            .filter(|event| {
                event.thread_id == awaiting.thread_id
                    && event.run_id == awaiting.run_id
                    && event.cursor > awaiting.cursor
                    && SessionAgentRunBoundary::is_boundary(event.kind)
            })
            .min_by_key(|event| event.cursor)
    }

    /// Every accepted inbound command, at its immutable admission fence.
    /// Admission order within the retained batches breaks ties; an entry
    /// without a fence is not public yet.
    pub fn inbound_orders(&self) -> impl Iterator<Item = (&'a str, EventPosition)> + '_ {
        self.evidence
            .batches
            .iter()
            .enumerate()
            .filter_map(move |(batch_index, batch)| {
                let anchor = batch.admission_projection_anchor?;
                let root_rank =
                    batch_index.checked_add(self.evidence.updates.map_or(0, |history| {
                        history
                            .facts()
                            .iter()
                            .filter(|fact| fact.admitted_batch_count() <= batch_index as u64)
                            .count()
                    }))?;
                Some(
                    batch
                        .events
                        .iter()
                        .enumerate()
                        .map(move |(ordinal, entry)| {
                            (
                                entry.event.operation_id(),
                                EventPosition {
                                    processed_at_unix_ms: entry.processed_at_unix_ms,
                                    source_commit_cursor: anchor.source_commit_cursor,
                                    phase: phase::INBOUND_ADMISSION,
                                    ordinal: Ordinal::within(root_rank, ordinal),
                                },
                            )
                        }),
                )
            })
            .flatten()
    }

    /// Order of the tool-use or tool-result block of `call_id` inside one
    /// committed Message of `thread_id`.
    pub fn message_call_order(
        &self,
        thread_id: &str,
        message_id: &str,
        call_id: &str,
    ) -> Result<Option<EventPosition>, ProjectionError> {
        let Some(thread) = self.threads.get(thread_id) else {
            return Ok(None);
        };
        if !thread.aligned {
            return Err(ProjectionError::corrupt(
                "tool source prefix has misaligned Message coordinates",
            ));
        }
        // Exact replay retains its FIRST committed occurrence, just like the
        // transcript/source index; a later replay is not an alternate proof.
        let Some(&position) = thread
            .messages_by_id
            .get(message_id)
            .and_then(|positions| positions.first())
        else {
            return Ok(None);
        };
        let message = &thread.snapshot.messages[position];
        let Some(block) = message.content.iter().position(|block| match block {
            ContentBlock::ToolUse { id, .. } => id == call_id,
            ContentBlock::ToolResult { tool_use_id, .. } => tool_use_id == call_id,
            _ => false,
        }) else {
            return Ok(None);
        };
        let mut order = thread.message_order(position, block).ok_or_else(|| {
            ProjectionError::corrupt("tool source has no exact committed Message coordinate")
        })?;
        if let ContentBlock::ToolUse { id, .. } = &message.content[block] {
            // Visibility and identity use the same exact Awaiting proof. A
            // corrupt source is an error, never a missing-anchor fallback.
            let awaiting = self
                .message_pending_sources(thread, message, position)?
                .into_iter()
                .filter(|(source, pending, _)| {
                    source.source_id == message.id.0 && pending.tool_use_id == *id
                })
                .map(|(_, _, cursor)| cursor)
                .min();
            let result = thread
                .transcript
                .result_message_index(message_id, block, call_id)
                .and_then(|result| thread.snapshot.message_commit_cursors.get(result).copied());
            let visible_cursor = awaiting.into_iter().chain(result).min();
            if let Some(cursor) =
                visible_cursor.filter(|cursor| *cursor > order.source_commit_cursor)
            {
                order.source_commit_cursor = cursor;
                order.processed_at_unix_ms =
                    thread.snapshot.commit_timestamps.get(&cursor).copied();
            }
        }
        Ok(Some(order))
    }

    /// Order of a tool-use event qualified by (Thread, source, call): its
    /// source Message's block, or the Awaiting commit of a synthetic pause
    /// whose source is the Run.
    pub fn tool_order(
        &self,
        thread_id: &str,
        source_id: &str,
        call_id: &str,
    ) -> Result<Option<EventPosition>, ProjectionError> {
        let from_message = self.message_call_order(thread_id, source_id, call_id)?;
        let from_pause = self
            .pending_orders
            .get(&(
                thread_id.to_string(),
                source_id.to_string(),
                call_id.to_string(),
            ))
            .copied();
        Ok(from_message.into_iter().chain(from_pause).min())
    }

    /// The Run operation that issued one tool call of a committed assistant
    /// response, when that response carries Run coordinates.
    #[must_use]
    pub fn tool_operation(
        &self,
        thread_id: &str,
        source_id: &str,
        call_id: &str,
    ) -> Option<String> {
        let thread = self.threads.get(thread_id)?;
        if !thread.aligned {
            return None;
        }
        let positions = thread.messages_by_id.get(source_id)?;
        if positions.len() != 1 {
            return None;
        }
        let message = thread.snapshot.messages.get(positions[0])?;
        awaken_tool_contract::ActiveToolBatch::operation_for_message(
            thread.snapshot,
            message,
            call_id,
        )
        .ok()
        .flatten()
    }

    /// Order of every committed Advisor consultation call, by its operation.
    #[must_use]
    pub fn advisor_operation_orders(&self) -> HashMap<String, EventPosition> {
        let mut orders = HashMap::new();
        for (thread_id, thread) in &self.threads {
            if !thread.aligned {
                continue;
            }
            let snapshot = thread.snapshot;
            for (message_index, (message, source_commit_cursor)) in snapshot
                .messages
                .iter()
                .zip(snapshot.message_commit_cursors.iter().copied())
                .enumerate()
            {
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
                    let Some(operation_id) = self.tool_operation(thread_id, &message.id.0, call_id)
                    else {
                        continue;
                    };
                    Self::retain_earliest(
                        &mut orders,
                        operation_id,
                        EventPosition {
                            processed_at_unix_ms: snapshot
                                .commit_timestamps
                                .get(&source_commit_cursor)
                                .copied(),
                            source_commit_cursor,
                            phase: phase::TRANSCRIPT,
                            ordinal: Ordinal::within(message_index, block_ordinal),
                        },
                    );
                }
            }
        }
        orders
    }

    /// Where the report of one coordinated Run's terminal becomes public: the
    /// later of its report Messages' commit and the terminal commit.
    #[must_use]
    pub fn report_order(&self, terminal: &RunLifecycleEvent) -> Option<EventPosition> {
        let snapshot = self.evidence.children.get(&terminal.thread_id.0)?;
        let report = report_commit_cursor(snapshot, &terminal.run_id)?;
        Some(EventPosition {
            processed_at_unix_ms: self
                .time_for(&terminal.thread_id.0, terminal.source_commit_cursor),
            source_commit_cursor: report.max(terminal.source_commit_cursor),
            phase: ManagedEventType::ChildMessageReceived.phase(),
            ordinal: Ordinal::FIRST,
        })
    }

    fn lifecycle_order(&self, event: &RunLifecycleEvent, role: ManagedEventType) -> EventPosition {
        EventPosition {
            source_commit_cursor: event.source_commit_cursor,
            processed_at_unix_ms: self.time_for(&event.thread_id.0, event.source_commit_cursor),
            phase: role.phase(),
            ordinal: Ordinal::FIRST,
        }
    }

    /// Complete position of Root-owned update, interval and budget facts.
    /// Thread/lifecycle fact selection never resamples their Root coordinates.
    fn root_fact_order(&self, key: &EventIdentity) -> Option<EventPosition> {
        let session_id = self.evidence.session_id;
        if key.thread != session_id {
            return None;
        }
        match (&key.provenance, key.role) {
            (FactProvenance::SessionUpdate { revision }, ManagedEventType::SessionUpdated) => {
                let history = self.evidence.updates?;
                let (index, fact) = history
                    .facts()
                    .iter()
                    .enumerate()
                    .find(|(_, fact)| fact.committed_revision().0 == *revision)?;
                let source = usize::try_from(fact.admitted_batch_count())
                    .ok()?
                    .checked_add(index)?;
                Some(EventPosition {
                    source_commit_cursor: fact.observed_runtime().source_commit_cursor,
                    phase: phase::INBOUND_ADMISSION,
                    ordinal: Ordinal::at(source),
                    processed_at_unix_ms: fact.recorded_at_unix_ms(),
                })
            }
            (
                FactProvenance::RuntimeInterval { interval_id },
                ManagedEventType::AggregateUsage | ManagedEventType::AggregateStatusIdle,
            ) => self
                .evidence
                .closed_intervals
                .iter()
                .filter(|interval| &interval.interval_id == interval_id)
                .filter_map(|interval| {
                    Some(EventPosition {
                        processed_at_unix_ms: Some(interval.ended_at_unix_ms),
                        source_commit_cursor: interval_close_cursor(interval)?,
                        phase: key.role.phase(),
                        ordinal: Ordinal::FIRST,
                    })
                })
                .min(),
            (
                FactProvenance::BudgetReach { generation },
                ManagedEventType::BudgetReachUsage | ManagedEventType::BudgetReachStatusIdle,
            ) => self
                .evidence
                .budget_reach
                .iter()
                .enumerate()
                .filter(|(_, anchor)| anchor.generation == *generation)
                .filter_map(|(index, anchor)| {
                    let source_commit_cursor = anchor.close_cursor?;
                    Some(EventPosition {
                        processed_at_unix_ms: self.time_for(session_id, source_commit_cursor),
                        source_commit_cursor,
                        phase: key.role.phase(),
                        ordinal: Ordinal::at(index),
                    })
                })
                .min(),
            _ => None,
        }
    }

    /// The order of one keyed fact, excluding coordination facts (which the
    /// caller orders from its accepted coordination receipts) and parent
    /// terminal facts (see [`Self::parent_terminal_order`]). `None` means the
    /// evidence does not anchor the fact, so the candidate must fail closed.
    #[must_use]
    pub fn fact_order(&self, key: &EventIdentity) -> Option<EventPosition> {
        let session_id = self.evidence.session_id;
        match (&key.provenance, key.role) {
            (
                FactProvenance::SessionUpdate { .. }
                | FactProvenance::RuntimeInterval { .. }
                | FactProvenance::BudgetReach { .. },
                _,
            ) => self.root_fact_order(key),

            (FactProvenance::Lifecycle { cursor }, ManagedEventType::AggregateStatusRunning) => {
                let event = self.lifecycle_by_cursor.get(&RunLifecycleCursor(*cursor))?;
                (key.thread == session_id && is_opening(event.kind))
                    .then(|| self.lifecycle_order(event, key.role))
            }
            (FactProvenance::Lifecycle { cursor }, role)
                if ManagedEventType::LIFECYCLE.contains(&role) =>
            {
                let event = self.lifecycle_by_cursor.get(&RunLifecycleCursor(*cursor))?;
                (event.thread_id.0 == key.thread).then(|| self.lifecycle_order(event, role))
            }
            (
                FactProvenance::LifecyclePrefix { cursor },
                ManagedEventType::AggregateTerminalStatusRunning,
            ) => {
                let event = self.lifecycle_by_cursor.get(&RunLifecycleCursor(*cursor))?;
                (key.thread == session_id
                    && event.thread_id.0 == session_id
                    && SessionAgentRunBoundary::is_boundary(event.kind))
                .then(|| self.lifecycle_order(event, key.role))
            }
            (
                FactProvenance::LifecyclePrefix { cursor },
                ManagedEventType::AggregateUsage | ManagedEventType::AggregateStatusIdle,
            ) => {
                let event = self.lifecycle_by_cursor.get(&RunLifecycleCursor(*cursor))?;
                (key.thread == session_id).then(|| self.lifecycle_order(event, key.role))
            }
            (
                FactProvenance::Audit { run_id, sequence },
                ManagedEventType::ModelRequestStart | ManagedEventType::ModelRequestEnd,
            ) => {
                let snapshot = self.threads.get(key.thread.as_str())?.snapshot;
                snapshot
                    .events
                    .iter()
                    .any(|audit| audit.run_id.0 == *run_id && audit.sequence == *sequence)
                    .then(|| {
                        let source_commit_cursor =
                            awaken_agent_contract::decode_run_lifecycle_cursor(RunLifecycleCursor(
                                *sequence,
                            ))
                            .0;
                        EventPosition {
                            processed_at_unix_ms: snapshot
                                .commit_timestamps
                                .get(&source_commit_cursor)
                                .copied(),
                            source_commit_cursor,
                            phase: key.role.phase(),
                            ordinal: Ordinal::at(usize::try_from(*sequence).unwrap_or(usize::MAX)),
                        }
                    })
            }
            (FactProvenance::RunState { run_id }, ManagedEventType::RunContextCompacted) => {
                let snapshot = self.threads.get(key.thread.as_str())?.snapshot;
                if !snapshot.runs.iter().any(|run| run.id.0 == *run_id) {
                    return None;
                }
                let source_commit_cursor =
                    awaken_runtime_contract::compaction::RunCompactionMarker::recorded_commit_cursor(
                        &snapshot.state,
                        &snapshot.state_commit_cursors,
                        run_id,
                    )?;
                Some(EventPosition {
                    processed_at_unix_ms: snapshot
                        .commit_timestamps
                        .get(&source_commit_cursor)
                        .copied(),
                    source_commit_cursor,
                    phase: key.role.phase(),
                    ordinal: Ordinal::FIRST,
                })
            }
            (FactProvenance::ArchivedDisposition, ManagedEventType::ArchivedStatusTerminated) => {
                let snapshot = self.threads.get(key.thread.as_str())?.snapshot;
                let source_commit_cursor = awaken_agent_contract::archived_thread_commit_cursor(
                    &snapshot.state,
                    &snapshot.state_commit_cursors,
                )?;
                Some(EventPosition {
                    processed_at_unix_ms: snapshot
                        .commit_timestamps
                        .get(&source_commit_cursor)
                        .copied(),
                    source_commit_cursor,
                    phase: key.role.phase(),
                    ordinal: Ordinal::FIRST,
                })
            }
            (
                FactProvenance::OutcomeEvaluation {
                    outcome_id,
                    iteration,
                },
                ManagedEventType::OutcomeEvaluationStart
                | ManagedEventType::OutcomeEvaluationOngoing
                | ManagedEventType::OutcomeEvaluationEnd,
            ) => {
                if key.thread != session_id {
                    return None;
                }
                self.evidence
                    .outcomes
                    .iter()
                    .enumerate()
                    .filter(|(_, outcome)| outcome.outcome_id == outcome_id)
                    .filter_map(|(batch_index, outcome)| {
                        let CommittedOutcomeProjection::Completed(report) = outcome.terminal else {
                            return None;
                        };
                        let iteration_index = report
                            .iterations
                            .iter()
                            .position(|item| item.iteration == *iteration)?;
                        let evaluation_cursor = outcome.evaluation_commit_cursor(*iteration)?;
                        (evaluation_cursor <= outcome.terminal_commit_cursor).then_some(())?;
                        let role_offset = match key.role {
                            ManagedEventType::OutcomeEvaluationStart => 0,
                            ManagedEventType::OutcomeEvaluationOngoing => 1,
                            ManagedEventType::OutcomeEvaluationEnd => 2,
                            _ => return None,
                        };
                        let within = iteration_index.checked_mul(3)?.checked_add(role_offset)?;
                        Some(EventPosition {
                            processed_at_unix_ms: self.time_for(session_id, evaluation_cursor),
                            // The Completed report is required to materialize
                            // this trio. Sorting at an earlier evaluation
                            // command would insert it before issued cursors.
                            source_commit_cursor: outcome.terminal_commit_cursor,
                            phase: key.role.phase(),
                            ordinal: Ordinal::within(batch_index, within),
                        })
                    })
                    .min()
            }
            (FactProvenance::Outcome { outcome_id }, ManagedEventType::OutcomeError) => {
                if key.thread != session_id {
                    return None;
                }
                self.evidence
                    .outcomes
                    .iter()
                    .enumerate()
                    .filter(|(_, outcome)| {
                        outcome.outcome_id == outcome_id
                            && matches!(outcome.terminal, CommittedOutcomeProjection::Errored(_))
                    })
                    .map(|(batch_index, outcome)| EventPosition {
                        processed_at_unix_ms: self
                            .time_for(session_id, outcome.terminal_commit_cursor),
                        source_commit_cursor: outcome.terminal_commit_cursor,
                        phase: key.role.phase(),
                        ordinal: Ordinal::at(batch_index),
                    })
                    .min()
            }
            (
                FactProvenance::Message {
                    message_id,
                    ordinal,
                },
                ManagedEventType::Transcript(_),
            ) => {
                let thread = self.threads.get(key.thread.as_str())?;
                if !thread.aligned {
                    return None;
                }
                thread
                    .messages_by_id
                    .get(message_id.as_str())?
                    .iter()
                    .filter(|&&index| {
                        *ordinal
                            < message_projection_ordinal_bound(&thread.snapshot.messages[index])
                    })
                    .filter_map(|&index| thread.message_order(index, *ordinal))
                    .min()
            }
            (
                FactProvenance::AssistantResponse {
                    run_id,
                    step,
                    response,
                },
                ManagedEventType::Transcript(role),
            ) => {
                let thread = self.threads.get(key.thread.as_str())?;
                if !thread.aligned {
                    return None;
                }
                thread
                    .messages_by_response
                    .get(&(run_id.clone(), *step, *response))?
                    .iter()
                    .filter_map(|&index| {
                        let message = &thread.snapshot.messages[index];
                        // The visible message follows its thinking marker.
                        let ordinal = usize::from(
                            role == TranscriptRole::AgentMessage
                                && message
                                    .content
                                    .iter()
                                    .any(|block| matches!(block, ContentBlock::Thinking { .. })),
                        );
                        thread.message_order(index, ordinal)
                    })
                    .min()
            }
            _ => None,
        }
    }

    /// The highest source cursor any evidence-anchored fact of this prefix
    /// can occupy. An activation failure without a Runtime cleanup cursor is
    /// terminal after the complete immutable prefix already indexed.
    #[must_use]
    pub fn evidence_frontier(&self) -> u64 {
        let lifecycle = self
            .evidence
            .lifecycle
            .iter()
            .map(|event| event.source_commit_cursor);
        let inbound = self
            .inbound_orders()
            .map(|(_, order)| order.source_commit_cursor)
            .collect::<Vec<_>>();
        let pending = self
            .pending_orders
            .values()
            .map(|order| order.source_commit_cursor);
        let intervals = self
            .evidence
            .closed_intervals
            .iter()
            .filter_map(interval_close_cursor);
        let snapshot_facts = self.threads.values().flat_map(|thread| {
            let snapshot = thread.snapshot;
            let audits = snapshot.events.iter().map(|audit| {
                awaken_agent_contract::decode_run_lifecycle_cursor(RunLifecycleCursor(
                    audit.sequence,
                ))
                .0
            });
            let compactions = snapshot.runs.iter().filter_map(|run| {
                awaken_runtime_contract::compaction::RunCompactionMarker::recorded_commit_cursor(
                    &snapshot.state,
                    &snapshot.state_commit_cursors,
                    &run.id.0,
                )
            });
            let archived = awaken_agent_contract::archived_thread_commit_cursor(
                &snapshot.state,
                &snapshot.state_commit_cursors,
            );
            audits.chain(compactions).chain(archived)
        });
        let outcomes = self.evidence.outcomes.iter().flat_map(|outcome| {
            let evaluations = match outcome.terminal {
                CommittedOutcomeProjection::Completed(report) => report
                    .iterations
                    .iter()
                    .filter_map(|item| outcome.evaluation_commit_cursor(item.iteration))
                    .collect::<Vec<_>>(),
                CommittedOutcomeProjection::Errored(_) => vec![outcome.terminal_commit_cursor],
            };
            evaluations.into_iter()
        });
        let budget = self
            .evidence
            .budget_reach
            .iter()
            .filter_map(|anchor| anchor.close_cursor);
        let reports = self.evidence.links.iter().flat_map(|link| {
            self.evidence
                .lifecycle
                .iter()
                .filter(move |event| {
                    event.thread_id == link.thread_id
                        && matches!(
                            event.kind,
                            RunLifecycleEventKind::Completed
                                | RunLifecycleEventKind::Failed
                                | RunLifecycleEventKind::Cancelled
                        )
                })
                .filter_map(|terminal| self.report_order(terminal))
                .map(|order| order.source_commit_cursor)
        });
        lifecycle
            .chain(inbound)
            .chain(pending)
            .chain(intervals)
            .chain(snapshot_facts)
            .chain(outcomes)
            .chain(budget)
            .chain(reports)
            .max()
            .unwrap_or_default()
    }

    /// Where the absorbing parent terminal sits: the Runtime cleanup commit,
    /// or, for an activation failure without one, after `frontier` (the
    /// highest cursor of every fact already ordered in this candidate).
    #[must_use]
    pub fn parent_terminal_source(&self, frontier: u64) -> Option<u64> {
        let terminal = self.evidence.parent_terminal;
        terminal
            .cleanup_commit_cursor
            .or_else(|| terminal.activation_failed.then_some(frontier))
    }

    /// Order of a parent-terminal fact at `terminal_source`.
    #[must_use]
    pub fn parent_terminal_order(
        &self,
        key: &EventIdentity,
        terminal_source: u64,
    ) -> Option<EventPosition> {
        let session_id = self.evidence.session_id;
        let terminal = self.evidence.parent_terminal;
        if key.provenance != FactProvenance::ParentTerminal {
            return None;
        }
        match key.role {
            ManagedEventType::ActivationFailedError
                if key.thread == session_id && terminal.activation_failed =>
            {
                Some(EventPosition {
                    processed_at_unix_ms: terminal.failed_at_unix_ms,
                    source_commit_cursor: terminal_source,
                    phase: key.role.phase(),
                    ordinal: Ordinal::FIRST,
                })
            }
            ManagedEventType::ParentTerminalChildStatusTerminated
                if self
                    .evidence
                    .links
                    .iter()
                    .any(|link| link.thread_id.0 == key.thread) =>
            {
                Some(self.parent_terminal_status_order(key.role, terminal_source))
            }
            ManagedEventType::ParentTerminalPrimaryStatusTerminated
            | ManagedEventType::ParentTerminalSessionStatusTerminated
                if key.thread == session_id =>
            {
                Some(self.parent_terminal_status_order(key.role, terminal_source))
            }
            _ => None,
        }
    }

    fn parent_terminal_status_order(
        &self,
        role: ManagedEventType,
        terminal_source: u64,
    ) -> EventPosition {
        let terminal = self.evidence.parent_terminal;
        EventPosition {
            // Activation failure can publish these absorbing status facts
            // before release archives the root. Their existing failure owner
            // must remain stable in warm and cold projections, even when its
            // historical time is unknown; archive is a later distinct fact.
            processed_at_unix_ms: if terminal.activation_failed {
                terminal.failed_at_unix_ms
            } else {
                terminal
                    .archived_at_unix_ms
                    .or_else(|| self.time_for(self.evidence.session_id, terminal_source))
            },
            source_commit_cursor: terminal_source,
            phase: role.phase(),
            ordinal: Ordinal::FIRST,
        }
    }

    /// The lifecycle terminals (Completed/Failed/Cancelled) of each Thread.
    #[must_use]
    pub fn lifecycle(&self) -> &'a [RunLifecycleEvent] {
        self.evidence.lifecycle
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::agent::run::{EndCause, Id as RunId, Record, RunState};
    use awaken_agent_contract::agent::state::StateKey;
    use awaken_agent_contract::agent::thread::Id as ThreadId;
    use awaken_session_event_contract::{SessionEventInput, SessionEventProjectionAnchor};

    const SESSION: &str = "session";

    fn cursor(source: u64) -> RunLifecycleCursor {
        awaken_agent_contract::encode_run_lifecycle_cursor(source, 0).unwrap()
    }

    fn lifecycle(source: u64, thread: &str, kind: RunLifecycleEventKind) -> RunLifecycleEvent {
        RunLifecycleEvent {
            cursor: cursor(source),
            source_commit_cursor: source,
            thread_id: ThreadId(thread.into()),
            run_id: RunId("run".into()),
            kind,
            state: match kind {
                RunLifecycleEventKind::Awaiting => RunState::Awaiting,
                RunLifecycleEventKind::Completed => RunState::Ended(EndCause::NaturalEnd),
                _ => RunState::Running,
            },
            await_reason: None,
        }
    }

    fn key(thread: &str, role: ManagedEventType, provenance: FactProvenance) -> EventIdentity {
        EventIdentity::new(thread, role, provenance)
    }

    type Coordinate = (u64, u16, (usize, usize), Option<u64>);

    fn order(order: Option<EventPosition>) -> Option<Coordinate> {
        order.map(|order| {
            (
                order.source_commit_cursor,
                order.phase,
                (order.ordinal.source, order.ordinal.within),
                order.processed_at_unix_ms,
            )
        })
    }

    #[test]
    fn completed_outcome_releases_ordered_spans_only_at_its_terminal_commit() {
        // Cause/effect table: C1 a Completed report contains two ordered
        // iterations with evaluation commits 10/20 and terminal commit 30;
        // C2 each iteration has start/ongoing/end; C3 one evaluation cursor
        // lies beyond the terminal commit. R1 C1+C2 => all six facts release
        // at 30 in iteration/trio order, regardless of their earlier times.
        // R2 C1+C3 => no order for the inconsistent evaluation fact.
        let terminal =
            CommittedOutcomeProjection::Completed(awaken_session_execution_contract::OutcomeReport {
                iterations: [0, 1]
                    .map(
                        |iteration| awaken_session_execution_contract::OutcomeIteration {
                            messages: Vec::new(),
                            outcome_id: "outcome".into(),
                            description: "ship".into(),
                            iteration,
                            result: "satisfied".into(),
                            explanation: "reviewed".into(),
                            usage: None,
                        },
                    )
                    .into(),
            });
        let children = HashMap::new();
        let historical = HashMap::new();
        let evaluations = HashMap::from([(0, 10), (1, 20)]);
        let outcomes = [AnchoredOutcome {
            outcome_id: "outcome",
            terminal: &terminal,
            terminal_commit_cursor: 30,
            evaluation_commit_cursors: &evaluations,
        }];
        let evidence = |outcomes| OrderEvidence {
            updates: None,
            session_id: SESSION,
            batches: &[],
            closed_intervals: &[],
            budget_reach: &[],
            parent_terminal: ParentTerminalEvidence::default(),
            lifecycle: &[],
            root: None,
            children: &children,
            links: &[],
            outcomes,
            historical_pending: &historical,
        };
        let index = OrderIndex::new(evidence(&outcomes));
        for (iteration, first) in [(0, 0), (1, 3)] {
            for (role, offset) in [
                (ManagedEventType::OutcomeEvaluationStart, 0),
                (ManagedEventType::OutcomeEvaluationOngoing, 1),
                (ManagedEventType::OutcomeEvaluationEnd, 2),
            ] {
                assert_eq!(
                    order(index.fact_order(&key(
                        SESSION,
                        role,
                        FactProvenance::OutcomeEvaluation {
                            outcome_id: "outcome".into(),
                            iteration,
                        },
                    ))),
                    Some((30, 60, (0, first + offset), None)),
                    "R1 iteration={iteration} role={role:?}"
                );
            }
        }
        let inconsistent = HashMap::from([(0, 31), (1, 20)]);
        let malformed = [AnchoredOutcome {
            evaluation_commit_cursors: &inconsistent,
            ..outcomes[0]
        }];
        let index = OrderIndex::new(evidence(&malformed));
        assert_eq!(
            order(index.fact_order(&key(
                SESSION,
                ManagedEventType::OutcomeEvaluationStart,
                FactProvenance::OutcomeEvaluation {
                    outcome_id: "outcome".into(),
                    iteration: 0,
                },
            ))),
            None,
            "R2 evaluation after terminal fails closed"
        );
    }

    #[test]
    fn every_fact_family_resolves_to_its_committed_coordinate() {
        // Cause/effect table over one fixed prefix. Causes: the key's role,
        // provenance and owning Thread, and whether the evidence anchors it.
        // Effects: E1 an anchored key resolves to (source commit, role phase,
        // ordinal, commit time); E2 an unanchored or mismatched key resolves to
        // nothing, which fails the caller's candidate closed.
        let run = RunId("run".into());
        let with_thinking = Message::new(
            MessageId::assistant(&run, 0),
            Role::Assistant,
            vec![ContentBlock::thinking("plan"), ContentBlock::text("answer")],
        );
        let tool_use = Message::new(
            MessageId::assistant(&run, 1),
            Role::Assistant,
            vec![ContentBlock::tool_use(
                "call",
                "read",
                serde_json::json!({}),
            )],
        );
        let user = Message::text(MessageId("user".into()), Role::User, "go");
        let audit_sequence = cursor(4).0;
        let root = RunRecoverySnapshot {
            thread_id: ThreadId(SESSION.into()),
            claimed_run_id: run.clone(),
            runs: vec![Record {
                id: run.clone(),
                thread_id: ThreadId(SESSION.into()),
                state: RunState::Running,
            }],
            latest_run_id: Some(run.clone()),
            messages: vec![user.clone(), with_thinking.clone(), tool_use.clone()],
            message_commit_cursors: vec![1, 2, 3],
            commit_timestamps: [(2, 2_000)].into(),
            state: vec![],
            state_commit_cursors: vec![],
            events: vec![awaken_agent_contract::audit::record::Record {
                sequence: audit_sequence,
                run_id: run.clone(),
                kind: awaken_agent_contract::audit::kind::Kind::ModelRequestCompleted,
                payload: serde_json::json!({}),
            }],
            resume_tickets: vec![],
            thread_version: 1,
            store_cursor: 6,
            next_commit_ordinal: 0,
        };
        let lifecycle = vec![
            lifecycle(1, SESSION, RunLifecycleEventKind::Running),
            lifecycle(2, "child", RunLifecycleEventKind::Running),
            lifecycle(5, SESSION, RunLifecycleEventKind::Awaiting),
            lifecycle(6, SESSION, RunLifecycleEventKind::Completed),
        ];
        let mut batch = SessionEventBatch::compile(
            &awaken_session_types::SessionId::from(SESSION),
            "batch",
            vec![
                SessionEventInput::UserMessage {
                    content: vec![ContentBlock::text("one")],
                },
                SessionEventInput::UserMessage {
                    content: vec![ContentBlock::text("two")],
                },
            ],
        )
        .unwrap();
        batch.admission_projection_anchor = Some(SessionEventProjectionAnchor {
            source_commit_cursor: 1,
        });
        let interval: SessionRuntimeInterval = serde_json::from_value(serde_json::json!({
            "interval_id": "interval",
            "opened_revision": 1,
            "activity_epoch": 1,
            "started_at_unix_ms": 1,
            "ended_at_unix_ms": 99,
            "observations": [{
                "activity_epoch": 1,
                "thread_id": SESSION,
                "run_id": "run",
                "lifecycle_cursor": cursor(6).0,
                "source_commit_cursor": 6,
            }],
        }))
        .unwrap();
        let pending = Pending {
            tool_use_id: "ticket".into(),
            name: "read".into(),
            input: serde_json::json!({}),
            client_executed: false,
        };
        let children = HashMap::new();
        let historical = HashMap::from([(cursor(5), pending.clone())]);
        let budget = [
            BudgetReachAnchor {
                generation: 1,
                close_cursor: Some(6),
            },
            BudgetReachAnchor {
                generation: 2,
                close_cursor: None,
            },
        ];
        let links = [CoordinatedThreadLink {
            session_id: SESSION.into(),
            thread_id: ThreadId("child".into()),
            target: awaken_session_execution_contract::CoordinatedThreadTarget::Agent {
                agent_id: "researcher".into(),
            },
            created_by_operation_id: "operation".into(),
            latest_run_id: None,
        }];
        let index = OrderIndex::new(OrderEvidence {
            updates: None,
            session_id: SESSION,
            batches: std::slice::from_ref(&batch),
            closed_intervals: std::slice::from_ref(&interval),
            budget_reach: &budget,
            parent_terminal: ParentTerminalEvidence {
                cleanup_commit_cursor: Some(9),
                ..ParentTerminalEvidence::default()
            },
            lifecycle: &lifecycle,
            root: Some(&root),
            children: &children,
            links: &links,
            outcomes: &[],
            historical_pending: &historical,
        });
        let at = |source| FactProvenance::Lifecycle {
            cursor: cursor(source).0,
        };
        let prefix = |source| FactProvenance::LifecyclePrefix {
            cursor: cursor(source).0,
        };
        let rows = [
            (
                "L1 root role at its commit",
                key(SESSION, ManagedEventType::RootLifecycleStatusRunning, at(1)),
                Some((1, 20, (0, 0), None)),
            ),
            (
                "L2 role on another Thread",
                key("child", ManagedEventType::RootLifecycleStatusRunning, at(1)),
                None,
            ),
            (
                "L3 aggregate opening",
                key(SESSION, ManagedEventType::AggregateStatusRunning, at(1)),
                Some((1, 10, (0, 0), None)),
            ),
            (
                "L4 child opening opens the aggregate",
                key(SESSION, ManagedEventType::AggregateStatusRunning, at(2)),
                Some((2, 10, (0, 0), None)),
            ),
            (
                "L5 a terminal is no opening",
                key(SESSION, ManagedEventType::AggregateStatusRunning, at(6)),
                None,
            ),
            (
                "L6 aggregate owned by the root",
                key("child", ManagedEventType::AggregateStatusRunning, at(1)),
                None,
            ),
            (
                "L7 terminal-prefix opening",
                key(
                    SESSION,
                    ManagedEventType::AggregateTerminalStatusRunning,
                    prefix(6),
                ),
                Some((6, 10, (0, 0), None)),
            ),
            (
                "L8 terminal prefix needs a boundary",
                key(
                    SESSION,
                    ManagedEventType::AggregateTerminalStatusRunning,
                    prefix(1),
                ),
                None,
            ),
            (
                "L9 aggregate usage at any commit",
                key(SESSION, ManagedEventType::AggregateUsage, prefix(2)),
                Some((2, 90, (0, 0), None)),
            ),
            (
                "I1 interval close",
                key(
                    SESSION,
                    ManagedEventType::AggregateStatusIdle,
                    FactProvenance::RuntimeInterval {
                        interval_id: "interval".into(),
                    },
                ),
                Some((6, 100, (0, 0), Some(99))),
            ),
            (
                "I2 unknown interval",
                key(
                    SESSION,
                    ManagedEventType::AggregateStatusIdle,
                    FactProvenance::RuntimeInterval {
                        interval_id: "other".into(),
                    },
                ),
                None,
            ),
            (
                "B1 closed budget reach",
                key(
                    SESSION,
                    ManagedEventType::BudgetReachUsage,
                    FactProvenance::BudgetReach { generation: 1 },
                ),
                Some((6, 90, (0, 0), None)),
            ),
            (
                "B2 open budget reach",
                key(
                    SESSION,
                    ManagedEventType::BudgetReachStatusIdle,
                    FactProvenance::BudgetReach { generation: 2 },
                ),
                None,
            ),
            (
                "M1 Message fold",
                key(
                    SESSION,
                    ManagedEventType::Transcript(TranscriptRole::AgentMessage),
                    FactProvenance::Message {
                        message_id: "user".into(),
                        ordinal: 1,
                    },
                ),
                Some((1, 50, (0, 1), None)),
            ),
            (
                "M2 fold beyond the Message bound",
                key(
                    SESSION,
                    ManagedEventType::Transcript(TranscriptRole::AgentMessage),
                    FactProvenance::Message {
                        message_id: "user".into(),
                        ordinal: 3,
                    },
                ),
                None,
            ),
            (
                "M3 visible message follows thinking",
                key(
                    SESSION,
                    ManagedEventType::Transcript(TranscriptRole::AgentMessage),
                    FactProvenance::AssistantResponse {
                        run_id: "run".into(),
                        step: 0,
                        response: 0,
                    },
                ),
                Some((2, 50, (1, 1), Some(2_000))),
            ),
            (
                "M4 thinking marker first",
                key(
                    SESSION,
                    ManagedEventType::Transcript(TranscriptRole::AgentThinking),
                    FactProvenance::AssistantResponse {
                        run_id: "run".into(),
                        step: 0,
                        response: 0,
                    },
                ),
                Some((2, 50, (1, 0), Some(2_000))),
            ),
            (
                "A1 model request",
                key(
                    SESSION,
                    ManagedEventType::ModelRequestStart,
                    FactProvenance::Audit {
                        run_id: "run".into(),
                        sequence: audit_sequence,
                    },
                ),
                Some((4, 40, (usize::try_from(audit_sequence).unwrap(), 0), None)),
            ),
            (
                "A2 unknown audit row",
                key(
                    SESSION,
                    ManagedEventType::ModelRequestEnd,
                    FactProvenance::Audit {
                        run_id: "other".into(),
                        sequence: audit_sequence,
                    },
                ),
                None,
            ),
            (
                "X1 role/provenance mismatch",
                key(SESSION, ManagedEventType::OutcomeError, at(1)),
                None,
            ),
        ];
        for (rule, key, expected) in rows {
            assert_eq!(order(index.fact_order(&key)), expected, "{rule}");
        }
        assert_eq!(
            order(index.tool_order(SESSION, &tool_use.id.0, "call").unwrap()),
            Some((3, 50, (2, 0), None)),
            "T1 tool use at its Message block"
        );
        assert_eq!(
            order(index.tool_order(SESSION, "run", "ticket").unwrap()),
            Some((5, 50, (0, 0), None)),
            "T2 audited pause at its own Awaiting commit"
        );
        assert_eq!(
            order(index.tool_order(SESSION, "run", "other").unwrap()),
            None,
            "T3 no Message and no pause"
        );
        assert_eq!(
            index
                .inbound_orders()
                .map(|(_, order)| (order.source_commit_cursor, order.phase, order.ordinal))
                .collect::<Vec<_>>(),
            vec![
                (1, 120, Ordinal::within(0, 0)),
                (1, 120, Ordinal::within(0, 1))
            ],
            "N1 admission fence and admission order"
        );
        let terminal_source = index.parent_terminal_source(0);
        assert_eq!(terminal_source, Some(9), "P1 cleanup commit wins");
        for (rule, thread, role, expected) in [
            (
                "P2 session terminal",
                SESSION,
                ManagedEventType::ParentTerminalSessionStatusTerminated,
                true,
            ),
            (
                "P3 linked child terminal",
                "child",
                ManagedEventType::ParentTerminalChildStatusTerminated,
                true,
            ),
            (
                "P4 unlinked child",
                "stray",
                ManagedEventType::ParentTerminalChildStatusTerminated,
                false,
            ),
            (
                "P5 no activation failure",
                SESSION,
                ManagedEventType::ActivationFailedError,
                false,
            ),
        ] {
            assert_eq!(
                index
                    .parent_terminal_order(&key(thread, role, FactProvenance::ParentTerminal), 9)
                    .is_some(),
                expected,
                "{rule}"
            );
        }
        assert!(
            index.evidence_frontier() >= 6,
            "F1 frontier covers the prefix"
        );
    }

    #[test]
    fn tool_operation_join_uses_the_thread_qualified_fact_order() {
        // Cause/effect graph: C1 a root Assistant message owns one exact source
        // id; C2 another Thread names that same source id with no matching
        // committed message; C3 the source/cursor vectors align. Effects: E1
        // only the root call receives the Run/step operation; E2 the foreign
        // Thread manufactures no operation; E3 the Advisor order retains the
        // message's commit/time/block coordinate; E4 an unaligned prefix
        // manufactures nothing.
        let run_id = RunId("root-run".into());
        let message = Message::new(
            MessageId::assistant(&run_id, 2),
            Role::Assistant,
            vec![ContentBlock::tool_use(
                "root-call",
                awaken_runtime_contract::resolved::ADVISOR_TOOL_ID,
                serde_json::json!({}),
            )],
        );
        let mut root = RunRecoverySnapshot {
            thread_id: ThreadId(SESSION.into()),
            claimed_run_id: run_id.clone(),
            runs: vec![Record {
                id: run_id.clone(),
                thread_id: ThreadId(SESSION.into()),
                state: RunState::Running,
            }],
            latest_run_id: Some(run_id.clone()),
            messages: vec![message.clone()],
            message_commit_cursors: vec![7],
            commit_timestamps: [(7, 1_800_000_000_123)].into(),
            state: vec![],
            state_commit_cursors: vec![],
            events: vec![],
            resume_tickets: vec![],
            thread_version: 1,
            store_cursor: 7,
            next_commit_ordinal: 1,
        };
        let children = HashMap::new();
        let history = HashMap::new();
        let base = root.clone();
        // E5 a retained historical batch owns the operation even when its
        // recorded step differs from the Message; E6 duplicate source ids are
        // ambiguous and must never select a first/last guessed source.
        for rule in ["transcript", "recorded", "unaligned", "duplicate"] {
            root = base.clone();
            if rule == "unaligned" {
                root.message_commit_cursors.clear();
            }
            if rule == "duplicate" {
                root.messages.push(message.clone());
                root.message_commit_cursors.push(7);
            }
            if rule == "recorded" {
                let batch = awaken_tool_contract::ToolBatch::for_step(
                    run_id.clone(),
                    0,
                    [(
                        awaken_runtime_contract::llm::ToolCall {
                            call_id: "root-call".into(),
                            tool_id: awaken_runtime_contract::resolved::ADVISOR_TOOL_ID.into(),
                            arguments: serde_json::json!({}),
                        },
                        awaken_tool_contract::tool::ToolRecoveryPolicy::durable_request(),
                    )],
                )
                .unwrap();
                let mut command = awaken_tool_contract::ActiveToolBatch::write(&Some(batch));
                command.bind_run(&run_id);
                root.state.push(command);
                root.state_commit_cursors.push(7);
            }
            let expected = awaken_tool_contract::tool_batch::ToolBatch::operation_id_for_step(
                &run_id,
                if rule == "recorded" { 0 } else { 2 },
                "root-call",
            );
            let aligned = rule == "transcript" || rule == "recorded";
            let index = OrderIndex::new(OrderEvidence {
                updates: None,
                session_id: SESSION,
                batches: &[],
                closed_intervals: &[],
                budget_reach: &[],
                parent_terminal: ParentTerminalEvidence::default(),
                lifecycle: &[],
                root: Some(&root),
                children: &children,
                links: &[],
                outcomes: &[],
                historical_pending: &history,
            });
            assert_eq!(
                index.tool_operation(SESSION, &message.id.0, "root-call"),
                aligned.then(|| expected.clone()),
                "E1/E4"
            );
            assert_eq!(
                index.tool_operation("child", &message.id.0, "root-call"),
                None,
                "E2"
            );
            let advisor = index.advisor_operation_orders();
            assert_eq!(
                order(advisor.get(&expected).copied()),
                aligned.then_some((7, 50, (0, 0), Some(1_800_000_000_123))),
                "E3/E4"
            );
        }
    }

    #[test]
    fn same_commit_order_is_a_function_of_each_facts_own_coordinates() {
        // Cause/effect graph: C1 two Messages share one commit and the first
        // has more blocks than the Managed record holds events; C2 a reader
        // canonicalizes that commit holding any number of events (a refresh
        // schedule); C3 a larger inbound batch is retained after a smaller
        // one; C4 two Advisor calls of one commit sit in Messages of different
        // lengths. Effects: E1 a fact's order is identical under every C2; E2
        // every block of the first Message precedes the second Message; E3 the
        // earlier batch keeps its order when C3 adds a later, larger batch;
        // E4 the Advisor calls keep Message order. Rules: R1 C1+C2 -> E1+E2;
        // R2 C3 -> E3; R3 C4 -> E4.
        let run = RunId("run".into());
        let mut blocks = (0..11)
            .map(|step| ContentBlock::thinking(format!("step {step}")))
            .collect::<Vec<_>>();
        blocks.push(ContentBlock::tool_use(
            "late-call",
            awaken_runtime_contract::resolved::ADVISOR_TOOL_ID,
            serde_json::json!({}),
        ));
        let long = Message::new(MessageId::assistant(&run, 0), Role::Assistant, blocks);
        let short = Message::new(
            MessageId::assistant(&run, 1),
            Role::Assistant,
            vec![ContentBlock::tool_use(
                "short-call",
                awaken_runtime_contract::resolved::ADVISOR_TOOL_ID,
                serde_json::json!({}),
            )],
        );
        let text = Message::text(MessageId("after".into()), Role::User, "after");
        let root = RunRecoverySnapshot {
            thread_id: ThreadId(SESSION.into()),
            claimed_run_id: run.clone(),
            runs: vec![Record {
                id: run.clone(),
                thread_id: ThreadId(SESSION.into()),
                state: RunState::Running,
            }],
            latest_run_id: Some(run.clone()),
            messages: vec![long.clone(), short.clone(), text.clone()],
            message_commit_cursors: vec![3, 3, 3],
            commit_timestamps: std::collections::BTreeMap::new(),
            state: vec![],
            state_commit_cursors: vec![],
            events: vec![],
            resume_tickets: vec![],
            thread_version: 1,
            store_cursor: 3,
            next_commit_ordinal: 1,
        };
        let batch = |inputs: usize, revision: u64| {
            let mut batch = SessionEventBatch::compile(
                &awaken_session_types::SessionId::from(SESSION),
                format!("batch-{revision}"),
                (0..inputs)
                    .map(|input| SessionEventInput::UserMessage {
                        content: vec![ContentBlock::text(format!("{revision}-{input}"))],
                    })
                    .collect(),
            )
            .unwrap();
            batch.admission_projection_anchor = Some(SessionEventProjectionAnchor {
                source_commit_cursor: 1,
            });
            batch
        };
        let children = HashMap::new();
        let historical = HashMap::new();
        macro_rules! index_over {
            ($batches:expr) => {
                OrderIndex::new(OrderEvidence {
                    updates: None,
                    session_id: SESSION,
                    batches: $batches,
                    closed_intervals: &[],
                    budget_reach: &[],
                    parent_terminal: ParentTerminalEvidence::default(),
                    lifecycle: &[],
                    root: Some(&root),
                    children: &children,
                    links: &[],
                    outcomes: &[],
                    historical_pending: &historical,
                })
            };
        }
        let small = [batch(1, 1), batch(1, 2)];
        let index = index_over!(&small);
        let after = key(
            SESSION,
            ManagedEventType::Transcript(TranscriptRole::AgentMessage),
            FactProvenance::Message {
                message_id: "after".into(),
                ordinal: 0,
            },
        );
        // The index is built from committed evidence alone: no reader's event
        // count or other held state is an input, so every refresh schedule
        // resolves these keys to the same coordinates.
        let late = order(index.tool_order(SESSION, &long.id.0, "late-call").unwrap());
        let short_order = order(
            index
                .tool_order(SESSION, &short.id.0, "short-call")
                .unwrap(),
        );
        let after = order(index.fact_order(&after));
        assert_eq!(late, Some((3, 50, (0, 11), None)), "R1/E1");
        assert_eq!(short_order, Some((3, 50, (1, 0), None)), "R1/E1");
        assert_eq!(after, Some((3, 50, (2, 0), None)), "R1/E1");
        assert!(late < short_order && short_order < after, "R1/E2");
        let inbound = |index: OrderIndex<'_>| {
            index
                .inbound_orders()
                .map(|(operation, order)| (operation.to_string(), order))
                .collect::<HashMap<_, _>>()
        };
        let before_growth = inbound(index_over!(&small));
        let grown = [batch(1, 1), batch(1, 2), batch(4, 3)];
        let after_growth = inbound(index_over!(&grown));
        for (operation, order) in &before_growth {
            assert_eq!(
                (order.source_commit_cursor, order.phase, order.ordinal),
                {
                    let grown = after_growth[operation];
                    (grown.source_commit_cursor, grown.phase, grown.ordinal)
                },
                "R2/E3 {operation}"
            );
        }
        let advisor = index.advisor_operation_orders();
        let operation = |step, call| {
            awaken_tool_contract::tool_batch::ToolBatch::operation_id_for_step(&run, step, call)
        };
        let late_call = advisor.get(&operation(0, "late-call")).copied();
        let short_call = advisor.get(&operation(1, "short-call")).copied();
        assert!(
            late_call.is_some() && short_call.is_some(),
            "R3 both Advisor calls are indexed"
        );
        assert!(
            late_call < short_call,
            "R3/E4 Advisor calls keep Message order"
        );
    }
}
