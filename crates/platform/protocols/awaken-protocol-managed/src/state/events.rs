// @formal entrypoint session_event_batch.idempotent_receipt_is_admission_stable
//! Event driving for [`ManagedState`]: projecting committed Runs/outcomes,
//! the live-inbox surface, and `send_events`/`list_events`.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;
use crate::types::{EvaluatedPermission, SpanModelUsage};
use awaken_agent_contract::RunLifecycleCursor;
use awaken_agent_contract::{RunLifecycleEvent, RunLifecycleEventKind};
#[cfg(test)]
use awaken_managed_session_events::SEND_MESSAGE_TOOL_ID as SEND_MESSAGE;
use awaken_runtime_contract::resolved::ADVISOR_FAILURE_NOTICE;
use awaken_runtime_contract::runs::Pending;
use awaken_session_contract::SessionThreadTarget;
#[cfg(test)]
use awaken_session_contract::SessionThreadToolReply;
use awaken_session_contract::{
    SessionEventCommand, SessionEventInput, SessionEventInterrupt, SessionEventToolReply,
    SessionEventToolReplyKind, SessionOutcomeRubric,
};
use awaken_session_execution_contract::{CoordinatedThreadLink, CoordinatedThreadTarget};

const MANAGED_MULTIAGENT_EVENT_ID_PREFIX: &str = "magent_v1_";

/// The sole order rule for accepted inbound commands. Processing/effect
/// observations may enrich the public row but can never replace this coordinate.
#[must_use]
const fn inbound_admission_order(
    anchor: Option<awaken_session_contract::SessionEventProjectionAnchor>,
) -> Option<(u64, u16)> {
    match anchor {
        Some(anchor) => Some((
            anchor.source_commit_cursor,
            awaken_managed_session_events::ManagedEventType::SessionUpdated.phase(),
        )),
        None => None,
    }
}

/// One public inbound event rebuilt from canonical Session/Thread truth. The
/// batch coordinate is ephemeral ordering metadata, never a second event log.
#[derive(Clone)]
struct DurableInboundProjection {
    event: Event,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InboundProjectionView {
    AdmissionReceipt,
    DurableHistory,
}

/// The one projection algebra for Session-root inbound commands. Admission
/// receipts retain every accepted command; durable history requires the batch's
/// immutable admission fence. Admission deliberately hides mutable progress;
/// history exposes the root-owned processed marker without changing order.
#[must_use]
const fn inbound_projection_visible(view: InboundProjectionView, admission_anchored: bool) -> bool {
    match view {
        InboundProjectionView::AdmissionReceipt => true,
        InboundProjectionView::DurableHistory => admission_anchored,
    }
}

#[must_use]
const fn inbound_processed_at_visible(view: InboundProjectionView, processed: bool) -> bool {
    !matches!(view, InboundProjectionView::AdmissionReceipt) && processed
}

#[cfg(kani)]
/// @obligation session_event_batch.idempotent_receipt_is_admission_stable
/// @kani profile=solver-kissat-v1 tier=nightly
#[kani::proof]
fn inbound_order_is_fixed_before_effect_observation() {
    let admission_cursor: u64 = kani::any();
    let _later_effect_cursor: u64 = kani::any();
    let anchor = Some(awaken_session_contract::SessionEventProjectionAnchor {
        source_commit_cursor: admission_cursor,
    });
    let issued = inbound_admission_order(anchor);
    assert_eq!(
        issued,
        Some((
            admission_cursor,
            awaken_managed_session_events::ManagedEventType::SessionUpdated.phase()
        ))
    );
    // Effect progress has no input to the order function: replay after any
    // effect observation produces the exact coordinate already issued.
    assert_eq!(inbound_admission_order(anchor), issued);
    assert_eq!(inbound_admission_order(None), None);
}

fn inbound_projections(
    session_id: &str,
    batches: &[awaken_session_contract::SessionEventBatch],
    view: InboundProjectionView,
) -> Vec<DurableInboundProjection> {
    let mut projections = Vec::new();
    for batch in batches {
        for entry in &batch.events {
            if inbound_projection_visible(
                view,
                inbound_admission_order(batch.admission_projection_anchor).is_some(),
            ) {
                projections.push(inbound_projection(session_id, entry, view));
            }
        }
    }
    projections
}

/// Rebuild every accepted inbound Event from the Session root's sole retained
/// command provenance. Thread/dispatch/Outcome owners decide when an entry's
/// `processed` bit may advance; the disposable Managed cache never infers the
/// original DTO from their lossy execution payloads.
fn durable_inbound_projections(
    session_id: &str,
    batches: &[awaken_session_contract::SessionEventBatch],
) -> Vec<DurableInboundProjection> {
    inbound_projections(session_id, batches, InboundProjectionView::DurableHistory)
}

fn accepted_inbound_receipts(
    session_id: &str,
    batch: &awaken_session_contract::SessionEventBatch,
) -> Vec<DurableInboundProjection> {
    inbound_projections(
        session_id,
        std::slice::from_ref(batch),
        InboundProjectionView::AdmissionReceipt,
    )
}

fn inbound_projection(
    session_id: &str,
    entry: &awaken_session_contract::SessionEventEntry,
    view: InboundProjectionView,
) -> DurableInboundProjection {
    DurableInboundProjection {
        event: Event {
            id: durable_inbound_event_id(session_id, entry.event.operation_id()),
            kind: public_inbound_kind_from_command(session_id, &entry.event),
            processed_at: inbound_processed_at_visible(view, entry.processed)
                .then(|| super::constants::persisted_time(entry.processed_at_unix_ms)),
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
                let internal = target.thread_id(&awaken_session_types::SessionId::from(session_id));
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
    projected: &[DurableInboundProjection],
) {
    for projection in projected {
        if let Some(existing) = record
            .events
            .iter_mut()
            .find(|existing| existing.id == projection.event.id)
        {
            if projection.event.processed_at.is_some() {
                existing.processed_at = projection.event.processed_at.clone();
            }
            continue;
        }
        record.events.push(projection.event.clone());
    }
}

/// A committed prefix the timeline domain refuses to project is an internal
/// Runtime fault, or a damaged prefix only an interrupt settles; either way
/// the published projection is kept.
pub(super) fn projection_error(
    error: awaken_managed_session_events::ProjectionError,
) -> StateError {
    match error {
        awaken_managed_session_events::ProjectionError::Corrupt(message) => {
            StateError::Run(RunError::internal(message))
        }
        awaken_managed_session_events::ProjectionError::RecoveryRequired(message) => {
            StateError::projection_recovery_required(message)
        }
    }
}

/// Borrowed committed evidence for one child transcript fold. This is only a
/// call-bound view over the existing transcript/lifecycle authorities; it owns
/// no cursor, cache, or projection state.
struct ChildTranscriptProjection<'a> {
    thread_id: &'a str,
    advisor_model: Option<&'a str>,
    messages: &'a [awaken_agent_contract::agent::message::Message],
    lifecycle_events: &'a [RunLifecycleEvent],
    latest_run_id: Option<&'a awaken_agent_contract::agent::run::Id>,
    latest_run_state: Option<&'a awaken_agent_contract::agent::run::RunState>,
}

/// One borrowed view of the already-read coordination facts consumed by the
/// sole child projector. Grouping them prevents the caller and projector from
/// drifting into parallel parameter lists without introducing another owner.
#[derive(Clone, Copy)]
struct DelegationProjectionEvidence<'a> {
    order_index: &'a awaken_managed_session_events::OrderIndex<'a>,
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
    historical_pending: &'a std::collections::HashMap<RunLifecycleCursor, Pending>,
    dispositions: &'a std::collections::HashMap<String, awaken_agent_contract::ThreadDisposition>,
    usage: &'a std::collections::HashMap<String, Option<crate::types::SessionThreadUsage>>,
}

/// Add only Outcome-owned public facts. Root transcript and lifecycle remain
/// exclusively owned by their existing committed projectors. Stable ids plus
/// the one Session-record lock make warm replay and active-active refreshes
/// idempotent without a cursor, receipt, or protocol-side registry. The
/// Session object's `outcome_evaluations` mirror each completed iteration.
fn append_committed_outcomes(
    record: &mut SessionRecord,
    outcomes: &[awaken_managed_session_events::CommittedOutcome],
) -> Result<(), StateError> {
    let session_id = record.session.id.clone();
    for outcome in outcomes {
        for fact in outcome.facts(&session_id) {
            let kind = timeline_encoding::encode_fact(record, &fact, None, None)?;
            record.append_fact_once(fact.key, kind);
        }
        let CommittedOutcomeProjection::Completed(report) = &outcome.terminal else {
            continue;
        };
        for iteration in &report.iterations {
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
    Ok(())
}

/// Durable provenance of one Managed multiagent event: the timeline domain's
/// fact provenance, under the name the event-id encoder has always used.
/// Every value is already owned by the Session/Thread stores and is supplied
/// to the same warm/cold projector that appends the public event. Tool-use
/// events keep their separate reversible `managed_tool_event_id` owner because
/// command admission must recover their Runtime call id.
pub(super) type ManagedMultiagentEventProvenance = awaken_managed_session_events::FactProvenance;

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
                stop_details: None,
            }
        }
        PrimaryThreadStatusProjection::Terminated => OutboundKind::SessionThreadStatusTerminated {
            session_thread_id,
            agent_name,
        },
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
            run_id: run_id.to_string(),
            step,
            response,
        },
    )
}

/// The public id of one timeline fact of `session_id`.
pub(super) fn fact_event_id(
    session_id: &str,
    key: &awaken_managed_session_events::EventIdentity,
) -> String {
    managed_multiagent_event_id(
        session_id,
        &key.thread,
        key.role.as_wire_role(),
        key.provenance.clone(),
    )
}

/// Stable identity for a non-tool event derived from durable multiagent truth.
/// Tool-use events retain the separate reversible `managed_tool_event_id` owner
/// because command admission must recover their Runtime call id.
fn managed_multiagent_event_id(
    session_id: &str,
    thread_id: &str,
    role: &str,
    provenance: ManagedMultiagentEventProvenance,
) -> String {
    format!(
        "{MANAGED_MULTIAGENT_EVENT_ID_PREFIX}{}",
        awaken_agent_contract::stable_fingerprint(&(
            "managed-multiagent-event-v1",
            session_id,
            thread_id,
            role,
            &provenance,
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
pub(super) enum ProjectedToolUseFamily {
    Tool,
    Custom,
    Mcp,
}

impl ProjectedToolUseFamily {
    pub(super) fn from_kind(kind: &OutboundKind) -> Option<Self> {
        match kind {
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
    answered_pending_commit_cursor: u64,
    projected_event_id: Option<String>,
    projected_family: Option<ProjectedToolUseFamily>,
}

impl PendingToolReplyCandidate {
    fn from_pending(
        target: SessionThreadTarget,
        expected_thread_version: u64,
        answered_pending_commit_cursor: u64,
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
            answered_pending_commit_cursor,
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
    answered_pending_commit_cursor: u64,
}

/// Fully validated protocol lowering for one atomic Session-root admission.
/// Ignored budget-pause interrupts are intentionally absent: they create no
/// public receipt and no durable command, matching the existing no-op policy.
struct ValidatedEventBatch {
    inputs: Vec<SessionEventInput>,
    drive: ValidatedEventBatchDrive,
}

/// Request-local execution policy only. Durable ordering and retry remain
/// owned by the existing Session Event lifecycle selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValidatedEventBatchDrive {
    PreferredBatch,
    LifecycleQueue,
}

mod admission;
mod committed_projection;
mod delivery;
#[cfg(test)]
mod event_role;
pub(crate) use committed_projection::ProjectionObservationDecision;
pub(super) use delivery::committed_delivery_delta;
pub(crate) use delivery::{CommittedDeliveryPage, CommittedEventDelivery, session_deletion_event};
mod lifecycle_projection;
mod recovery;
mod stream;
mod timeline_encoding;
mod transcript_projection;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod exhaustive {
    use super::*;

    /// @exhaustive bool(2) x bool(2) = 4
    /// @obligation session_event_batch.idempotent_receipt_is_admission_stable
    #[test]
    fn inbound_admission_receipt_is_stable_across_processing_races() {
        for processed in [false, true] {
            for admission_anchored in [false, true] {
                assert!(inbound_projection_visible(
                    InboundProjectionView::AdmissionReceipt,
                    admission_anchored,
                ));
                assert_eq!(
                    inbound_projection_visible(
                        InboundProjectionView::DurableHistory,
                        admission_anchored,
                    ),
                    admission_anchored
                );
                assert!(!inbound_processed_at_visible(
                    InboundProjectionView::AdmissionReceipt,
                    processed
                ));
                assert_eq!(
                    inbound_processed_at_visible(InboundProjectionView::DurableHistory, processed),
                    processed
                );
            }
        }
    }
}
