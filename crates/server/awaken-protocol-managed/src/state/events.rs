//! Event driving for [`ManagedState`]: projecting committed Runs/outcomes,
//! the live-inbox surface, and `send_events`/`list_events`.

use super::*;
use crate::types::{EvaluatedPermission, SpanModelUsage};
use awaken_agent_contract::RunLifecycleCursor;
use awaken_agent_contract::{RunLifecycleEvent, RunLifecycleEventKind};
use awaken_ext_builtin_tools::SEND_TO_AGENT;
use awaken_runtime_contract::resolved::ADVISOR_FAILURE_NOTICE;
#[cfg(test)]
use awaken_session_contract::SessionThreadToolReply;
use awaken_session_contract::{
    CoordinatedThreadLink, CoordinatedThreadTarget, Pending, SessionEventCommand,
    SessionEventInput, SessionEventInterrupt, SessionEventToolReply, SessionEventToolReplyKind,
    SessionOutcomeRubric, SessionThreadTarget, session_agent_report_messages,
    session_agent_report_text,
};

const MANAGED_MULTIAGENT_EVENT_ID_PREFIX: &str = "magent_v1_";

/// Whether one committed coordinated Run permanently closes its logical child
/// Thread. Ordinary Agent Threads remain reusable after completion or external
/// cancellation and close only on the contract's failed classification;
/// one-shot Advisor consultations close at every terminal Run boundary.
pub(super) fn coordinated_child_run_is_terminal(
    is_advisor: bool,
    state: &awaken_agent_contract::agent::run::RunState,
) -> bool {
    if is_advisor {
        state.is_terminal()
    } else {
        awaken_session_contract::coordinated_thread_failed(state)
    }
}

/// One public inbound event rebuilt from canonical Session/Thread truth. The
/// batch coordinate is ephemeral ordering metadata, never a second event log.
struct DurableInboundProjection {
    event: Event,
}

/// Rebuild every accepted inbound Event from the Session root's sole retained
/// command provenance. Thread/dispatch/Outcome owners decide when an entry's
/// `processed` bit may advance; the disposable Managed cache never infers the
/// original DTO from their lossy execution payloads.
fn durable_inbound_projections(
    session_id: &str,
    batches: &[awaken_session_contract::SessionEventBatch],
) -> Vec<DurableInboundProjection> {
    batches
        .iter()
        .flat_map(|batch| {
            batch
                .events
                .iter()
                // `processed && no anchor` is the isolated pre-anchor schema.
                // Preserve its functional history at the legacy prefix; only
                // new unprocessed/unanchored receipts stay non-listable.
                .filter(|entry| entry.projection_anchor.is_some() || entry.processed)
                .map(move |entry| inbound_projection(session_id, entry))
        })
        .collect()
}

fn accepted_inbound_receipts(
    session_id: &str,
    batch: &awaken_session_contract::SessionEventBatch,
) -> Vec<DurableInboundProjection> {
    batch
        .events
        .iter()
        .map(|entry| inbound_projection(session_id, entry))
        .collect()
}

fn inbound_projection(
    session_id: &str,
    entry: &awaken_session_contract::SessionEventEntry,
) -> DurableInboundProjection {
    DurableInboundProjection {
        event: Event {
            id: durable_inbound_event_id(session_id, entry.event.operation_id()),
            kind: public_inbound_kind_from_command(session_id, &entry.event),
            processed_at: entry.processed.then(|| PROCESSED_AT.to_string()),
        },
    }
}

fn public_inbound_kind_from_command(
    session_id: &str,
    command: &SessionEventCommand,
) -> OutboundKind {
    match command {
        SessionEventCommand::UserMessage { content, .. } => OutboundKind::UserMessage {
            content: content.clone(),
        },
        SessionEventCommand::SystemMessage { content, .. } => OutboundKind::SystemMessage {
            content: content.clone(),
        },
        SessionEventCommand::DefineOutcome {
            outcome_id,
            description,
            rubric,
            max_iterations,
            ..
        } => OutboundKind::UserDefineOutcome {
            description: description.clone(),
            rubric: match rubric {
                SessionOutcomeRubric::Text { content } => OutcomeRubric::Text {
                    content: content.clone(),
                },
                SessionOutcomeRubric::File { file_id } => OutcomeRubric::File {
                    file_id: file_id.clone(),
                },
            },
            max_iterations: *max_iterations,
            outcome_id: outcome_id.clone(),
        },
        SessionEventCommand::ToolReply { reply, .. } => {
            let session_thread_id = reply
                .target
                .child_thread_id()
                .map(|thread_id| public_thread_id(session_id, &thread_id.0));
            match &reply.reply {
                SessionEventToolReplyKind::Confirmation {
                    allow,
                    deny_message,
                } => OutboundKind::UserToolConfirmation {
                    tool_use_id: reply.tool_request_event_id.clone(),
                    result: if *allow {
                        ConfirmResult::Allow
                    } else {
                        ConfirmResult::Deny
                    },
                    deny_message: deny_message.clone(),
                    session_thread_id,
                },
                SessionEventToolReplyKind::CustomToolResult { content, is_error } => {
                    OutboundKind::UserCustomToolResult {
                        custom_tool_use_id: reply.tool_request_event_id.clone(),
                        content: content.clone(),
                        is_error: *is_error,
                        session_thread_id,
                    }
                }
                SessionEventToolReplyKind::ToolResult { content, is_error } => {
                    OutboundKind::UserToolResult {
                        tool_use_id: reply.tool_request_event_id.clone(),
                        content: content.clone(),
                        is_error: *is_error,
                        session_thread_id,
                    }
                }
            }
        }
        SessionEventCommand::Interrupt { interrupt, .. } => OutboundKind::UserInterrupt {
            session_thread_id: interrupt.requested_target.as_ref().map(|target| {
                let internal = target.thread_id(session_id);
                public_thread_id(session_id, &internal.0)
            }),
        },
    }
}

/// Merge rebuildable inbound projections into the disposable record. Root CAS
/// progress advances an existing receipt in place; no process-local marker may
/// move it ahead of the durable entry.
fn merge_durable_inbound_projections(
    record: &mut SessionRecord,
    projected: Vec<DurableInboundProjection>,
) {
    for projection in projected {
        if let Some(existing) = record
            .events
            .iter_mut()
            .find(|existing| existing.id == projection.event.id)
        {
            if projection.event.processed_at.is_some() {
                existing.processed_at = projection.event.processed_at;
            }
            continue;
        }
        record.events.push(projection.event);
    }
}

/// One terminal Outcome selected by retained Session-root provenance and read
/// from the extension's exact committed Thread snapshot. This is transient
/// projector input, not another Outcome record.
struct DurableOutcomeProjection {
    outcome_id: String,
    terminal: CommittedOutcomeProjection,
}

/// A terminal Outcome paired with the exact root-Thread commit coordinates
/// that make each projected fact public. Keeping the evidence with the value
/// prevents admission and canonical ordering from independently rediscovering
/// (and potentially disagreeing about) the same anchors.
struct AnchoredOutcomeProjection {
    outcome_id: String,
    terminal: CommittedOutcomeProjection,
    terminal_commit_cursor: u64,
    evaluation_commit_cursors: std::collections::HashMap<u32, u64>,
}

/// Borrowed committed evidence for one child transcript fold. This is only a
/// call-bound view over the existing transcript/lifecycle authorities; it owns
/// no cursor, cache, or projection state.
struct ChildTranscriptProjection<'a> {
    thread_id: &'a str,
    agent_name: &'a str,
    advisor_model: Option<&'a str>,
    messages: &'a [awaken_agent_contract::agent::message::Message],
    lifecycle_events: &'a [RunLifecycleEvent],
    latest_run_id: Option<&'a awaken_agent_contract::agent::run::Id>,
    latest_run_state: Option<&'a awaken_agent_contract::agent::run::RunState>,
    pending: Option<&'a Pending>,
    pending_source_run_id: Option<&'a awaken_agent_contract::agent::run::Id>,
    historical_pending: &'a std::collections::HashMap<RunLifecycleCursor, Pending>,
}

/// One borrowed view of the already-read coordination facts consumed by the
/// sole child projector. Grouping them prevents the caller and projector from
/// drifting into parallel parameter lists without introducing another owner.
struct DelegationProjectionEvidence<'a> {
    links: &'a [CoordinatedThreadLink],
    snapshots: &'a std::collections::HashMap<
        String,
        awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
    >,
    transcripts:
        &'a std::collections::HashMap<String, Vec<awaken_agent_contract::agent::message::Message>>,
    lifecycle_events: &'a [RunLifecycleEvent],
    latest_run_states:
        &'a std::collections::HashMap<String, awaken_agent_contract::agent::run::RunState>,
    pending: &'a std::collections::HashMap<String, Pending>,
    historical_pending: &'a std::collections::HashMap<RunLifecycleCursor, Pending>,
    dispositions: &'a std::collections::HashMap<String, awaken_agent_contract::ThreadDisposition>,
    usage: &'a std::collections::HashMap<String, Option<crate::types::SessionThreadUsage>>,
}

impl AnchoredOutcomeProjection {
    fn owns_failure_run(&self, run_id: &awaken_agent_contract::agent::run::Id) -> bool {
        matches!(
            &self.terminal,
            CommittedOutcomeProjection::Errored(failure)
                if failure.source_run_id.as_ref() == Some(run_id)
        )
    }
}

impl DurableOutcomeProjection {
    /// Pair the terminal query with its exact Outcome-owned root commit. The
    /// terminal state update and removal of the active pointer are emitted by
    /// one commit, so their shared cursor is a typed terminal fence without
    /// decoding extension-private JSON. A cancellation can synthesize its last
    /// public `interrupted` cycle without a Grade; that cycle is ordered by the
    /// terminal fence, while every ordinary cycle still requires its immutable
    /// evaluation command.
    fn anchor(
        self,
        snapshot: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Option<AnchoredOutcomeProjection> {
        let snapshot = snapshot?;
        if snapshot.state.len() != snapshot.state_commit_cursors.len() {
            return None;
        }
        let commands = snapshot
            .state
            .iter()
            .zip(snapshot.state_commit_cursors.iter().copied())
            .collect::<Vec<_>>();
        let state_key = format!("outcome/{}/state", self.outcome_id);
        let terminal_commit_cursor = commands
            .iter()
            .rev()
            .find_map(|(command, cursor)| (command.key.0 == state_key).then_some(*cursor))?;
        let has_terminal_pointer_removal = commands.iter().any(|(command, cursor)| {
            command.key.0 == "outcome/active"
                && matches!(
                    command.action,
                    awaken_agent_contract::agent::state::Action::Remove
                )
                && *cursor == terminal_commit_cursor
        });
        if !has_terminal_pointer_removal {
            return None;
        }

        let mut evaluation_commit_cursors = std::collections::HashMap::new();
        if let CommittedOutcomeProjection::Completed(report) = &self.terminal {
            for (index, item) in report.iterations.iter().enumerate() {
                let key = format!("outcome/{}/evaluation/{}", self.outcome_id, item.iteration);
                let cursor = commands
                    .iter()
                    .find_map(|(command, cursor)| (command.key.0 == key).then_some(*cursor))
                    .or_else(|| {
                        (index + 1 == report.iterations.len() && item.result == "interrupted")
                            .then_some(terminal_commit_cursor)
                    })?;
                evaluation_commit_cursors.insert(item.iteration, cursor);
            }
        }
        Some(AnchoredOutcomeProjection {
            outcome_id: self.outcome_id,
            terminal: self.terminal,
            terminal_commit_cursor,
            evaluation_commit_cursors,
        })
    }
}

fn retained_outcome_ids(batches: &[awaken_session_contract::SessionEventBatch]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    batches
        .iter()
        .flat_map(|batch| &batch.events)
        .filter_map(|entry| match &entry.event {
            SessionEventCommand::DefineOutcome { outcome_id, .. }
                if seen.insert(outcome_id.clone()) =>
            {
                Some(outcome_id.clone())
            }
            _ => None,
        })
        .collect()
}

fn validate_durable_outcome_projection(
    outcome_id: String,
    terminal: CommittedOutcomeProjection,
) -> Result<DurableOutcomeProjection, StateError> {
    if let CommittedOutcomeProjection::Completed(report) = &terminal {
        let mut previous_iteration = None;
        for iteration in &report.iterations {
            if iteration.outcome_id != outcome_id {
                return Err(StateError::Run(RunError::internal(
                    "committed Outcome report id disagrees with Session provenance",
                )));
            }
            if previous_iteration.is_some_and(|previous| iteration.iteration <= previous) {
                return Err(StateError::Run(RunError::internal(
                    "committed Outcome report iterations are not strictly ordered",
                )));
            }
            previous_iteration = Some(iteration.iteration);
        }
    }
    Ok(DurableOutcomeProjection {
        outcome_id,
        terminal,
    })
}

/// Add only Outcome-owned public facts. Root transcript and lifecycle remain
/// exclusively owned by their existing committed projectors. Stable ids plus
/// the one Session-record lock make warm replay and active-active refreshes
/// idempotent without a cursor, receipt, or protocol-side registry.
fn append_durable_outcome_projections(
    record: &mut SessionRecord,
    projections: &[AnchoredOutcomeProjection],
) {
    let mut event_ids = record
        .events
        .iter()
        .map(|event| event.id.clone())
        .collect::<std::collections::HashSet<_>>();
    for projection in projections {
        match &projection.terminal {
            CommittedOutcomeProjection::Completed(report) => {
                for iteration in &report.iterations {
                    for (role, kind) in [
                        (
                            "outcome-evaluation-start",
                            OutboundKind::SpanOutcomeEvaluationStart {
                                outcome_id: projection.outcome_id.clone(),
                                iteration: iteration.iteration,
                            },
                        ),
                        (
                            "outcome-evaluation-ongoing",
                            OutboundKind::SpanOutcomeEvaluationOngoing {
                                outcome_id: projection.outcome_id.clone(),
                                iteration: iteration.iteration,
                            },
                        ),
                        (
                            "outcome-evaluation-end",
                            OutboundKind::SpanOutcomeEvaluationEnd {
                                outcome_id: projection.outcome_id.clone(),
                                iteration: iteration.iteration,
                                result: iteration.result.clone(),
                                explanation: iteration.explanation.clone(),
                            },
                        ),
                    ] {
                        let id = managed_multiagent_event_id(
                            &record.session.id,
                            &record.session.id,
                            role,
                            ManagedMultiagentEventProvenance::OutcomeEvaluation {
                                outcome_id: &projection.outcome_id,
                                iteration: iteration.iteration,
                            },
                        );
                        if event_ids.insert(id.clone()) {
                            record.events.push(Event {
                                id,
                                kind,
                                processed_at: Some(PROCESSED_AT.to_string()),
                            });
                        }
                    }

                    let evaluation = project::outcome_evaluation(iteration);
                    if let Some(existing) = record
                        .session
                        .outcome_evaluations
                        .iter_mut()
                        .find(|existing| existing.outcome_id == evaluation.outcome_id)
                    {
                        *existing = evaluation;
                    } else {
                        record.session.outcome_evaluations.push(evaluation);
                    }
                }
            }
            CommittedOutcomeProjection::Errored(failure) => {
                let id = managed_multiagent_event_id(
                    &record.session.id,
                    &record.session.id,
                    "outcome-error",
                    ManagedMultiagentEventProvenance::Outcome {
                        outcome_id: &projection.outcome_id,
                    },
                );
                if event_ids.insert(id.clone()) {
                    record.events.push(Event {
                        id,
                        kind: OutboundKind::SessionError {
                            error: SessionError::classify(&failure.code, failure.message.clone()),
                        },
                        processed_at: Some(PROCESSED_AT.to_string()),
                    });
                }
            }
        }
    }
}

/// Durable provenance accepted by the sole Managed multiagent projector.
///
/// This is deliberately not a second event aggregate or registry: every value
/// is already owned by the Session/Thread stores and is supplied to the same
/// warm/cold projector that appends the public event. A root/child assistant
/// preview uses the same response coordinate when present; generic inbound and
/// outcome events do not use this identity because their current durable owners
/// do not expose an exactly replayable per-event coordinate.
#[derive(serde::Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
enum ManagedMultiagentEventProvenance<'a> {
    LinkOperation {
        operation_id: &'a str,
    },
    CoordinationCall {
        event_id: &'a str,
    },
    Message {
        message_id: &'a str,
        ordinal: usize,
    },
    AssistantResponse {
        run_id: &'a str,
        step: usize,
        response: usize,
    },
    AgentReport {
        run_id: &'a str,
    },
    Lifecycle {
        cursor: u64,
    },
    LifecyclePrefix {
        cursor: u64,
    },
    RuntimeInterval {
        interval_id: &'a str,
    },
    BudgetReach {
        generation: u64,
    },
    Audit {
        run_id: &'a str,
        sequence: u64,
    },
    RunState {
        run_id: &'a str,
    },
    OutcomeEvaluation {
        outcome_id: &'a str,
        iteration: u32,
    },
    Outcome {
        outcome_id: &'a str,
    },
    ArchivedDisposition,
    ParentTerminal,
}

/// The four official primary-Thread lifecycle projections. Session lifecycle
/// remains the aggregate authority; this enum only prevents foreground,
/// warm/cold recovery, Outcome, and failure paths from hand-building divergent
/// public `session.thread_status_*` payloads.
enum PrimaryThreadStatusProjection {
    Running,
    Rescheduled,
    Idle { stop_reason: StopReason },
    Terminated,
}

fn primary_thread_status_kind(
    record: &SessionRecord,
    status: PrimaryThreadStatusProjection,
) -> OutboundKind {
    let session_thread_id = public_thread_id(&record.session.id, &record.session.id);
    let agent_name = record.session.agent.name.clone();
    match status {
        PrimaryThreadStatusProjection::Running => OutboundKind::SessionThreadStatusRunning {
            session_thread_id,
            agent_name,
        },
        PrimaryThreadStatusProjection::Rescheduled => {
            OutboundKind::SessionThreadStatusRescheduled {
                session_thread_id,
                agent_name,
            }
        }
        PrimaryThreadStatusProjection::Idle { stop_reason } => {
            OutboundKind::SessionThreadStatusIdle {
                session_thread_id,
                agent_name,
                stop_reason,
            }
        }
        PrimaryThreadStatusProjection::Terminated => OutboundKind::SessionThreadStatusTerminated {
            session_thread_id,
            agent_name,
        },
    }
}

fn primary_thread_status_event(
    record: &SessionRecord,
    id: String,
    status: PrimaryThreadStatusProjection,
) -> Event {
    Event {
        id,
        kind: primary_thread_status_kind(record, status),
        processed_at: Some(PROCESSED_AT.to_string()),
    }
}

/// Canonical deterministic id shared by root/child live previews and their
/// warm/cold committed transcript projector. The event role is kept in the
/// outer `role` coordinate, so thinking/message ids remain distinct without
/// depending on whether the provider emitted a reasoning Delta.
pub(crate) fn managed_assistant_event_id(
    session_id: &str,
    thread_id: &str,
    run_id: &str,
    step: usize,
    response: usize,
    role: &'static str,
) -> String {
    managed_multiagent_event_id(
        session_id,
        thread_id,
        role,
        ManagedMultiagentEventProvenance::AssistantResponse {
            run_id,
            step,
            response,
        },
    )
}

/// Stable identity for a non-tool event derived from durable multiagent truth.
/// Tool-use events retain the separate reversible `managed_tool_event_id` owner
/// because command admission must recover their Runtime call id.
fn managed_multiagent_event_id(
    session_id: &str,
    thread_id: &str,
    role: &str,
    provenance: ManagedMultiagentEventProvenance<'_>,
) -> String {
    format!(
        "{MANAGED_MULTIAGENT_EVENT_ID_PREFIX}{}",
        awaken_session_contract::stable_fingerprint(&(
            "managed-multiagent-event-v1",
            session_id,
            thread_id,
            role,
            provenance,
        ))
    )
}

/// Translate the Managed public Thread id at the protocol edge. The neutral
/// `SessionThreadTarget` is the sole topology vocabulary used by admission,
/// Session activity transfer, durable dispatch, and interruption.
fn session_thread_target_from_public(session_id: &str, thread_id: &str) -> SessionThreadTarget {
    let internal_thread_id = internal_thread_id(session_id, thread_id);
    if internal_thread_id == session_id {
        SessionThreadTarget::Primary
    } else {
        SessionThreadTarget::Child(awaken_agent_contract::agent::thread::Id(internal_thread_id))
    }
}

fn public_child_thread_id(target: &SessionThreadTarget) -> Option<&str> {
    target
        .child_thread_id()
        .map(|thread_id| thread_id.0.as_str())
}

/// Public tool-use family already committed by the sole Managed projector.
/// Runtime's pending boolean distinguishes permission from externally supplied
/// results, but cannot distinguish an Agent tool from a custom tool; that wire
/// constraint therefore remains at this adapter boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectedToolUseFamily {
    Tool,
    Custom,
    Mcp,
}

impl ProjectedToolUseFamily {
    fn from_event(event: &Event) -> Option<Self> {
        match event.kind {
            OutboundKind::AgentToolUse { .. } => Some(Self::Tool),
            OutboundKind::AgentCustomToolUse { .. } => Some(Self::Custom),
            OutboundKind::AgentMcpToolUse { .. } => Some(Self::Mcp),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolReplyFamily {
    Confirmation,
    CustomResult,
    ToolResult,
}

impl ToolReplyFamily {
    const fn requires_client_execution(self) -> bool {
        matches!(self, Self::CustomResult | Self::ToolResult)
    }

    const fn accepts(self, projected: ProjectedToolUseFamily) -> bool {
        match self {
            Self::Confirmation => {
                matches!(
                    projected,
                    ProjectedToolUseFamily::Tool | ProjectedToolUseFamily::Mcp
                )
            }
            Self::CustomResult => matches!(projected, ProjectedToolUseFamily::Custom),
            Self::ToolResult => matches!(projected, ProjectedToolUseFamily::Tool),
        }
    }
}

#[derive(Debug, Clone)]
struct PendingToolReplyCandidate {
    key: PendingToolReplyKey,
    projected_event_id: Option<String>,
    projected_family: Option<ProjectedToolUseFamily>,
}

impl PendingToolReplyCandidate {
    fn from_pending(
        target: SessionThreadTarget,
        expected_thread_version: u64,
        expected_run_id: awaken_agent_contract::agent::run::Id,
        expected_correlation_id: String,
        pending: Pending,
    ) -> Self {
        Self {
            key: PendingToolReplyKey {
                target,
                expected_thread_version,
                expected_run_id,
                expected_correlation_id,
                runtime_call_id: pending.tool_use_id,
                client_executed: pending.client_executed,
            },
            projected_event_id: None,
            projected_family: None,
        }
    }
}

/// Unique committed pending identity within one admission snapshot. Thread
/// alone is insufficient because one Runtime ToolBatch may expose multiple
/// independently answerable calls on that same Thread.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PendingToolReplyKey {
    target: SessionThreadTarget,
    expected_thread_version: u64,
    expected_run_id: awaken_agent_contract::agent::run::Id,
    expected_correlation_id: String,
    runtime_call_id: String,
    client_executed: bool,
}

/// Ephemeral output of batch admission. It carries the one resolved target and
/// Runtime call id through activity admission and execution, so neither phase
/// reinterprets the optional public selector or maintains a parallel registry.
#[derive(Debug, Clone)]
struct ResolvedToolReply {
    key: PendingToolReplyKey,
}

/// Fully validated protocol lowering for one atomic Session-root admission.
/// Ignored budget-pause interrupts are intentionally absent: they create no
/// public receipt and no durable command, matching the existing no-op policy.
struct ValidatedEventBatch {
    inputs: Vec<SessionEventInput>,
}

mod admission;
mod committed_projection;
mod lifecycle_projection;
mod recovery;
mod stream;
mod transcript_projection;

#[cfg(test)]
mod tests;
