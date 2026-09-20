//! Durable, protocol-neutral Session Event-batch intent.
//!
//! The Session root owns the complete ordered batch. User execution is carried
//! by the existing Run dispatch and Thread commit authorities, including any
//! accompanying System input. Outcome state remains owned by the existing Thread
//! Outcome aggregate. These values retain only the stable command intent needed
//! to reconcile those owners after a crash.

use awaken_agent_contract::agent::awaiting::PermissionDecision;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_session_types::{SessionId, SessionRevision};
use serde::{Deserialize, Serialize};

mod identity;
pub use identity::*;

mod thread_reply;
pub use thread_reply::{
    SessionThreadTarget, SessionThreadToolReply, SessionThreadToolReplyActivity,
    SessionThreadToolReplyCommand, SessionThreadToolReplyDelivery, SessionThreadToolReplyFence,
};

#[cfg(kani)]
mod observation_proofs;
#[cfg(any(test, kani))]
mod terminal_resolution_proofs;

/// Protocol-neutral Outcome rubric provenance retained by the Session root.
/// The Outcome aggregate consumes the enclosed reference, while public history
/// can reconstruct whether the caller supplied inline text or a File id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionOutcomeRubric {
    Text { content: String },
    File { file_id: String },
}

impl SessionOutcomeRubric {
    /// Exact value lowered into the existing Outcome definition. File-backed
    /// rubric materialization remains outside the Session root; until that
    /// owner resolves content, the stable File id is the durable reference.
    #[must_use]
    pub fn execution_reference(&self) -> &str {
        match self {
            Self::Text { content } => content,
            Self::File { file_id } => file_id,
        }
    }
}

/// Fully validated neutral input shared by create-time and ordinary root-batch
/// compilation. [`SessionInitialEventPlan`] closes the smaller create-time
/// subset before a Session can be inserted.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionEventInput {
    UserMessage {
        content: Vec<ContentBlock>,
    },
    SystemMessage {
        content: Vec<ContentBlock>,
    },
    DefineOutcome {
        description: String,
        rubric: SessionOutcomeRubric,
        max_iterations: Option<u32>,
    },
    ToolReply(SessionEventToolReply),
    Interrupt(SessionEventInterrupt),
}

/// Exact public provenance for one accepted tool-reply Event. Runtime delivery
/// is derived from this value; it is not a second pending-input owner. Keeping
/// the nullable content shape here lets warm/cold public history distinguish an
/// omitted payload from an explicitly empty payload after the Runtime has
/// consumed both as the same empty result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionEventToolReply {
    /// Public Agent Event id supplied by the SDK client.
    #[serde(rename = "public_tool_use_event_id", alias = "tool_request_event_id")]
    pub tool_request_event_id: String,
    /// Canonical logical target frozen during batch-wide validation.
    pub target: SessionThreadTarget,
    /// Exact Awaiting Run and ticket selected during validation.
    pub expected_run_id: RunId,
    pub expected_correlation_id: String,
    /// Exact Thread commit version that exposed the selected ticket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_thread_version: Option<u64>,
    /// Backend commit that exposed the exact Awaiting boundary answered by this
    /// reply. New admissions retain it; `None` is reserved for legacy rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_pending_commit_cursor: Option<u64>,
    /// Runtime call id carried by the committed ResumeTicket.
    pub runtime_tool_use_id: String,
    pub reply: SessionEventToolReplyKind,
}

/// The three official reply families remain closed and non-interchangeable.
/// This value preserves wire provenance; [`SessionThreadToolReply`] remains the
/// neutral Runtime effect vocabulary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SessionEventToolReplyKind {
    Confirmation {
        allow: bool,
        deny_message: Option<String>,
    },
    CustomToolResult {
        content: Option<Vec<ContentBlock>>,
        is_error: bool,
    },
    ToolResult {
        content: Option<Vec<ContentBlock>>,
        is_error: bool,
    },
}

impl SessionEventToolReply {
    /// The pending occurrence is independent of HTTP key, payload, public id
    /// spelling and later Thread versions. A legacy missing commit cannot
    /// establish a different occurrence of the same Run/correlation/call.
    #[must_use]
    pub fn answers_same_pending_call(&self, other: &Self) -> bool {
        self.target == other.target
            && self.expected_run_id == other.expected_run_id
            && self.expected_correlation_id == other.expected_correlation_id
            && self.runtime_tool_use_id == other.runtime_tool_use_id
            && match (
                self.answered_pending_commit_cursor,
                other.answered_pending_commit_cursor,
            ) {
                (Some(left), Some(right)) => left == right,
                _ => true,
            }
    }

    /// Lower retained provenance into the one existing Session/dispatch resume
    /// command. Adjacent System input is frozen by the batch reconciler so the
    /// System Message and tool result commit in the same resumed Run.
    #[must_use]
    pub fn delivery_command(
        &self,
        session_id: &SessionId,
        accompanying_system: Option<SessionUserRunSystemInput>,
    ) -> SessionThreadToolReplyCommand {
        let reply = match &self.reply {
            SessionEventToolReplyKind::Confirmation {
                allow,
                deny_message,
            } => SessionThreadToolReply::Confirm(if *allow {
                PermissionDecision::Allow { note: None }
            } else {
                PermissionDecision::Deny {
                    reason: deny_message.clone(),
                }
            }),
            SessionEventToolReplyKind::CustomToolResult { content, is_error } => {
                SessionThreadToolReply::Custom {
                    content: content.clone().unwrap_or_default(),
                    is_error: *is_error,
                }
            }
            SessionEventToolReplyKind::ToolResult { content, is_error } => {
                SessionThreadToolReply::Result {
                    content: content.clone().unwrap_or_default(),
                    is_error: *is_error,
                }
            }
        };
        SessionThreadToolReplyCommand {
            session_id: session_id.clone(),
            tool_request_event_id: Some(self.tool_request_event_id.clone()),
            expected_thread_version: self.expected_thread_version,
            target: self.target.clone(),
            expected_run_id: self.expected_run_id.clone(),
            expected_correlation_id: self.expected_correlation_id.clone(),
            tool_use_id: self.runtime_tool_use_id.clone(),
            reply,
            accompanying_system,
        }
    }
}

/// Exact interrupt intent accepted in one root batch. `requested_target`
/// preserves the nullable SDK selector for public echo; `targets` freezes the
/// validated topology so delayed reconciliation cannot broaden the command to
/// Threads created after acceptance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEventInterrupt {
    pub requested_target: Option<SessionThreadTarget>,
    pub targets: Vec<SessionThreadTarget>,
}

/// Complete stable input for one Session-root User Run reservation. Runtime
/// adapters lower this into their existing serializable activation/dispatch;
/// the Session application retains activity admission ownership.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionUserRunCommand {
    pub session_id: crate::SessionId,
    pub agent_id: String,
    pub operation_id: String,
    pub run_id: RunId,
    pub content: Vec<ContentBlock>,
    /// Optional System input frozen into this same Run reservation. It is not a
    /// standalone Session aggregate: the dispatch payload owns intent replay and
    /// the Thread commit owns completion/history.
    pub accompanying_system: Option<SessionUserRunSystemInput>,
    pub data_subject_id: Option<String>,
    /// W3C trace context frozen by the admitting edge. Recovery must relay this
    /// exact optional value instead of capturing the supervisor's ambient span.
    pub traceparent: Option<String>,
}

/// Stable System input accompanying one User Run. The operation coordinate is
/// distinct from the preceding User coordinate so the committed Message alone
/// can rebuild both public inbound Events in their original batch order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionUserRunSystemInput {
    pub operation_id: String,
    pub content: Vec<ContentBlock>,
}

/// One immutable ordered intent retained in the Session root.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEventCommand {
    UserMessage {
        operation_id: String,
        run_id: RunId,
        content: Vec<ContentBlock>,
        /// Request-grain attribution frozen with ordinary admission. Create-time
        /// inputs have no attributed request and therefore retain `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data_subject_id: Option<String>,
    },
    /// Pre-response command intent. The preceding User Run freezes this value
    /// into its activation; only the matching committed System Message proves
    /// completion.
    SystemMessage {
        operation_id: String,
        content: Vec<ContentBlock>,
    },
    DefineOutcome {
        operation_id: String,
        outcome_id: String,
        description: String,
        rubric: SessionOutcomeRubric,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_iterations: Option<u32>,
    },
    ToolReply {
        operation_id: String,
        reply: SessionEventToolReply,
    },
    Interrupt {
        operation_id: String,
        interrupt: SessionEventInterrupt,
    },
}

/// Immutable backend-wide Runtime coordinate selected from the command's
/// authoritative committed effect. The Session root persists it in the same
/// CAS that marks the command processed, so every replica lowers the retained
/// input at one append-only position without a protocol event store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionEventProjectionAnchor {
    pub source_commit_cursor: u64,
}

mod dispatch_settlement;
pub use dispatch_settlement::{
    SessionEventDispatchSettlementPlan, SessionEventDispatchSettlementReceipt,
    session_event_dispatch_settlement_plan,
};
pub use dispatch_settlement::{session_event_execution_may_close, session_inputs_allow_idle};
mod admission;
pub use admission::{
    SessionEventBatchAdmissionAction, SessionEventBatchAdmissionClass,
    SessionEventBatchAdmissionPlan, classify_session_event_batch_admission,
    resolve_session_event_batch_admission,
};

impl SessionEventCommand {
    #[must_use]
    pub fn operation_id(&self) -> &str {
        match self {
            Self::UserMessage { operation_id, .. }
            | Self::SystemMessage { operation_id, .. }
            | Self::DefineOutcome { operation_id, .. }
            | Self::ToolReply { operation_id, .. }
            | Self::Interrupt { operation_id, .. } => operation_id,
        }
    }
}

/// One retained inbound Event and its root-owned publication progress. The
/// immutable Event remains public-history provenance after processing. Its
/// committed-input anchor may arrive before processing reaches a Run boundary.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionEventEntry {
    pub event: SessionEventCommand,
    #[serde(default)]
    pub processed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processed_at_unix_ms: Option<u64>,
    /// Absent until the existing effect owner supplies its immutable commit
    /// coordinate. Public ordering is owned separately by the batch admission
    /// fence, so this later observation cannot relocate an issued Event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection_anchor: Option<SessionEventProjectionAnchor>,
    /// Present only for a User Run completed by the post-Dispatch settlement
    /// path. Legacy/non-Run Events retain `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_settlement: Option<SessionEventDispatchSettlementReceipt>,
}

/// Ordered Session Event-batch provenance. Identity, Events, order, optional
/// admission trace context, and the optional create wake epoch are immutable
/// after admission; each entry owns its input anchor and processed marker, while
/// completed effects remain authoritative in their existing owners. Ordinary
/// accepted batches never hold a Session activity while waiting behind an
/// earlier Run.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionEventBatch {
    pub batch_id: String,
    pub events: Vec<SessionEventEntry>,
    /// Exact Session root revision that admitted this immutable batch. Interval
    /// projection uses the same root-CAS coordinate to assign overlapping and
    /// sequential input to one closed Running interval without guessing from
    /// vector position or a process-local clock.
    #[serde(default)]
    pub admitted_revision: SessionRevision,
    /// Immutable Runtime ordering fence observed before this batch's root CAS.
    /// Every effect caused by the newly admitted command commits after this
    /// fence, so public receipts can be ordered once at admission instead of
    /// moving when their later processing anchor arrives. Closed receipts use
    /// `None` because they contain no public Event.
    pub admission_projection_anchor: Option<SessionEventProjectionAnchor>,
    /// Optional HTTP retry identity and its canonical request fingerprint. The
    /// pair is retained on this existing Session-root command; there is no
    /// protocol-side idempotency table or second Event store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_fingerprint: Option<String>,
    /// A final non-admission receipt competes with ordinary acceptance at the
    /// SAME Session root CAS. It contains no executable Events and permanently
    /// prevents a delayed request with this retry identity from being accepted.
    #[serde(default)]
    pub admission_closed: bool,
    /// W3C trace context captured once for this atomic ordinary admission. It
    /// is request-local observability provenance, not Event or Run identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
    /// Create-only wake activity opened in the original Session insert and
    /// settled as soon as the first effect has its own durable admission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_activity_epoch: Option<u64>,
}

// The sole processed-marker transition, shared by normal processing and
// terminal resolution. Lookup/command identity remains owned by the batch;
// this pure reducer owns only its existing metadata fields, with no new state.
fn anchor_committed_observation(
    processed: bool,
    recorded: &mut Option<SessionEventProjectionAnchor>,
    incoming: SessionEventProjectionAnchor,
) -> Result<bool, SessionEventBatchError> {
    match *recorded {
        Some(anchor) if anchor == incoming => Ok(false),
        Some(_) => Err(SessionEventBatchError::ProgressMismatch),
        None if processed => Err(SessionEventBatchError::ProgressMismatch),
        None => {
            *recorded = Some(incoming);
            Ok(true)
        }
    }
}

fn mark_processed_observation(
    processed: &mut bool,
    recorded_anchor: &mut Option<SessionEventProjectionAnchor>,
    recorded_time: &mut Option<u64>,
    incoming_anchor: Option<SessionEventProjectionAnchor>,
    now: u64,
) -> Result<bool, SessionEventBatchError> {
    if *processed {
        return if *recorded_anchor == incoming_anchor {
            Ok(false)
        } else {
            Err(SessionEventBatchError::ProgressMismatch)
        };
    }
    if recorded_anchor.is_some() && *recorded_anchor != incoming_anchor {
        return Err(SessionEventBatchError::ProgressMismatch);
    }
    *recorded_anchor = incoming_anchor;
    *recorded_time = Some(now);
    *processed = true;
    Ok(true)
}

impl SessionEventBatch {
    /// Ordered identities shared by live receipts and content-free deletion
    /// evidence. Do not derive a second sequence from current execution state.
    pub fn operation_ids(&self) -> Vec<String> {
        self.events
            .iter()
            .map(|entry| entry.event.operation_id().to_owned())
            .collect()
    }

    /// A final non-admission receipt can be observed but cannot be replayed as
    /// accepted input. Both application and wire preflight use this same gate.
    pub fn require_accepted(&self) -> Result<(), SessionEventBatchError> {
        if classify_session_event_batch_admission(true, true, self.admission_closed)
            == SessionEventBatchAdmissionClass::Closed
        {
            return Err(SessionEventBatchError::AdmissionClosed);
        }
        Ok(())
    }

    /// Verify the final-close invariant at the durable codec boundary. Legacy
    /// accepted batches omit `admission_closed` and retain their old meaning.
    pub fn verify_admission(&self) -> Result<(), SessionEventBatchError> {
        let admission = classify_session_event_batch_admission(true, true, self.admission_closed);
        if admission == SessionEventBatchAdmissionClass::Closed
            && (!self.events.is_empty()
                || self.admitted_revision.0 == 0
                || self.batch_id.is_empty()
                || self
                    .idempotency_key
                    .as_ref()
                    .is_none_or(|key| key.trim().is_empty())
                || self
                    .request_fingerprint
                    .as_ref()
                    .is_none_or(|value| value.trim().is_empty())
                || self.admission_projection_anchor.is_some()
                || self.wake_activity_epoch.is_some()
                || self.traceparent.is_some())
        {
            return Err(SessionEventBatchError::InvalidClosedAdmission);
        }
        if admission == SessionEventBatchAdmissionClass::Accepted
            && (self.events.is_empty()
                || self.admitted_revision.0 == 0
                || self.batch_id.is_empty()
                || self.admission_projection_anchor.is_none())
        {
            return Err(SessionEventBatchError::InvalidAcceptedAdmission);
        }
        if admission == SessionEventBatchAdmissionClass::Accepted
            && self.events.iter().any(|entry| {
                entry.dispatch_settlement.is_some_and(|receipt| {
                    !entry.processed
                        || entry.processed_at_unix_ms.is_none()
                        || entry.projection_anchor.is_none()
                        || receipt.session_activity_epoch == 0
                        || receipt.commit_observation_anchor.source_commit_cursor == 0
                        || !matches!(entry.event, SessionEventCommand::UserMessage { .. })
                })
            })
        {
            return Err(SessionEventBatchError::InvalidAcceptedAdmission);
        }
        Ok(())
    }

    pub fn closed_admission(
        batch_id: String,
        revision: SessionRevision,
        key: String,
        fingerprint: String,
    ) -> Result<Self, SessionEventBatchError> {
        if batch_id.is_empty() || revision.0 == 0 {
            return Err(SessionEventBatchError::InvalidBatchRevision);
        }
        let mut batch = Self {
            batch_id,
            events: Vec::new(),
            admitted_revision: revision,
            admission_projection_anchor: None,
            idempotency_key: None,
            request_fingerprint: None,
            admission_closed: true,
            traceparent: None,
            wake_activity_epoch: None,
        };
        batch.bind_idempotency(key, fingerprint)?;
        Ok(batch)
    }

    pub fn bind_idempotency(
        &mut self,
        key: impl Into<String>,
        request_fingerprint: impl Into<String>,
    ) -> Result<(), SessionEventBatchError> {
        let key = key.into();
        let request_fingerprint = request_fingerprint.into();
        if key.trim().is_empty() || request_fingerprint.trim().is_empty() {
            return Err(SessionEventBatchError::InvalidIdempotencyCoordinate);
        }
        self.idempotency_key = Some(key);
        self.request_fingerprint = Some(request_fingerprint);
        Ok(())
    }

    /// Bind the one public-order coordinate before an accepted batch becomes
    /// executable. Exact replay is inert; changing a persisted coordinate fails
    /// closed rather than rewriting issued Event cursors.
    pub fn anchor_admission_projection(
        &mut self,
        anchor: SessionEventProjectionAnchor,
    ) -> Result<bool, SessionEventBatchError> {
        if self.admission_closed {
            return Err(SessionEventBatchError::ProgressMismatch);
        }
        match self.admission_projection_anchor {
            Some(recorded) if recorded == anchor => Ok(false),
            Some(_) => Err(SessionEventBatchError::ProgressMismatch),
            None => {
                self.admission_projection_anchor = Some(anchor);
                Ok(true)
            }
        }
    }

    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.events.iter().all(|entry| entry.processed)
    }

    /// Return the retained User operation for one exact stable Run identity.
    /// Run ids are derived from the immutable batch/ordinal coordinate, so more
    /// than one match is a corrupt aggregate rather than another selection rule.
    pub fn user_run_operation_id(
        &self,
        run_id: &RunId,
    ) -> Result<Option<&str>, SessionEventBatchError> {
        let mut matches = self.events.iter().filter_map(|entry| match &entry.event {
            SessionEventCommand::UserMessage {
                operation_id,
                run_id: retained,
                ..
            } if retained == run_id => Some(operation_id.as_str()),
            _ => None,
        });
        let first = matches.next();
        if matches.next().is_some() {
            return Err(SessionEventBatchError::ProgressMismatch);
        }
        Ok(first)
    }

    /// Retain committed input visibility without completing the command. An
    /// anchor is immutable even while processing remains pending; a late reader
    /// cannot relocate already published input.
    pub fn anchor_committed_input(
        &mut self,
        operation_id: &str,
        anchor: SessionEventProjectionAnchor,
    ) -> Result<bool, SessionEventBatchError> {
        let entry = self
            .events
            .iter_mut()
            .find(|entry| entry.event.operation_id() == operation_id)
            .ok_or(SessionEventBatchError::ProgressMismatch)?;
        anchor_committed_observation(entry.processed, &mut entry.projection_anchor, anchor)
    }

    /// Mark one exact retained entry processed. Exact replay is a no-op; an
    /// operation outside this immutable batch fails closed.
    pub fn mark_processed(
        &mut self,
        operation_id: &str,
        projection_anchor: SessionEventProjectionAnchor,
        processed_at_unix_ms: u64,
    ) -> Result<bool, SessionEventBatchError> {
        let entry = self
            .events
            .iter_mut()
            .find(|entry| entry.event.operation_id() == operation_id)
            .ok_or(SessionEventBatchError::ProgressMismatch)?;
        mark_processed_observation(
            &mut entry.processed,
            &mut entry.projection_anchor,
            &mut entry.processed_at_unix_ms,
            Some(projection_anchor),
            processed_at_unix_ms,
        )
    }

    /// Resolve every still-pending accepted command after a terminal
    /// root won. No Runtime effect is executed: the terminal cleanup cursor is
    /// the first immutable visibility anchor when available, while `None`
    /// leaves the effect observation unknown without changing public order.
    pub fn resolve_terminally(
        &mut self,
        projection_anchor: Option<SessionEventProjectionAnchor>,
        processed_at_unix_ms: u64,
    ) -> usize {
        let mut resolved = 0;
        for entry in &mut self.events {
            if !entry.processed {
                let anchor = entry.projection_anchor.or(projection_anchor);
                mark_processed_observation(
                    &mut entry.processed,
                    &mut entry.projection_anchor,
                    &mut entry.processed_at_unix_ms,
                    anchor,
                    processed_at_unix_ms,
                )
                .expect("an unprocessed entry admits its first observation");
                resolved += 1;
            }
        }
        resolved
    }

    /// Check occupancy in the root snapshot that will be CASed. Resolve exact
    /// key replay first. Processed replies still occupy their original call.
    pub fn validate_tool_reply_occupancy(
        &self,
        retained: &[Self],
    ) -> Result<(), SessionEventBatchError> {
        for (ordinal, entry) in self.events.iter().enumerate() {
            let SessionEventCommand::ToolReply { reply, .. } = &entry.event else {
                continue;
            };
            if retained
                .iter()
                .flat_map(|batch| &batch.events)
                .chain(self.events[..ordinal].iter())
                .any(|previous| {
                    matches!(&previous.event,
                    SessionEventCommand::ToolReply { reply: earlier, .. }
                        if reply.answers_same_pending_call(earlier))
                })
            {
                return Err(SessionEventBatchError::DuplicateToolReply);
            }
        }
        Ok(())
    }

    pub fn compile(
        session_id: &SessionId,
        batch_id: impl Into<String>,
        inputs: Vec<SessionEventInput>,
    ) -> Result<Self, SessionEventBatchError> {
        // Create-time Events do not have an ordinary `/events` admission span.
        // Keep their request-grain attribution and trace provenance absent.
        Self::compile_attributed(session_id, batch_id, inputs, None, None)
    }

    /// Compile the same neutral batch while freezing request-grain data-subject
    /// attribution into every User command and one batch-level trace context.
    pub fn compile_attributed(
        session_id: &SessionId,
        batch_id: impl Into<String>,
        inputs: Vec<SessionEventInput>,
        data_subject_id: Option<String>,
        traceparent: Option<String>,
    ) -> Result<Self, SessionEventBatchError> {
        let batch_id = batch_id.into();
        if session_id.as_str().trim().is_empty() {
            return Err(SessionEventBatchError::EmptySessionId);
        }
        if batch_id.is_empty() {
            return Err(SessionEventBatchError::EmptyBatchId);
        }
        if inputs.is_empty() {
            return Err(SessionEventBatchError::EmptyBatch);
        }
        let system_count = inputs
            .iter()
            .filter(|input| matches!(input, SessionEventInput::SystemMessage { .. }))
            .count();
        if system_count > 1 {
            return Err(SessionEventBatchError::MultipleSystemMessages);
        }
        if let Some(system_ordinal) = inputs
            .iter()
            .position(|input| matches!(input, SessionEventInput::SystemMessage { .. }))
            && (system_ordinal + 1 != inputs.len()
                || system_ordinal == 0
                || !matches!(
                    inputs.get(system_ordinal - 1),
                    Some(
                        SessionEventInput::UserMessage { .. }
                            | SessionEventInput::ToolReply(SessionEventToolReply {
                                reply: SessionEventToolReplyKind::CustomToolResult { .. }
                                    | SessionEventToolReplyKind::ToolResult { .. },
                                ..
                            })
                    )
                ))
        {
            return Err(SessionEventBatchError::SystemMessagePlacement);
        }

        let mut events = Vec::with_capacity(inputs.len());
        for (ordinal, input) in inputs.into_iter().enumerate() {
            let operation_id = session_event_batch_operation(&batch_id, ordinal)?;
            match input {
                SessionEventInput::UserMessage { content } => {
                    if content.is_empty() {
                        return Err(SessionEventBatchError::EmptyMessageContent);
                    }
                    let run_id = session_event_user_run_id(session_id, &operation_id);
                    events.push(SessionEventEntry {
                        event: SessionEventCommand::UserMessage {
                            operation_id,
                            run_id,
                            content,
                            data_subject_id: data_subject_id.clone(),
                        },
                        processed: false,
                        processed_at_unix_ms: None,
                        projection_anchor: None,
                        dispatch_settlement: None,
                    });
                }
                SessionEventInput::SystemMessage { content } => {
                    if content.is_empty() {
                        return Err(SessionEventBatchError::EmptyMessageContent);
                    }
                    events.push(SessionEventEntry {
                        event: SessionEventCommand::SystemMessage {
                            operation_id,
                            content,
                        },
                        processed: false,
                        processed_at_unix_ms: None,
                        projection_anchor: None,
                        dispatch_settlement: None,
                    });
                }
                SessionEventInput::DefineOutcome {
                    description,
                    rubric,
                    max_iterations,
                } => {
                    if description.trim().is_empty() {
                        return Err(SessionEventBatchError::EmptyOutcomeDescription);
                    }
                    if rubric.execution_reference().trim().is_empty() {
                        return Err(SessionEventBatchError::EmptyOutcomeRubric);
                    }
                    if max_iterations.is_some_and(|iterations| iterations == 0) {
                        return Err(SessionEventBatchError::InvalidOutcomeIterations);
                    }
                    let outcome_id = session_event_outcome_id(session_id, &operation_id);
                    events.push(SessionEventEntry {
                        event: SessionEventCommand::DefineOutcome {
                            operation_id,
                            outcome_id,
                            description,
                            rubric,
                            max_iterations,
                        },
                        processed: false,
                        processed_at_unix_ms: None,
                        projection_anchor: None,
                        dispatch_settlement: None,
                    });
                }
                SessionEventInput::ToolReply(reply) => {
                    if reply.tool_request_event_id.trim().is_empty()
                        || reply.runtime_tool_use_id.trim().is_empty()
                        || reply.expected_run_id.0.trim().is_empty()
                        || reply.expected_correlation_id.trim().is_empty()
                    {
                        return Err(SessionEventBatchError::InvalidToolReplyCoordinate);
                    }
                    events.push(SessionEventEntry {
                        event: SessionEventCommand::ToolReply {
                            operation_id,
                            reply,
                        },
                        processed: false,
                        processed_at_unix_ms: None,
                        projection_anchor: None,
                        dispatch_settlement: None,
                    });
                }
                SessionEventInput::Interrupt(interrupt) => {
                    let target_shape_invalid = match &interrupt.requested_target {
                        Some(requested) => {
                            interrupt.targets.len() != 1
                                || interrupt.targets.first() != Some(requested)
                        }
                        None => !interrupt.targets.contains(&SessionThreadTarget::Primary),
                    };
                    if target_shape_invalid
                        || interrupt
                            .targets
                            .iter()
                            .enumerate()
                            .any(|(ordinal, target)| {
                                interrupt.targets[ordinal + 1..].contains(target)
                            })
                    {
                        return Err(SessionEventBatchError::InvalidInterruptTargets);
                    }
                    events.push(SessionEventEntry {
                        event: SessionEventCommand::Interrupt {
                            operation_id,
                            interrupt,
                        },
                        processed: false,
                        processed_at_unix_ms: None,
                        projection_anchor: None,
                        dispatch_settlement: None,
                    });
                }
            }
        }
        Ok(Self {
            batch_id,
            events,
            admitted_revision: SessionRevision::default(),
            admission_projection_anchor: None,
            idempotency_key: None,
            request_fingerprint: None,
            admission_closed: false,
            traceparent,
            wake_activity_epoch: None,
        })
    }
}

/// Complete root command fragment produced before the Session insert.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionInitialEventPlan {
    pub batch: SessionEventBatch,
}

impl SessionInitialEventPlan {
    pub fn compile(
        session_id: &SessionId,
        batch_id: impl Into<String>,
        inputs: Vec<SessionEventInput>,
    ) -> Result<Self, SessionEventBatchError> {
        if inputs.len() > MAX_SESSION_INITIAL_EVENTS {
            return Err(SessionEventBatchError::TooManyEvents);
        }
        if inputs.iter().any(|input| {
            matches!(
                input,
                SessionEventInput::ToolReply(_) | SessionEventInput::Interrupt(_)
            )
        }) {
            return Err(SessionEventBatchError::UnsupportedInitialEvent);
        }
        Ok(Self {
            batch: SessionEventBatch::compile(session_id, batch_id, inputs)?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionEventBatchError {
    #[error("Session Event admission is already closed")]
    AdmissionClosed,
    #[error("Session id is empty")]
    EmptySessionId,
    #[error("Session Event batch id is empty")]
    EmptyBatchId,
    #[error("Session Event batch committed revision must be nonzero")]
    InvalidBatchRevision,
    #[error("Session Event batch is empty")]
    EmptyBatch,
    #[error("Session initial Event batch exceeds 50 Events")]
    TooManyEvents,
    #[error("Session Event message content is empty")]
    EmptyMessageContent,
    #[error("Session Event Outcome description is empty")]
    EmptyOutcomeDescription,
    #[error("Session Event Outcome rubric is empty")]
    EmptyOutcomeRubric,
    #[error("Session Event Outcome iterations must be nonzero")]
    InvalidOutcomeIterations,
    #[error("Session Event batch contains more than one System message")]
    MultipleSystemMessages,
    #[error(
        "Session System message must be final and immediately follow a User message, custom tool result, or tool result"
    )]
    SystemMessagePlacement,
    #[error("Session Event tool reply coordinate is incomplete")]
    InvalidToolReplyCoordinate,
    #[error("tool result already retained for this pending tool event")]
    DuplicateToolReply,
    #[error("Session Event interrupt target set is empty or inconsistent")]
    InvalidInterruptTargets,
    #[error("Session initial Event batch contains an Event allowed only by events.send")]
    UnsupportedInitialEvent,
    #[error("Session Event progress does not match the current operation")]
    ProgressMismatch,
    #[error("Session Event idempotency key and request fingerprint must both be non-empty")]
    InvalidIdempotencyCoordinate,
    #[error("closed Session Event admission must contain only its immutable retry coordinate")]
    InvalidClosedAdmission,
    #[error("accepted Session Event admission must contain events, revision, and projection fence")]
    InvalidAcceptedAdmission,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn final_close_rejects_execution_intent_and_preserves_legacy_acceptance() {
        // Causes: C1 final receipt; C2 a required coordinate is absent; C3
        // execution/activity/trace is injected; C4 old serialized accepted batch
        // omits the new field. Effects: E1 closed receipt roundtrips with no
        // executable work; E2 corrupt proof fails closed; E3 legacy stays accepted.
        // V1 C1=>E1; V2 C1+(C2 or C3)=>E2 (each field independently);
        // V3 C4=>E3. This codec invariant is shared by both durable backends.
        let closed = SessionEventBatch::closed_admission(
            "batch".into(),
            SessionRevision(2),
            "key".into(),
            "fp".into(),
        )
        .unwrap();
        assert!(closed.is_complete());
        assert_eq!(
            closed.require_accepted(),
            Err(SessionEventBatchError::AdmissionClosed)
        );
        assert_eq!(closed.verify_admission(), Ok(()));
        let encoded = serde_json::to_value(&closed).unwrap();
        assert_eq!(
            serde_json::from_value::<SessionEventBatch>(encoded.clone()).unwrap(),
            closed
        );
        let mut accepted = SessionEventBatch::compile(
            &crate::SessionId::from("session"),
            "accepted",
            vec![user("hello")],
        )
        .unwrap();
        accepted.admitted_revision = crate::SessionRevision(1);
        accepted
            .anchor_admission_projection(SessionEventProjectionAnchor {
                source_commit_cursor: 0,
            })
            .unwrap();
        for (field, value) in [
            ("events", serde_json::to_value(&accepted.events).unwrap()),
            ("admitted_revision", serde_json::json!(0)),
            ("batch_id", serde_json::json!("")),
            ("idempotency_key", serde_json::Value::Null),
            ("request_fingerprint", serde_json::json!(" ")),
            ("wake_activity_epoch", serde_json::json!(1)),
            ("traceparent", serde_json::json!("trace")),
        ] {
            let mut corrupt = encoded.clone();
            corrupt[field] = value;
            assert_eq!(
                serde_json::from_value::<SessionEventBatch>(corrupt)
                    .unwrap()
                    .verify_admission(),
                Err(SessionEventBatchError::InvalidClosedAdmission),
                "V2 {field}"
            );
        }
        let mut legacy = serde_json::to_value(&accepted).unwrap();
        legacy.as_object_mut().unwrap().remove("admission_closed");
        let restored: SessionEventBatch = serde_json::from_value(legacy).unwrap();
        assert_eq!(restored, accepted, "V3");
        assert_eq!(restored.verify_admission(), Ok(()));
        assert!(restored.require_accepted().is_ok());
    }

    fn user(text: &str) -> SessionEventInput {
        SessionEventInput::UserMessage {
            content: vec![ContentBlock::text(text)],
        }
    }

    fn system(text: &str) -> SessionEventInput {
        SessionEventInput::SystemMessage {
            content: vec![ContentBlock::text(text)],
        }
    }

    #[test]
    fn tool_reply_occupancy_uses_pending_occurrence_not_delivery_metadata() {
        // Cause/effect decision table: R1 exact occurrence rejects whether the
        // previous entry is pending or processed; R2 payload/public spelling/
        // Thread version changes do not free it; R3 each different owner, Run,
        // correlation, call or known pending commit is a distinct occurrence;
        // R4 missing legacy commit on either side fails closed for the same
        // neutral coordinates; R5 duplicate inside one batch rejects atomically;
        // R6 empty and unrelated batches do not occupy a call.
        let SessionEventInput::ToolReply(mut base) = tool_reply() else {
            unreachable!()
        };
        base.answered_pending_commit_cursor = Some(7);
        let compile = |id: &str, replies: Vec<SessionEventToolReply>| {
            SessionEventBatch::compile(
                &crate::SessionId::from("s"),
                id,
                replies
                    .into_iter()
                    .map(SessionEventInput::ToolReply)
                    .collect(),
            )
            .unwrap()
        };
        let mut retained = compile("old", vec![base.clone()]);
        for processed in [false, true] {
            retained.events[0].processed = processed;
            for variant in 0..11 {
                let mut next = base.clone();
                match variant {
                    0 => {}
                    1 => next.tool_request_event_id = "new-public-spelling".into(),
                    2 => next.expected_thread_version = Some(900),
                    3 => {
                        next.reply = SessionEventToolReplyKind::Confirmation {
                            allow: false,
                            deny_message: None,
                        }
                    }
                    4 => {
                        next.target = SessionThreadTarget::Child(
                            awaken_agent_contract::agent::thread::Id("other".into()),
                        )
                    }
                    5 => next.expected_run_id = RunId("other".into()),
                    6 => next.expected_correlation_id = "other".into(),
                    7 => next.runtime_tool_use_id = "other".into(),
                    8 => next.answered_pending_commit_cursor = Some(900),
                    9 => next.answered_pending_commit_cursor = None,
                    10 => {}
                    _ => unreachable!(),
                }
                let mut old = retained.clone();
                if variant == 10 {
                    let SessionEventCommand::ToolReply { reply, .. } = &mut old.events[0].event
                    else {
                        unreachable!()
                    };
                    reply.answered_pending_commit_cursor = None;
                }
                let result = compile("new", vec![next]).validate_tool_reply_occupancy(&[old]);
                assert_eq!(
                    result.is_ok(),
                    (4..=8).contains(&variant),
                    "R1-R4 variant {variant}, processed {processed}"
                );
            }
        }
        assert!(
            compile("pair", vec![base.clone(), base.clone()])
                .validate_tool_reply_occupancy(&[])
                .is_err(),
            "R5"
        );
        assert!(
            compile("one", vec![base])
                .validate_tool_reply_occupancy(&[])
                .is_ok(),
            "R6"
        );
        let mut empty = retained.clone();
        empty.events.clear();
        assert!(
            empty.validate_tool_reply_occupancy(&[retained]).is_ok(),
            "R6"
        );
    }

    fn tool_reply() -> SessionEventInput {
        SessionEventInput::ToolReply(SessionEventToolReply {
            tool_request_event_id: "evt_tool".into(),
            target: SessionThreadTarget::Primary,
            expected_run_id: RunId("run-awaiting".into()),
            expected_correlation_id: "correlation-awaiting".into(),
            expected_thread_version: None,
            answered_pending_commit_cursor: None,
            runtime_tool_use_id: "tool-awaiting".into(),
            reply: SessionEventToolReplyKind::CustomToolResult {
                content: None,
                is_error: false,
            },
        })
    }

    fn interrupt() -> SessionEventInput {
        SessionEventInput::Interrupt(SessionEventInterrupt {
            requested_target: None,
            targets: vec![SessionThreadTarget::Primary],
        })
    }

    #[test]
    fn batch_plan_preserves_stable_order_and_system_identity() {
        // Cause/effect graph: C1 a nonempty ordered input; C2 the optional System
        // command is final and immediately follows a User message; C3 exact
        // compilation replay. Effects: E1 stable distinct operation/Run ids;
        // E2 the System value remains in the root's pre-response command intent
        // with its stable operation identity; E3 replay is byte-for-byte equal;
        // E4 one exact entry advances independently. The application
        // freezes E2 into the immediately preceding User Run reservation.
        //
        // | Rule | C1 | C2 | C3 | Effect |
        // | R1 | yes | yes | no | E1+E2 |
        // | R2 | yes | yes | yes | E3 |
        // | R3 | retained op | - | - | E4 advance |
        // | R4 | foreign op | - | - | E4 reject |
        // Constraint/invariant: the Session root owns immutable Event order;
        // progress may advance only the entry named by its retained operation.
        let inputs = vec![user("one"), user("two"), system("context")];
        let plan = SessionInitialEventPlan::compile(
            &crate::SessionId::from("session-1"),
            "initial:session-1",
            inputs,
        )
        .expect("R1 compiles");
        let replay = SessionInitialEventPlan::compile(
            &crate::SessionId::from("session-1"),
            "initial:session-1",
            vec![user("one"), user("two"), system("context")],
        )
        .expect("R2 compiles");
        assert_eq!(plan, replay, "R2/E3");
        let SessionEventCommand::SystemMessage {
            operation_id,
            content,
        } = &plan.batch.events[2].event
        else {
            panic!("R1/E2 System command intent")
        };
        assert_eq!(
            decode_session_event_batch_operation(operation_id).map(|value| value.ordinal),
            Some(2),
            "R1/E2"
        );
        assert_eq!(content, &[ContentBlock::text("context")], "R1/E2");
        let mut progress = plan.batch;
        assert!(
            matches!(
                progress.mark_processed(
                    "foreign",
                    SessionEventProjectionAnchor {
                        source_commit_cursor: 9,
                    },
                    1_700_000_000_000
                ),
                Err(SessionEventBatchError::ProgressMismatch)
            ),
            "R4/E4"
        );
        let retained = progress.events[0].event.operation_id().to_string();
        assert!(
            progress
                .mark_processed(
                    &retained,
                    SessionEventProjectionAnchor {
                        source_commit_cursor: 9,
                    },
                    1_700_000_000_000
                )
                .unwrap(),
            "R3/E4"
        );
        assert!(progress.events[0].processed, "R3/E4");
        assert_eq!(
            progress.events[0]
                .projection_anchor
                .map(|anchor| anchor.source_commit_cursor),
            Some(9),
            "R3/E4 processed CAS retains its immutable public-order receipt"
        );
    }

    #[test]
    fn committed_input_anchor_is_immutable_before_and_after_completion() {
        // Cause/effect table: A1 pending+no anchor -> anchor only; A2 same
        // observation/restart -> no-op; A3 changed anchor or unknown operation
        // -> atomic rejection; A4 same-anchor completion -> first timestamp;
        // A5 terminal cleanup -> retain the already published input coordinate;
        // A6 processed/no effect anchor -> reject late relocation. Cross A1-A3
        // with pending/completed state; no row invents a completed Run.
        let anchor = SessionEventProjectionAnchor {
            source_commit_cursor: 7,
        };
        let changed = SessionEventProjectionAnchor {
            source_commit_cursor: 9,
        };
        for complete in [false, true] {
            let mut batch =
                SessionEventBatch::compile(&crate::SessionId::from("s"), "b", vec![user("one")])
                    .unwrap();
            let op = batch.events[0].event.operation_id().to_owned();
            assert!(batch.anchor_committed_input(&op, anchor).unwrap(), "A1");
            assert!(!batch.events[0].processed && batch.events[0].processed_at_unix_ms.is_none());
            batch = serde_json::from_value(serde_json::to_value(batch).unwrap()).unwrap();
            if complete {
                assert!(batch.mark_processed(&op, anchor, 100).unwrap(), "A4");
            }
            let before = batch.clone();
            assert!(!batch.anchor_committed_input(&op, anchor).unwrap(), "A2");
            assert!(batch.anchor_committed_input(&op, changed).is_err(), "A3");
            assert!(
                batch.anchor_committed_input("foreign", anchor).is_err(),
                "A3"
            );
            assert!(batch.mark_processed(&op, changed, 200).is_err(), "A3");
            assert_eq!(batch, before, "A2-A3 no partial mutation");
            batch.resolve_terminally(Some(changed), 300);
            assert_eq!(batch.events[0].projection_anchor, Some(anchor), "A5");
            assert_eq!(
                batch.events[0].processed_at_unix_ms,
                Some(if complete { 100 } else { 300 }),
                "A4-A5"
            );
        }
        let mut unobserved =
            SessionEventBatch::compile(&crate::SessionId::from("s"), "b", vec![user("one")])
                .unwrap();
        let op = unobserved.events[0].event.operation_id().to_owned();
        unobserved.resolve_terminally(None, 100);
        let before = unobserved.clone();
        assert!(
            unobserved.anchor_committed_input(&op, anchor).is_err(),
            "A6"
        );
        assert_eq!(unobserved, before);
    }

    #[test]
    fn admission_projection_anchor_is_required_and_immutable() {
        // Cause/effect graph: C1 accepted versus closed receipt; C2 anchor is
        // absent, exact replay, or different; C3 durable verification occurs.
        // Effects: E1 first accepted anchor binds; E2 exact replay is inert;
        // E3 relocation and closed anchoring fail; E4 only anchored accepted
        // and unanchored closed rows pass the storage invariant. Rules:
        // A1=accepted+absent=>E1; A2=accepted+exact=>E2;
        // A3=accepted+different=>E3; A4=closed+any=>E3;
        // A5=accepted+absent+C3=>E4(reject); A6=A1+C3=>E4(accept).
        let anchor = SessionEventProjectionAnchor {
            source_commit_cursor: 7,
        };
        let mut accepted =
            SessionEventBatch::compile(&crate::SessionId::from("s"), "b", vec![user("one")])
                .unwrap();
        accepted.admitted_revision = crate::SessionRevision(1);
        assert_eq!(
            accepted.verify_admission(),
            Err(SessionEventBatchError::InvalidAcceptedAdmission),
            "A5/E4"
        );
        assert!(
            accepted.anchor_admission_projection(anchor).unwrap(),
            "A1/E1"
        );
        assert!(
            !accepted.anchor_admission_projection(anchor).unwrap(),
            "A2/E2"
        );
        assert!(
            accepted
                .anchor_admission_projection(SessionEventProjectionAnchor {
                    source_commit_cursor: 8,
                })
                .is_err(),
            "A3/E3"
        );
        assert_eq!(accepted.verify_admission(), Ok(()), "A6/E4");

        let mut closed = SessionEventBatch::closed_admission(
            "closed".into(),
            crate::SessionRevision(1),
            "key".into(),
            "fingerprint".into(),
        )
        .unwrap();
        assert!(closed.anchor_admission_projection(anchor).is_err(), "A4/E3");
        assert_eq!(closed.verify_admission(), Ok(()), "A4/E4");
    }

    #[test]
    fn processed_time_is_first_observation_and_legacy_is_unknown() {
        // Decision table: T1 unprocessed -> stamp; T2 exact replay with later
        // clock -> unchanged; T3 changed anchor -> reject atomically; T4 terminal
        // resolution -> stamp only pending; T5 historical missing field -> None.
        let mut batch = SessionEventBatch::compile(
            &crate::SessionId::from("s"),
            "b",
            vec![user("one"), user("two")],
        )
        .unwrap();
        let op = batch.events[0].event.operation_id().to_string();
        let anchor = SessionEventProjectionAnchor {
            source_commit_cursor: 1,
        };
        let state = |batch: &SessionEventBatch, index: usize| serde_json::json!({"processed": batch.events[index].processed, "time": batch.events[index].processed_at_unix_ms});
        let pending = state(&batch, 0);
        assert!(batch.mark_processed(&op, anchor, 100).unwrap(), "T1");
        let first = state(&batch, 0);
        assert!(!batch.mark_processed(&op, anchor, 200).unwrap(), "T2");
        let replayed = state(&batch, 0);
        let before = batch.clone();
        assert!(
            batch
                .mark_processed(
                    &op,
                    SessionEventProjectionAnchor {
                        source_commit_cursor: 2
                    },
                    300
                )
                .is_err(),
            "T3"
        );
        assert_eq!(batch, before);
        let rejected = state(&batch, 0);
        let terminal_pending = state(&batch, 1);
        assert_eq!(batch.resolve_terminally(Some(anchor), 400), 1, "T4");
        awaken_reliability_testkit::write_observation_trace(
            "processed-first-replay-terminal",
            "processed",
            serde_json::json!([
            {"rule": "first", "before": pending, "after": first, "now":100},
            {"rule": "replay", "before": first, "after": replayed, "now":200},
            {"rule": "reject", "before": replayed, "after": rejected, "now":300},
            {"rule": "terminal", "before": terminal_pending, "after": state(&batch, 1), "now":400}]),
        );
        assert_eq!(batch.events[0].processed_at_unix_ms, Some(100));
        assert_eq!(batch.events[1].processed_at_unix_ms, Some(400));
        let mut json = serde_json::to_value(&batch).unwrap();
        for entry in json["events"].as_array_mut().unwrap() {
            entry
                .as_object_mut()
                .unwrap()
                .remove("processed_at_unix_ms");
        }
        let legacy: SessionEventBatch = serde_json::from_value(json).unwrap();
        assert!(
            legacy
                .events
                .iter()
                .all(|entry| entry.processed_at_unix_ms.is_none()),
            "T5"
        );
    }

    #[test]
    fn system_placement_is_a_closed_decision_table() {
        // Cause/effect graph: C1 System count; C2 System is final; C3 immediate
        // predecessor is User. Effect: accept exactly C1<=1+C2+C3 and reject the
        // whole plan otherwise, before any root exists.
        //
        // | Rule | C1 | C2 | C3 | Effect |
        // | S1 | 1 | yes | yes | accept |
        // | S2 | 1 | yes | no | reject |
        // | S3 | 1 | no | any | reject |
        // | S4 | 2 | any | any | reject |
        // Constraint/invariant: at most one System command is accepted, and it
        // is inseparable from the immediately preceding executable input.
        assert!(
            SessionInitialEventPlan::compile(
                &crate::SessionId::from("s"),
                "b1",
                vec![user("u"), system("s")]
            )
            .is_ok(),
            "S1"
        );
        assert!(
            matches!(
                SessionInitialEventPlan::compile(
                    &crate::SessionId::from("s"),
                    "b2",
                    vec![
                        SessionEventInput::DefineOutcome {
                            description: "d".into(),
                            rubric: SessionOutcomeRubric::Text {
                                content: "r".into(),
                            },
                            max_iterations: Some(1),
                        },
                        system("s"),
                    ],
                ),
                Err(SessionEventBatchError::SystemMessagePlacement)
            ),
            "S2"
        );
        assert!(
            matches!(
                SessionInitialEventPlan::compile(
                    &crate::SessionId::from("s"),
                    "b3",
                    vec![user("u"), system("s"), user("later")]
                ),
                Err(SessionEventBatchError::SystemMessagePlacement)
            ),
            "S3"
        );
        assert!(
            matches!(
                SessionInitialEventPlan::compile(
                    &crate::SessionId::from("s"),
                    "b4",
                    vec![user("u"), system("a"), system("b")]
                ),
                Err(SessionEventBatchError::MultipleSystemMessages)
            ),
            "S4"
        );
    }

    #[test]
    fn ordinary_only_commands_are_closed_and_initial_plans_reject_them() {
        // Cause/effect graph: C1 command is ToolReply/Interrupt; C2 compilation
        // is create-time/ordinary; C3 custom/generic result or Confirmation is
        // followed by final System; C4 the exact reply or frozen target
        // coordinate is complete/incomplete.
        // Effects: E1 create rejects every ordinary-only command before root
        // insertion; E2 ordinary retains ToolReply+System in order and lowers
        // the exact Awaiting coordinate through the sole coordination command;
        // E3 ordinary retains the frozen interrupt set; E4 malformed coordinates
        // fail closed before any batch exists.
        //
        // | Rule | C1 | C2 | C3/C4 | Effect |
        // | O1 | ToolReply | create | valid | E1 |
        // | O2 | Interrupt | create | valid | E1 |
        // | O3 | ToolReply | ordinary | final System + complete | E2 |
        // | O4 | Interrupt | ordinary | complete | E3 |
        // | O5 | either | ordinary | incomplete | E4 |
        // | O6 | Confirmation | ordinary | final System | E4 placement reject |
        // | R1 | Confirmation allow | deny text absent/present | closed Allow, no deny text |
        // | R2 | Confirmation deny | deny text absent | closed Deny(None) |
        // | R3 | Confirmation deny | deny text present | closed Deny(exact) |
        // Constraint/invariant: initial Events are a closed subset, and any
        // invalid coordinate rejects the whole batch before durable insertion.
        for (batch_id, input) in [
            ("create-tool", tool_reply()),
            ("create-interrupt", interrupt()),
        ] {
            assert!(
                matches!(
                    SessionInitialEventPlan::compile(
                        &crate::SessionId::from("session"),
                        batch_id,
                        vec![input]
                    ),
                    Err(SessionEventBatchError::UnsupportedInitialEvent)
                ),
                "O1+O2/E1"
            );
        }

        let tool_batch = SessionEventBatch::compile(
            &crate::SessionId::from("session"),
            "ordinary-tool",
            vec![tool_reply(), system("reply context")],
        )
        .expect("O3/E2");
        let SessionEventCommand::ToolReply { reply, .. } = &tool_batch.events[0].event else {
            panic!("O3/E2 retained ToolReply")
        };
        let command = reply.delivery_command(
            &crate::SessionId::from("session"),
            Some(SessionUserRunSystemInput {
                operation_id: tool_batch.events[1].event.operation_id().to_string(),
                content: vec![ContentBlock::text("reply context")],
            }),
        );
        assert_eq!(command.target, SessionThreadTarget::Primary, "O3/E2");
        assert_eq!(command.expected_run_id.0, "run-awaiting", "O3/E2");
        assert_eq!(
            command.expected_correlation_id, "correlation-awaiting",
            "O3/E2"
        );
        assert_eq!(command.tool_use_id, "tool-awaiting", "O3/E2");
        assert!(
            matches!(
                command.reply,
                SessionThreadToolReply::Custom {
                    ref content,
                    is_error: false
                } if content.is_empty()
            ),
            "O3/E2 omitted result remains public provenance but lowers to empty Runtime content"
        );
        assert!(command.accompanying_system.is_some(), "O3/E2");

        for (rule, allow, deny_message, expected) in [
            ("R1a", true, None, PermissionDecision::Allow { note: None }),
            (
                "R1b",
                true,
                Some("must be ignored on allow".to_string()),
                PermissionDecision::Allow { note: None },
            ),
            ("R2", false, None, PermissionDecision::Deny { reason: None }),
            (
                "R3",
                false,
                Some("blocked".to_string()),
                PermissionDecision::Deny {
                    reason: Some("blocked".to_string()),
                },
            ),
        ] {
            let mut input = tool_reply();
            let SessionEventInput::ToolReply(reply) = &mut input else {
                unreachable!()
            };
            reply.reply = SessionEventToolReplyKind::Confirmation {
                allow,
                deny_message,
            };
            assert_eq!(
                reply
                    .delivery_command(&crate::SessionId::from("session"), None)
                    .reply,
                SessionThreadToolReply::Confirm(expected),
                "{rule} sole wire-to-session lowering"
            );
        }

        let mut confirmation = tool_reply();
        let SessionEventInput::ToolReply(reply) = &mut confirmation else {
            unreachable!()
        };
        reply.reply = SessionEventToolReplyKind::Confirmation {
            allow: true,
            deny_message: None,
        };
        assert!(
            matches!(
                SessionEventBatch::compile(
                    &crate::SessionId::from("session"),
                    "confirmation-system",
                    vec![confirmation, system("not allowed")],
                ),
                Err(SessionEventBatchError::SystemMessagePlacement)
            ),
            "O6/E4 official SDK does not allow System after Confirmation"
        );

        let interrupt_batch = SessionEventBatch::compile(
            &crate::SessionId::from("session"),
            "ordinary-interrupt",
            vec![interrupt()],
        )
        .expect("O4/E3");
        assert!(
            matches!(
                &interrupt_batch.events[0].event,
                SessionEventCommand::Interrupt { interrupt, .. }
                    if interrupt.requested_target.is_none()
                        && interrupt.targets == [SessionThreadTarget::Primary]
            ),
            "O4/E3"
        );

        let mut invalid_reply = tool_reply();
        let SessionEventInput::ToolReply(reply) = &mut invalid_reply else {
            unreachable!()
        };
        reply.expected_correlation_id.clear();
        assert!(
            matches!(
                SessionEventBatch::compile(
                    &crate::SessionId::from("session"),
                    "invalid-tool",
                    vec![invalid_reply]
                ),
                Err(SessionEventBatchError::InvalidToolReplyCoordinate)
            ),
            "O5/E4"
        );
        assert!(
            matches!(
                SessionEventBatch::compile(
                    &crate::SessionId::from("session"),
                    "invalid-interrupt",
                    vec![SessionEventInput::Interrupt(SessionEventInterrupt {
                        requested_target: Some(SessionThreadTarget::Primary),
                        targets: vec![SessionThreadTarget::Child(
                            awaken_agent_contract::agent::thread::Id("child".into()),
                        )],
                    })],
                ),
                Err(SessionEventBatchError::InvalidInterruptTargets)
            ),
            "O5/E4"
        );
    }

    #[test]
    fn operation_codec_is_canonical_and_delimiter_safe() {
        // Cause/effect: a valid opaque batch id, including delimiters, and an
        // ordinal round-trip exactly. The create plan separately owns its 50
        // Event limit, so the neutral codec must also support larger ordinary
        // send batches; empty/malformed coordinates still fail closed.
        // Constraint/invariant: byte-length framing plus canonical re-encoding
        // is the only accepted representation. Decision rules: C1 valid opaque
        // id=>exact decode; C2 empty/noncanonical encoding=>reject.
        let encoded = session_event_batch_operation("initial:s:1", 400).unwrap();
        assert_eq!(
            decode_session_event_batch_operation(&encoded),
            Some(SessionEventBatchOperation {
                batch_id: "initial:s:1",
                ordinal: 400,
            })
        );
        assert!(session_event_batch_operation("", 0).is_err());
        assert!(decode_session_event_batch_operation("session-event-batch-v1:1:b:050").is_none());
    }

    #[test]
    fn outcome_identity_is_stable_for_event_and_convenience_commands() {
        // Causes: C1 an Event command has a stable Session/operation coordinate;
        // C2 the legacy convenience command has only Thread + definition.
        // Effects: E1 exact replay returns the same identity in either path; E2
        // a changed operation coordinate or definition changes identity. These
        // rules exclude random/process-local Outcome ids.
        // Constraint/invariant: each command family has one domain-separated,
        // deterministic identity function and never allocates a second owner.
        let event = session_event_outcome_id(&crate::SessionId::from("session"), "operation");
        assert_eq!(
            event,
            session_event_outcome_id(&crate::SessionId::from("session"), "operation"),
            "C1/E1"
        );
        assert_ne!(
            event,
            session_event_outcome_id(&crate::SessionId::from("session"), "other"),
            "C1/E2"
        );
        let convenience = session_outcome_convenience_id("thread", "ship", "correct", 3);
        assert_eq!(
            convenience,
            session_outcome_convenience_id("thread", "ship", "correct", 3),
            "C2/E1"
        );
        assert_ne!(
            convenience,
            session_outcome_convenience_id("thread", "ship", "different", 3),
            "C2/E2"
        );
    }

    #[test]
    fn ordinary_batch_builder_is_revision_owned_attributed_and_not_create_limited() {
        // Builder cause/effect table. C1 prospective root revision is
        // zero/nonzero; C2 input count is above the create-only 50 limit; C3
        // request attribution is absent/present. Effects: E1 derive a stable,
        // lexically ordered batch coordinate only for nonzero revision; E2 the
        // neutral ordinary builder accepts C2 while create Plan rejects it; E3
        // every User command freezes C3. Rules N1 revision7+51+subject=>E1-E3;
        // N2 create Plan+51=>reject; N3 revision0=>reject.
        // Constraint/invariant: revision is prospective committed Session-root
        // order; the 50-Event ceiling belongs only to create-time admission.
        let batch_id = session_event_batch_id(
            &crate::SessionId::from("ordinary"),
            crate::SessionRevision(7),
        )
        .unwrap();
        assert!(
            batch_id
                < session_event_batch_id(
                    &crate::SessionId::from("ordinary"),
                    crate::SessionRevision(8)
                )
                .unwrap(),
            "N1/E1"
        );
        let inputs = (0..=MAX_SESSION_INITIAL_EVENTS)
            .map(|ordinal| user(&format!("message-{ordinal}")))
            .collect::<Vec<_>>();
        let batch = SessionEventBatch::compile_attributed(
            &crate::SessionId::from("ordinary"),
            batch_id,
            inputs.clone(),
            Some("subject-1".into()),
            None,
        )
        .expect("N1 neutral ordinary batch");
        assert_eq!(batch.events.len(), MAX_SESSION_INITIAL_EVENTS + 1, "N1/E2");
        assert!(
            batch.events.iter().all(|entry| matches!(
                &entry.event,
                SessionEventCommand::UserMessage { data_subject_id, .. }
                    if data_subject_id.as_deref() == Some("subject-1")
            )),
            "N1/E3"
        );
        assert!(
            matches!(
                SessionInitialEventPlan::compile(
                    &crate::SessionId::from("ordinary"),
                    "create",
                    inputs
                ),
                Err(SessionEventBatchError::TooManyEvents)
            ),
            "N2/E2"
        );
        assert!(
            session_event_batch_id(
                &crate::SessionId::from("ordinary"),
                crate::SessionRevision(0)
            )
            .is_err(),
            "N3/E1"
        );
    }

    #[test]
    fn admission_traceparent_is_optional_persisted_and_identity_free() {
        // Cause/effect graph: C1 a validated admission traceparent is
        // present/absent; C2 the retained batch is serialized and recovered;
        // C3 the same Session/batch/ordinal is compiled with another trace.
        // Effects: E1 present context round-trips byte-for-byte; E2 absence is
        // omitted on write and defaults to None on recovery; E3 operation,
        // Event, and Run identities are unchanged by C1/C3; E4 a multi-User
        // atomic request stores one batch-level value, never per-User copies.
        //
        // | Rule | C1 | C2 | C3 | Effect |
        // | T1 | present | yes | same | E1+E3 |
        // | T2 | absent | yes | - | E2+E3 |
        // | T3 | changed | no | yes | E3 |
        // | T4 | initial Event | yes | - | E2 |
        // | T5 | present, 2 Users | yes | - | E1+E4 |
        // Constraint/invariant: trace context is immutable observability
        // provenance after admission, never an idempotency or replay key.
        // Create-time Event plans deliberately retain None; this matrix owns
        // ordinary `/events` admission only.
        let traceparent = "00-11111111111111111111111111111111-2222222222222222-01".to_string();
        let traced = SessionEventBatch::compile_attributed(
            &crate::SessionId::from("trace-session"),
            "trace-batch",
            vec![user("run"), user("run-two")],
            None,
            Some(traceparent.clone()),
        )
        .expect("T1 traced admission");
        let absent = SessionEventBatch::compile_attributed(
            &crate::SessionId::from("trace-session"),
            "trace-batch",
            vec![user("run"), user("run-two")],
            None,
            None,
        )
        .expect("T2 untraced admission");
        let changed = SessionEventBatch::compile_attributed(
            &crate::SessionId::from("trace-session"),
            "trace-batch",
            vec![user("run"), user("run-two")],
            None,
            Some("00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01".into()),
        )
        .expect("T3 retraced admission");
        let initial = SessionInitialEventPlan::compile(
            &crate::SessionId::from("trace-session"),
            "initial-trace-batch",
            vec![user("initial")],
        )
        .expect("T4 initial Event plan");

        let coordinate = |batch: &SessionEventBatch| match &batch.events[0].event {
            SessionEventCommand::UserMessage {
                operation_id,
                run_id,
                ..
            } => (operation_id.clone(), run_id.clone()),
            _ => panic!("trace test requires User command"),
        };
        let traced_coordinate = coordinate(&traced);
        let absent_coordinate = coordinate(&absent);
        let changed_coordinate = coordinate(&changed);
        assert_eq!(
            traced.traceparent.as_deref(),
            Some(traceparent.as_str()),
            "T1/E1"
        );
        assert_eq!(absent.traceparent, None, "T2/E2");
        assert_eq!(traced_coordinate.0, absent_coordinate.0, "T1+T2/E3");
        assert_eq!(traced_coordinate.1, absent_coordinate.1, "T1+T2/E3");
        assert_eq!(traced_coordinate.0, changed_coordinate.0, "T3/E3");
        assert_eq!(traced_coordinate.1, changed_coordinate.1, "T3/E3");
        assert_eq!(initial.batch.traceparent, None, "T4/E2");

        let traced_json = serde_json::to_value(&traced).expect("T1 serialize");
        assert!(
            traced_json["events"]
                .as_array()
                .expect("T5 Event array")
                .iter()
                .all(|entry| entry["event"].get("traceparent").is_none()),
            "T5/E4 one batch-level traceparent"
        );
        let recovered: SessionEventBatch = serde_json::from_value(traced_json).expect("T1 recover");
        assert_eq!(
            recovered.traceparent.as_deref(),
            Some(traceparent.as_str()),
            "T1/E1"
        );
        let absent_json = serde_json::to_value(&absent).expect("T2 serialize");
        assert!(
            absent_json.get("traceparent").is_none(),
            "T2/E2 omitted compatibility field"
        );
        let recovered_absent: SessionEventBatch =
            serde_json::from_value(absent_json).expect("T2 recover legacy-compatible row");
        assert_eq!(recovered_absent.traceparent, None, "T2/E2");
    }

    #[test]
    fn tool_request_event_identity_renames_code_without_rewriting_durable_rows() {
        // Causes: C1 an existing retained row uses the historical storage key;
        // C2 a transitional producer uses the clearer Rust field name. Effects:
        // E1 both decode to `tool_request_event_id`; E2 current writers retain
        // the historical key so an older process can still read new rows.
        // Decision table: N1=C1=>E1; N2=C2=>E1; N3=current write=>E2.
        // Constraint/invariant: this is one occurrence identity with a clearer
        // code name, not a schema fork or second compatibility field.
        let SessionEventInput::ToolReply(reply) = tool_reply() else {
            unreachable!("fixture is a tool reply")
        };
        let legacy = serde_json::to_value(&reply).expect("serialize retained reply");
        assert_eq!(legacy["public_tool_use_event_id"], "evt_tool", "N3/E2");
        assert!(legacy.get("tool_request_event_id").is_none(), "N3/E2");
        let recovered: SessionEventToolReply =
            serde_json::from_value(legacy.clone()).expect("N1 legacy row");
        assert_eq!(recovered.tool_request_event_id, "evt_tool", "N1/E1");

        let mut transitional = legacy;
        let value = transitional
            .as_object_mut()
            .unwrap()
            .remove("public_tool_use_event_id")
            .unwrap();
        transitional
            .as_object_mut()
            .unwrap()
            .insert("tool_request_event_id".into(), value);
        let recovered: SessionEventToolReply =
            serde_json::from_value(transitional).expect("N2 transitional row");
        assert_eq!(recovered.tool_request_event_id, "evt_tool", "N2/E1");
    }

    #[test]
    fn outcome_identity_is_stable_and_uses_the_wire_family() {
        // Cause/effect graph: C1 the Session/operation coordinate is replayed or
        // changed. Effects: E1 replay preserves one identity; E2 a changed
        // coordinate changes it; E3 every identity belongs to the server-owned
        // `outc_` wire family. Decision table: O1 same coordinate -> E1+E3;
        // O2 different operation -> E2+E3.
        // Constraint/invariant: identity stays deterministic and within the
        // single Outcome wire namespace; no random or local counter participates.
        let first = session_event_outcome_id(&crate::SessionId::from("session-1"), "operation-1");
        assert_eq!(
            first,
            session_event_outcome_id(&crate::SessionId::from("session-1"), "operation-1"),
            "O1/E1"
        );
        assert!(first.starts_with("outc_"), "O1/E3");
        let second = session_event_outcome_id(&crate::SessionId::from("session-1"), "operation-2");
        assert_ne!(first, second, "O2/E2");
        assert!(second.starts_with("outc_"), "O2/E3");
    }
}
