//! Durable, protocol-neutral Session Event-batch intent.
//!
//! The Session root owns the complete ordered batch. User execution is carried
//! by the existing Run dispatch and Thread commit authorities, including any
//! accompanying System input. Outcome state remains owned by the existing Thread
//! Outcome aggregate. These values retain only the stable command intent needed
//! to reconcile those owners after a crash.

use crate::{SessionThreadTarget, SessionThreadToolReply, SessionThreadToolReplyCommand};
use awaken_agent_contract::agent::awaiting::PermissionDecision;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::run::Id as RunId;
use serde::{Deserialize, Serialize};

const SESSION_EVENT_BATCH_OPERATION_PREFIX: &str = "session-event-batch-v1:";
const SESSION_ROOT_REVISION_BATCH_PREFIX: &str = "session-root-revision-v1:";
pub const MAX_SESSION_INITIAL_EVENTS: usize = 50;
/// Stable retry classification when another Outcome owns the Thread aggregate.
pub const OUTCOME_BUSY_CODE: &str = "outcome_busy";

/// Derive the only ordinary Event-batch identity from its prospective committed
/// Session root revision. Fixed-width decimal encoding preserves FIFO ordering
/// without a clock, random id, or process-local counter.
pub fn session_event_batch_id(
    session_id: &str,
    committed_revision: crate::SessionRevision,
) -> Result<String, SessionEventBatchError> {
    if session_id.trim().is_empty() {
        return Err(SessionEventBatchError::EmptySessionId);
    }
    if committed_revision.0 == 0 {
        return Err(SessionEventBatchError::InvalidBatchRevision);
    }
    Ok(format!(
        "{SESSION_ROOT_REVISION_BATCH_PREFIX}{:020}",
        committed_revision.0
    ))
}

/// Borrowed coordinate decoded from one canonical Session Event operation id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionEventBatchOperation<'a> {
    pub batch_id: &'a str,
    pub ordinal: usize,
}

/// Encode one Event's stable operation coordinate. Byte-length framing keeps an
/// opaque batch id containing `:` exactly reversible without another registry.
pub fn session_event_batch_operation(
    batch_id: &str,
    ordinal: usize,
) -> Result<String, SessionEventBatchError> {
    if batch_id.is_empty() {
        return Err(SessionEventBatchError::EmptyBatchId);
    }
    Ok(format!(
        "{SESSION_EVENT_BATCH_OPERATION_PREFIX}{}:{batch_id}:{ordinal}",
        batch_id.len()
    ))
}

/// Decode only the canonical representation produced by
/// [`session_event_batch_operation`].
#[must_use]
pub fn decode_session_event_batch_operation(
    operation_id: &str,
) -> Option<SessionEventBatchOperation<'_>> {
    let encoded = operation_id.strip_prefix(SESSION_EVENT_BATCH_OPERATION_PREFIX)?;
    let (batch_len, payload) = encoded.split_once(':')?;
    let batch_len = batch_len.parse::<usize>().ok()?;
    let batch_id = payload.get(..batch_len)?;
    let ordinal = payload.get(batch_len..)?.strip_prefix(':')?;
    let ordinal = ordinal.parse::<usize>().ok()?;
    if batch_id.is_empty() {
        return None;
    }
    let coordinate = SessionEventBatchOperation { batch_id, ordinal };
    (session_event_batch_operation(batch_id, ordinal)
        .ok()
        .as_deref()
        == Some(operation_id))
    .then_some(coordinate)
}

/// Stable User Run identity shared by create-time and ordinary Session Event
/// batches. The operation id is already the unique batch/ordinal coordinate.
#[must_use]
pub fn session_event_user_run_id(session_id: &str, operation_id: &str) -> RunId {
    RunId(format!(
        "session-event-run-{}",
        crate::stable_fingerprint(&("session-event-user-run-v1", session_id, operation_id,))
    ))
}

/// Stable Outcome identity shared by create-time and ordinary Session Event
/// batches; ThreadOutcomeState remains the result owner.
#[must_use]
pub fn session_event_outcome_id(session_id: &str, operation_id: &str) -> String {
    format!(
        "outc_{}",
        crate::stable_fingerprint(&("session-event-outcome-v1", session_id, operation_id,))
    )
}

/// Stable identity for the legacy convenience composition that has no explicit
/// Session Event operation coordinate. This keeps all callers deterministic;
/// durable Event admission uses [`session_event_outcome_id`] instead.
#[must_use]
pub fn session_outcome_convenience_id(
    thread: &str,
    description: &str,
    rubric: &str,
    max_iterations: u32,
) -> String {
    crate::stable_fingerprint(&(
        "session-outcome-convenience-v1",
        thread,
        description,
        rubric,
        max_iterations,
    ))
}

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
    /// Lower retained provenance into the one existing Session/dispatch resume
    /// command. Adjacent System input is frozen by the batch reconciler so the
    /// System Message and tool result commit in the same resumed Run.
    #[must_use]
    pub fn delivery_command(
        &self,
        session_id: &str,
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
            session_id: session_id.to_string(),
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
    pub session_id: String,
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

/// Closed result of reserving the existing dispatch row before root activity
/// admission. No variant authorizes direct execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionUserRunReservation {
    Reserved,
    AlreadyReserved,
    RecoveryClaimed,
    AlreadyActivated { session_activity_epoch: u64 },
    Completed,
}

/// Bind one already-admitted Session activity to its exact reserved Run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionUserRunDelivery {
    pub session_id: String,
    pub run_id: RunId,
    pub session_activity_epoch: u64,
}

/// Closed application result after the durable Run reservation and exact
/// Session activity receipt have both been observed.
///
/// Delivery-bearing variants may be passed to the Host's register-before-
/// activation boundary. Recovery variants retain the same Session/Run identity
/// so a foreground caller can still observe the committed `Awaiting`/`Ended`
/// fact without manufacturing another delivery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionUserRunAdmission {
    Reserved(SessionUserRunDelivery),
    AlreadyReserved(SessionUserRunDelivery),
    AlreadyActivated(SessionUserRunDelivery),
    RecoveryClaimed { session_id: String, run_id: RunId },
    Completed { session_id: String, run_id: RunId },
}

impl SessionUserRunAdmission {
    #[must_use]
    pub fn session_id(&self) -> &str {
        match self {
            Self::Reserved(delivery)
            | Self::AlreadyReserved(delivery)
            | Self::AlreadyActivated(delivery) => &delivery.session_id,
            Self::RecoveryClaimed { session_id, .. } | Self::Completed { session_id, .. } => {
                session_id
            }
        }
    }

    #[must_use]
    pub fn run_id(&self) -> &RunId {
        match self {
            Self::Reserved(delivery)
            | Self::AlreadyReserved(delivery)
            | Self::AlreadyActivated(delivery) => &delivery.run_id,
            Self::RecoveryClaimed { run_id, .. } | Self::Completed { run_id, .. } => run_id,
        }
    }

    #[must_use]
    pub fn delivery(&self) -> Option<&SessionUserRunDelivery> {
        match self {
            Self::Reserved(delivery)
            | Self::AlreadyReserved(delivery)
            | Self::AlreadyActivated(delivery) => Some(delivery),
            Self::RecoveryClaimed { .. } | Self::Completed { .. } => None,
        }
    }
}

/// Closed publication result for a Session User Run reservation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionUserRunActivation {
    Activated,
    AlreadyActivated { session_activity_epoch: u64 },
    RecoveryClaimed,
    Completed,
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
/// immutable Event remains public-history provenance after processing; only
/// this flag may change.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionEventEntry {
    pub event: SessionEventCommand,
    #[serde(default)]
    pub processed: bool,
    /// Absent for unprocessed and legacy commands. Such entries remain durable
    /// provenance but are not listable until the existing effect owner supplies
    /// one immutable commit coordinate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection_anchor: Option<SessionEventProjectionAnchor>,
}

/// Ordered Session Event-batch provenance. Identity, Events, order, optional
/// admission trace context, and the optional create wake epoch are immutable
/// after admission; each entry owns only its independent processed marker, while
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
    pub admitted_revision: crate::SessionRevision,
    /// Optional HTTP retry identity and its canonical request fingerprint. The
    /// pair is retained on this existing Session-root command; there is no
    /// protocol-side idempotency table or second Event store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_fingerprint: Option<String>,
    /// W3C trace context captured once for this atomic ordinary admission. It
    /// is request-local observability provenance, not Event or Run identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
    /// Create-only wake activity opened in the original Session insert and
    /// settled as soon as the first effect has its own durable admission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_activity_epoch: Option<u64>,
}

impl SessionEventBatch {
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

    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.events.iter().all(|entry| entry.processed)
    }

    /// Mark one exact retained entry processed. Exact replay is a no-op; an
    /// operation outside this immutable batch fails closed.
    pub fn mark_processed(
        &mut self,
        operation_id: &str,
        projection_anchor: SessionEventProjectionAnchor,
    ) -> Result<bool, SessionEventBatchError> {
        let entry = self
            .events
            .iter_mut()
            .find(|entry| entry.event.operation_id() == operation_id)
            .ok_or(SessionEventBatchError::ProgressMismatch)?;
        if entry.processed {
            return if entry.projection_anchor == Some(projection_anchor) {
                Ok(false)
            } else {
                Err(SessionEventBatchError::ProgressMismatch)
            };
        }
        entry.projection_anchor = Some(projection_anchor);
        entry.processed = true;
        Ok(true)
    }

    /// Resolve every still-pending accepted command after a legacy terminal
    /// root won. No Runtime effect is executed: the terminal cleanup cursor is
    /// the first immutable visibility anchor when available, while `None`
    /// deliberately preserves the isolated pre-anchor legacy prefix.
    pub fn resolve_terminally(
        &mut self,
        projection_anchor: Option<SessionEventProjectionAnchor>,
    ) -> usize {
        let mut resolved = 0;
        for entry in &mut self.events {
            if !entry.processed {
                entry.projection_anchor = projection_anchor;
                entry.processed = true;
                resolved += 1;
            }
        }
        resolved
    }

    pub fn compile(
        session_id: &str,
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
        session_id: &str,
        batch_id: impl Into<String>,
        inputs: Vec<SessionEventInput>,
        data_subject_id: Option<String>,
        traceparent: Option<String>,
    ) -> Result<Self, SessionEventBatchError> {
        let batch_id = batch_id.into();
        if session_id.trim().is_empty() {
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
                        projection_anchor: None,
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
                        projection_anchor: None,
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
                        projection_anchor: None,
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
                        projection_anchor: None,
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
                        projection_anchor: None,
                    });
                }
            }
        }
        Ok(Self {
            batch_id,
            events,
            admitted_revision: crate::SessionRevision::default(),
            idempotency_key: None,
            request_fingerprint: None,
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
        session_id: &str,
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
    #[error("Session Event interrupt target set is empty or inconsistent")]
    InvalidInterruptTargets,
    #[error("Session initial Event batch contains an Event allowed only by events.send")]
    UnsupportedInitialEvent,
    #[error("Session Event progress does not match the current operation")]
    ProgressMismatch,
    #[error("Session Event idempotency key and request fingerprint must both be non-empty")]
    InvalidIdempotencyCoordinate,
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let plan = SessionInitialEventPlan::compile("session-1", "initial:session-1", inputs)
            .expect("R1 compiles");
        let replay = SessionInitialEventPlan::compile(
            "session-1",
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
            SessionInitialEventPlan::compile("s", "b1", vec![user("u"), system("s")]).is_ok(),
            "S1"
        );
        assert!(
            matches!(
                SessionInitialEventPlan::compile(
                    "s",
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
                    "s",
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
                    "s",
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
                    SessionInitialEventPlan::compile("session", batch_id, vec![input]),
                    Err(SessionEventBatchError::UnsupportedInitialEvent)
                ),
                "O1+O2/E1"
            );
        }

        let tool_batch = SessionEventBatch::compile(
            "session",
            "ordinary-tool",
            vec![tool_reply(), system("reply context")],
        )
        .expect("O3/E2");
        let SessionEventCommand::ToolReply { reply, .. } = &tool_batch.events[0].event else {
            panic!("O3/E2 retained ToolReply")
        };
        let command = reply.delivery_command(
            "session",
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
                reply.delivery_command("session", None).reply,
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
                    "session",
                    "confirmation-system",
                    vec![confirmation, system("not allowed")],
                ),
                Err(SessionEventBatchError::SystemMessagePlacement)
            ),
            "O6/E4 official SDK does not allow System after Confirmation"
        );

        let interrupt_batch =
            SessionEventBatch::compile("session", "ordinary-interrupt", vec![interrupt()])
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
                SessionEventBatch::compile("session", "invalid-tool", vec![invalid_reply]),
                Err(SessionEventBatchError::InvalidToolReplyCoordinate)
            ),
            "O5/E4"
        );
        assert!(
            matches!(
                SessionEventBatch::compile(
                    "session",
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
        let event = session_event_outcome_id("session", "operation");
        assert_eq!(
            event,
            session_event_outcome_id("session", "operation"),
            "C1/E1"
        );
        assert_ne!(event, session_event_outcome_id("session", "other"), "C1/E2");
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
        let batch_id = session_event_batch_id("ordinary", crate::SessionRevision(7)).unwrap();
        assert!(
            batch_id < session_event_batch_id("ordinary", crate::SessionRevision(8)).unwrap(),
            "N1/E1"
        );
        let inputs = (0..=MAX_SESSION_INITIAL_EVENTS)
            .map(|ordinal| user(&format!("message-{ordinal}")))
            .collect::<Vec<_>>();
        let batch = SessionEventBatch::compile_attributed(
            "ordinary",
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
                SessionInitialEventPlan::compile("ordinary", "create", inputs),
                Err(SessionEventBatchError::TooManyEvents)
            ),
            "N2/E2"
        );
        assert!(
            session_event_batch_id("ordinary", crate::SessionRevision(0)).is_err(),
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
            "trace-session",
            "trace-batch",
            vec![user("run"), user("run-two")],
            None,
            Some(traceparent.clone()),
        )
        .expect("T1 traced admission");
        let absent = SessionEventBatch::compile_attributed(
            "trace-session",
            "trace-batch",
            vec![user("run"), user("run-two")],
            None,
            None,
        )
        .expect("T2 untraced admission");
        let changed = SessionEventBatch::compile_attributed(
            "trace-session",
            "trace-batch",
            vec![user("run"), user("run-two")],
            None,
            Some("00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01".into()),
        )
        .expect("T3 retraced admission");
        let initial = SessionInitialEventPlan::compile(
            "trace-session",
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
        let first = session_event_outcome_id("session-1", "operation-1");
        assert_eq!(
            first,
            session_event_outcome_id("session-1", "operation-1"),
            "O1/E1"
        );
        assert!(first.starts_with("outc_"), "O1/E3");
        let second = session_event_outcome_id("session-1", "operation-2");
        assert_ne!(first, second, "O2/E2");
        assert!(second.starts_with("outc_"), "O2/E3");
    }
}
