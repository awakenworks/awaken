//! Storage traits for thread, run record, and persistence.
use super::lifecycle::{RunStatus, TerminationReason};
use super::message::{Message, MessageRecord, Visibility};
use super::suspension::{ToolCallResume, ToolCallResumeMode};
use super::tool::ToolDescriptor;
use crate::state::PersistedState;
use crate::thread::{Thread, normalize_lineage_id};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

mod error;
pub mod message_append;

pub use error::StorageError;

const MESSAGE_CURSOR_PREFIX: &str = "msg_";
const THREAD_CURSOR_PREFIX: &str = "thr_";

// ── run record ──────────────────────────────────────────────────────

/// Origin of a run request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunRequestOrigin {
    /// HTTP API, SDK.
    #[default]
    User,
    Mcp,
    /// Agent-to-Agent protocol.
    A2A,
    /// Child run completion notification, handoff.
    Internal,
}

/// Durable snapshot of the request that created or resumed a run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunRequestSnapshot {
    /// Where this user intent originated.
    #[serde(default = "default_run_origin")]
    pub origin: RunRequestOrigin,
    /// Optional sender/audit identifier from the transport layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_id: Option<String>,
    /// Message ids that triggered this run activation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_message_ids: Vec<String>,
    /// Count of new input messages in this activation.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub input_message_count: u64,
    /// Opaque request extras preserved for protocol adapters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_extras: Option<Value>,
    /// Resume decisions included with this activation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<RunResumeDecision>,
    /// Frontend-defined tools available to this run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub frontend_tools: Vec<ToolDescriptor>,
    /// Parent thread for child-run message routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<String>,
    /// Transport request identifier associated with the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_request_id: Option<String>,
}

fn default_run_origin() -> RunRequestOrigin {
    RunRequestOrigin::User
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

/// Stored resume decision for a suspended tool call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunResumeDecision {
    pub call_id: String,
    pub resume: ToolCallResume,
}

/// Inclusive range of messages in a thread's append-only log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageSeqRange {
    /// 1-based first message sequence number.
    pub from_seq: u64,
    /// 1-based last message sequence number.
    pub to_seq: u64,
}

impl MessageSeqRange {
    /// Create a non-empty inclusive range.
    #[must_use]
    pub fn new(from_seq: u64, to_seq: u64) -> Option<Self> {
        (from_seq > 0 && from_seq <= to_seq).then_some(Self { from_seq, to_seq })
    }

    /// Number of messages covered by this range.
    #[must_use]
    pub fn len(self) -> u64 {
        self.to_seq - self.from_seq + 1
    }

    /// Returns true when the range contains no messages.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.from_seq > self.to_seq
    }
}

/// Message log slice consumed by a run.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunMessageInput {
    /// Thread whose message log is read.
    pub thread_id: String,
    /// Contiguous range read from the thread log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<MessageSeqRange>,
    /// User/input messages that triggered this run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trigger_message_ids: Vec<String>,
    /// Optional explicit selection for non-contiguous reads.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected_message_ids: Vec<String>,
    /// Optional context policy identifier used to build the prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_policy: Option<String>,
    /// Optional compacted context snapshot used instead of raw messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted_snapshot_id: Option<String>,
}

/// Message log slice produced by a run.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunMessageOutput {
    /// Thread whose message log was appended.
    pub thread_id: String,
    /// Contiguous range produced by the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<MessageSeqRange>,
    /// Produced message ids in append order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub message_ids: Vec<String>,
}

/// Why a run is currently waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitingReason {
    ToolPermission,
    UserInput,
    BackgroundTasks,
    ExternalEvent,
    RateLimit,
    ManualPause,
}

/// Durable projection for a non-terminal waiting run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunWaitingTicket {
    /// Stable external ticket id. Prefer the suspension id when one exists.
    pub ticket_id: String,
    /// Runtime tool-call id that owns this pending control point.
    pub tool_call_id: String,
    /// Tool name associated with the pending call.
    pub tool_name: String,
    /// Original tool-call arguments.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub arguments: Value,
    /// Resume mapping strategy needed to continue the run.
    #[serde(default)]
    pub resume_mode: ToolCallResumeMode,
    /// Optional suspension action/reason from the ticket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Unix timestamp (milliseconds) when this ticket was last updated.
    #[serde(default)]
    pub updated_at: u64,
}

/// Durable projection for a non-terminal waiting run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunWaitingState {
    pub reason: WaitingReason,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ticket_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tickets: Vec<RunWaitingTicket>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_dispatch_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Terminal outcome for a run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunOutcome {
    pub termination_reason: TerminationReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_payload: Option<Value>,
}

/// A run record for tracking run history and enabling resume.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunRecord {
    /// Unique run identifier.
    pub run_id: String,
    /// The thread this run belongs to.
    pub thread_id: String,
    /// The agent that executed this run.
    pub agent_id: String,
    /// Parent run identifier for nested/handoff runs.
    pub parent_run_id: Option<String>,
    /// Opaque id of the resolved registry binding frozen for this run. The
    /// server owns the referenced content; the runtime treats it as opaque.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation: Option<super::run::RunActivationSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RunRequestSnapshot>,
    /// Messages read by this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<RunMessageInput>,
    /// Messages produced by this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<RunMessageOutput>,
    /// Current status of the run.
    pub status: RunStatus,
    /// Structured termination reason for completed or waiting runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination_reason: Option<TerminationReason>,
    /// Final text response, when the run produced one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_output: Option<String>,
    /// Structured error payload, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_payload: Option<Value>,
    /// Queue dispatch that delivered this run, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_id: Option<String>,
    /// External session/dispatch identifier associated with this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Transport request identifier associated with this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport_request_id: Option<String>,
    /// Structured waiting state for non-terminal suspended runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<RunWaitingState>,
    /// Structured terminal outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<RunOutcome>,
    /// Unix timestamp (seconds) when the run was created.
    pub created_at: u64,
    /// Unix timestamp (seconds) when execution first started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    /// Unix timestamp (seconds) when execution reached a terminal state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    /// Unix timestamp (seconds) of the last update.
    pub updated_at: u64,
    /// Number of steps (rounds) completed.
    pub steps: usize,
    /// Total input tokens consumed.
    pub input_tokens: u64,
    /// Total output tokens consumed.
    pub output_tokens: u64,
    /// State snapshot for resume.
    pub state: Option<PersistedState>,
}

impl RunRecord {
    /// Return the structured waiting reason for a non-terminal run.
    ///
    /// Waiting state is durable and structured. Runtime status reason strings
    /// are not used for same-run resume.
    #[must_use]
    pub fn waiting_reason(&self) -> Option<WaitingReason> {
        if self.status != RunStatus::Waiting {
            return None;
        }

        self.waiting.as_ref().map(|waiting| waiting.reason)
    }

    /// Return true when this waiting run can be resumed as the same user intent.
    #[must_use]
    pub fn is_resumable_waiting(&self) -> bool {
        self.waiting_reason().is_some()
    }

    /// Return true when startup recovery should enqueue an internal background wake.
    #[must_use]
    pub fn is_background_task_waiting(&self) -> bool {
        self.waiting_reason() == Some(WaitingReason::BackgroundTasks)
    }
}

// ── query types ─────────────────────────────────────────────────────

/// Pagination/filter query for listing messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageQuery {
    /// Number of items to skip.
    pub offset: usize,
    /// Maximum number of items to return.
    pub limit: usize,
    /// Return records with sequence numbers greater than this value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<u64>,
    /// Return records with sequence numbers less than this value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<u64>,
    /// Sort order for message sequence numbers.
    #[serde(default)]
    pub order: MessageOrder,
    /// Visibility filter applied before pagination.
    #[serde(default)]
    pub visibility: MessageVisibilityFilter,
    /// Filter by producing run ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

impl Default for MessageQuery {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: 50,
            after: None,
            before: None,
            order: MessageOrder::Asc,
            visibility: MessageVisibilityFilter::Any,
            run_id: None,
        }
    }
}

impl MessageQuery {
    /// Return a copy with contract-level pagination limits applied.
    #[must_use]
    pub fn normalized(&self) -> Self {
        Self {
            offset: self.offset,
            limit: self.limit.min(200),
            after: self.after,
            before: self.before,
            order: self.order,
            visibility: self.visibility,
            run_id: self.run_id.clone(),
        }
    }

    /// Encode an opaque cursor for continuing this exact query.
    #[must_use]
    pub fn encode_cursor(&self, offset: usize) -> String {
        let normalized = self.normalized();
        encode_cursor_token(
            MESSAGE_CURSOR_PREFIX,
            &MessageCursorToken {
                offset,
                after: normalized.after,
                before: normalized.before,
                order: normalized.order,
                visibility: normalized.visibility,
                run_id: normalized.run_id,
            },
        )
    }

    /// Decode a cursor and verify it belongs to this exact query shape.
    pub fn decode_cursor(&self, cursor: &str) -> Result<usize, String> {
        if let Ok(offset) = cursor.parse::<usize>() {
            return Ok(offset);
        }

        let normalized = self.normalized();
        let token: MessageCursorToken = decode_cursor_token(MESSAGE_CURSOR_PREFIX, cursor)?;
        if token.after != normalized.after
            || token.before != normalized.before
            || token.order != normalized.order
            || token.visibility != normalized.visibility
            || token.run_id != normalized.run_id
        {
            return Err("cursor does not match message query filters".to_string());
        }
        Ok(token.offset)
    }

    /// Return true when a record passes the query filters.
    #[must_use]
    pub fn matches_record(&self, record: &MessageRecord) -> bool {
        if self.after.is_some_and(|after| record.seq <= after) {
            return false;
        }
        if self.before.is_some_and(|before| record.seq >= before) {
            return false;
        }
        if self
            .run_id
            .as_deref()
            .is_some_and(|run_id| record.produced_by_run_id.as_deref() != Some(run_id))
        {
            return false;
        }
        match self.visibility {
            MessageVisibilityFilter::Any => true,
            MessageVisibilityFilter::External => record.message.visibility != Visibility::Internal,
            MessageVisibilityFilter::Internal => record.message.visibility == Visibility::Internal,
        }
    }
}

/// Message sequence ordering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageOrder {
    /// Oldest message first.
    #[default]
    Asc,
    /// Newest message first.
    Desc,
}

/// Message visibility filter for storage queries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageVisibilityFilter {
    /// Include all stored messages.
    #[default]
    Any,
    /// Include externally visible messages.
    External,
    /// Include internal-only messages.
    Internal,
}

/// Paginated message record response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagePage {
    pub records: Vec<MessageRecord>,
    pub total: usize,
    pub has_more: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_cursor: Option<String>,
}

impl MessagePage {
    /// Empty page for a missing thread or message log.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            records: Vec::new(),
            total: 0,
            has_more: false,
            next_cursor: None,
            prev_cursor: None,
        }
    }
}

/// Parent/root lineage filter for thread queries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadParentFilter {
    /// Do not filter by parent linkage.
    #[default]
    Any,
    /// Restrict results to root threads with no parent.
    Root,
    /// Restrict results to direct children of the specified parent thread.
    Parent(String),
}

impl ThreadParentFilter {
    #[must_use]
    pub fn is_any(&self) -> bool {
        matches!(self, Self::Any)
    }

    #[must_use]
    pub fn normalized(&self) -> Self {
        match self {
            Self::Any => Self::Any,
            Self::Root => Self::Root,
            Self::Parent(parent_thread_id) => normalize_lineage_id(Some(parent_thread_id))
                .map(Self::Parent)
                .unwrap_or(Self::Any),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct MessageCursorToken {
    offset: usize,
    after: Option<u64>,
    before: Option<u64>,
    order: MessageOrder,
    visibility: MessageVisibilityFilter,
    run_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ThreadCursorToken {
    offset: usize,
    resource_id: Option<String>,
    parent_filter: ThreadParentFilter,
}

/// Pagination/filter query for listing threads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadQuery {
    /// Number of items to skip after filtering.
    pub offset: usize,
    /// Maximum number of items to return.
    pub limit: usize,
    /// Filter by external resource grouping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<String>,
    /// Filter by parent/root lineage.
    #[serde(default, skip_serializing_if = "ThreadParentFilter::is_any")]
    pub parent_filter: ThreadParentFilter,
}

impl Default for ThreadQuery {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: 50,
            resource_id: None,
            parent_filter: ThreadParentFilter::Any,
        }
    }
}

impl ThreadQuery {
    /// Return true when the query carries any non-pagination filter.
    #[must_use]
    pub fn has_filters(&self) -> bool {
        normalize_lineage_id(self.resource_id.as_deref()).is_some() || !self.parent_filter.is_any()
    }

    /// Return a copy with normalized lineage filters.
    #[must_use]
    pub fn normalized(&self) -> Self {
        Self {
            offset: self.offset,
            limit: self.limit.min(200),
            resource_id: normalize_lineage_id(self.resource_id.as_deref()),
            parent_filter: self.parent_filter.normalized(),
        }
    }

    /// Encode an opaque cursor for continuing this exact query.
    #[must_use]
    pub fn encode_cursor(&self, offset: usize) -> String {
        let normalized = self.normalized();
        encode_cursor_token(
            THREAD_CURSOR_PREFIX,
            &ThreadCursorToken {
                offset,
                resource_id: normalized.resource_id,
                parent_filter: normalized.parent_filter,
            },
        )
    }

    /// Decode a cursor and verify it belongs to this exact query shape.
    pub fn decode_cursor(&self, cursor: &str) -> Result<usize, String> {
        if let Ok(offset) = cursor.parse::<usize>() {
            return Ok(offset);
        }

        let normalized = self.normalized();
        let token: ThreadCursorToken = decode_cursor_token(THREAD_CURSOR_PREFIX, cursor)?;
        if token.resource_id != normalized.resource_id
            || token.parent_filter != normalized.parent_filter
        {
            return Err("cursor does not match thread query filters".to_string());
        }
        Ok(token.offset)
    }

    /// Return true when a thread passes the query filters.
    #[must_use]
    pub fn matches_thread(&self, thread: &Thread) -> bool {
        let normalized = self.normalized();
        if normalized
            .resource_id
            .as_deref()
            .is_some_and(|resource_id| {
                normalize_lineage_id(thread.resource_id.as_deref()).as_deref() != Some(resource_id)
            })
        {
            return false;
        }
        match &normalized.parent_filter {
            ThreadParentFilter::Any => {}
            ThreadParentFilter::Root => {
                if normalize_lineage_id(thread.parent_thread_id.as_deref()).is_some() {
                    return false;
                }
            }
            ThreadParentFilter::Parent(parent_thread_id) => {
                if normalize_lineage_id(thread.parent_thread_id.as_deref()).as_deref()
                    != Some(parent_thread_id.as_str())
                {
                    return false;
                }
            }
        }
        true
    }
}

/// Paginated thread ID response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadPage {
    pub items: Vec<String>,
    pub total: usize,
    pub has_more: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_cursor: Option<String>,
}

impl ThreadPage {
    /// Empty thread page.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            items: Vec::new(),
            total: 0,
            has_more: false,
            next_cursor: None,
            prev_cursor: None,
        }
    }
}

/// How deleting a thread should treat direct and transitive child threads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildThreadDeleteStrategy {
    /// Reject deletion when at least one direct child exists.
    Reject,
    /// Preserve child threads and clear their `parent_thread_id`.
    #[default]
    Detach,
    /// Recursively delete all descendants before deleting the target thread.
    Cascade,
}

/// Parent thread that should be materialized by a checkpoint projection.
#[must_use]
pub fn checkpoint_parent_thread_id<'a>(
    existing_thread: Option<&'a Thread>,
    run: &'a RunRecord,
) -> Option<&'a str> {
    existing_thread
        .and_then(|thread| thread.parent_thread_id.as_deref())
        .or_else(|| {
            run.request
                .as_ref()
                .and_then(|request| request.parent_thread_id.as_deref())
        })
}

/// Sort threads by recent activity, then ID for deterministic ties.
pub fn sort_threads_by_recent_activity(threads: &mut [Thread]) {
    threads.sort_by(|a, b| {
        let a_updated = a.metadata.updated_at.or(a.metadata.created_at).unwrap_or(0);
        let b_updated = b.metadata.updated_at.or(b.metadata.created_at).unwrap_or(0);
        b_updated.cmp(&a_updated).then_with(|| a.id.cmp(&b.id))
    });
}

/// Apply thread filters and offset pagination to an in-memory thread set.
#[must_use]
pub fn paginate_threads(mut threads: Vec<Thread>, query: &ThreadQuery) -> ThreadPage {
    let query = query.normalized();
    sort_threads_by_recent_activity(&mut threads);
    let filtered: Vec<Thread> = threads
        .into_iter()
        .filter(|thread| query.matches_thread(thread))
        .collect();
    let total = filtered.len();
    let start = query.offset.min(total);
    let items: Vec<String> = filtered
        .into_iter()
        .skip(start)
        .take(query.limit)
        .map(|thread| thread.id)
        .collect();
    let next_offset = start + items.len();
    let has_more = query.limit > 0 && next_offset < total;
    ThreadPage {
        items,
        total,
        has_more,
        next_cursor: has_more.then(|| query.encode_cursor(next_offset)),
        prev_cursor: (query.limit > 0 && start > 0)
            .then(|| query.encode_cursor(start.saturating_sub(query.limit))),
    }
}

/// Apply message filters and offset pagination to an in-memory record set.
#[must_use]
pub fn paginate_message_records(
    mut records: Vec<MessageRecord>,
    query: &MessageQuery,
) -> MessagePage {
    let query = query.normalized();
    records.retain(|record| query.matches_record(record));
    match query.order {
        MessageOrder::Asc => records.sort_by_key(|record| record.seq),
        MessageOrder::Desc => records.sort_by(|a, b| b.seq.cmp(&a.seq)),
    }
    let total = records.len();
    let start = query.offset.min(total);
    let page_records: Vec<MessageRecord> =
        records.into_iter().skip(start).take(query.limit).collect();
    let next_offset = start + page_records.len();
    let has_more = query.limit > 0 && next_offset < total;
    MessagePage {
        records: page_records,
        total,
        has_more,
        next_cursor: has_more.then(|| query.encode_cursor(next_offset)),
        prev_cursor: (query.limit > 0 && start > 0)
            .then(|| query.encode_cursor(start.saturating_sub(query.limit))),
    }
}

fn encode_cursor_token<T: Serialize>(prefix: &str, token: &T) -> String {
    let bytes = serde_json::to_vec(token).expect("cursor token serialization should succeed");
    format!("{prefix}{}", URL_SAFE_NO_PAD.encode(bytes))
}

fn decode_cursor_token<T: DeserializeOwned>(prefix: &str, cursor: &str) -> Result<T, String> {
    let payload = cursor
        .strip_prefix(prefix)
        .ok_or_else(|| "cursor must be a valid pagination token".to_string())?;
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| "cursor must be a valid pagination token".to_string())?;
    serde_json::from_slice(&decoded)
        .map_err(|_| "cursor must be a valid pagination token".to_string())
}

/// Pagination/filter query for listing runs.
#[derive(Debug, Clone)]
pub struct RunQuery {
    /// Number of items to skip.
    pub offset: usize,
    /// Maximum number of items to return.
    pub limit: usize,
    /// Filter by thread ID.
    pub thread_id: Option<String>,
    /// Filter by run status.
    pub status: Option<RunStatus>,
}

impl Default for RunQuery {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: 50,
            thread_id: None,
            status: None,
        }
    }
}

/// Paginated run list response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunPage {
    pub items: Vec<RunRecord>,
    pub total: usize,
    pub has_more: bool,
}

// ── runtime read port ────────────────────────────────────────────────

/// Narrow read port the runtime needs from durable storage during a run:
/// the handful of resume reads (thread, messages, run records). The full
/// `ThreadStore`/`RunStore`/`ThreadRunStore` CRUD + query surface is a
/// server/store concern and is not exposed to the runtime through this port.
#[async_trait]
pub trait RuntimeCheckpointStore: Send + Sync {
    async fn load_thread(&self, thread_id: &str) -> Result<Option<Thread>, StorageError>;

    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError>;

    async fn load_committed_messages(
        &self,
        thread_id: &str,
    ) -> Result<Option<Vec<Message>>, StorageError>;

    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError>;

    async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError>;
}

#[cfg(test)]
mod tests;
