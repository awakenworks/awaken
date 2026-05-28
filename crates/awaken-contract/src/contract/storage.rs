//! Storage traits for thread, run record, and persistence.
use super::lifecycle::{RunStatus, TerminationReason};
use super::message::{Message, MessageRecord, Visibility};
use super::scope::{ScopeId, scoped_key, unscoped_key};
use super::suspension::{ToolCallResume, ToolCallResumeMode};
use super::tool::ToolDescriptor;
pub use super::versioned_registry::{PinnedRegistryEntry, PinnedRegistryManifest};
use crate::state::PersistedState;
use crate::thread::{Thread, normalize_lineage_id};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

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
    /// Published runtime-config versions frozen for this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_manifest: Option<PinnedRegistryManifest>,
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

#[derive(Clone)]
pub struct ScopedThreadRunStore {
    inner: Arc<dyn ThreadRunStore>,
    scope_id: ScopeId,
}

impl ScopedThreadRunStore {
    pub fn new(inner: Arc<dyn ThreadRunStore>, scope_id: ScopeId) -> Self {
        Self { inner, scope_id }
    }

    pub fn scope_id(&self) -> &ScopeId {
        &self.scope_id
    }

    pub fn inner(&self) -> &dyn ThreadRunStore {
        self.inner.as_ref()
    }

    fn scoped(&self, id: &str) -> String {
        scoped_key(&self.scope_id, id)
    }

    fn unscoped<'a>(&self, id: &'a str) -> Option<&'a str> {
        unscoped_key(&self.scope_id, id)
    }

    fn encode_thread(&self, thread: &Thread) -> Thread {
        let mut thread = thread.clone();
        thread.id = self.scoped(&thread.id);
        thread.parent_thread_id = thread.parent_thread_id.as_deref().map(|id| self.scoped(id));
        thread
    }

    fn decode_thread(&self, mut thread: Thread) -> Option<Thread> {
        thread.id = self.unscoped(&thread.id)?.to_string();
        thread.parent_thread_id = match thread.parent_thread_id.as_deref() {
            Some(id) => Some(self.unscoped(id)?.to_string()),
            None => None,
        };
        Some(thread)
    }

    fn encode_run(&self, run: &RunRecord) -> RunRecord {
        let mut run = run.clone();
        run.run_id = self.scoped(&run.run_id);
        run.thread_id = self.scoped(&run.thread_id);
        run.parent_run_id = run.parent_run_id.as_deref().map(|id| self.scoped(id));
        if let Some(input) = run.input.as_mut() {
            input.thread_id = self.scoped(&input.thread_id);
        }
        if let Some(output) = run.output.as_mut() {
            output.thread_id = self.scoped(&output.thread_id);
        }
        if let Some(request) = run.request.as_mut() {
            request.parent_thread_id = request
                .parent_thread_id
                .as_deref()
                .map(|id| self.scoped(id));
        }
        run
    }

    fn decode_run(&self, mut run: RunRecord) -> Option<RunRecord> {
        run.run_id = self.unscoped(&run.run_id)?.to_string();
        run.thread_id = self.unscoped(&run.thread_id)?.to_string();
        run.parent_run_id = match run.parent_run_id.as_deref() {
            Some(id) => Some(self.unscoped(id)?.to_string()),
            None => None,
        };
        if let Some(input) = run.input.as_mut() {
            input.thread_id = self.unscoped(&input.thread_id)?.to_string();
        }
        if let Some(output) = run.output.as_mut() {
            output.thread_id = self.unscoped(&output.thread_id)?.to_string();
        }
        if let Some(request) = run.request.as_mut() {
            request.parent_thread_id = match request.parent_thread_id.as_deref() {
                Some(id) => Some(self.unscoped(id)?.to_string()),
                None => None,
            };
        }
        Some(run)
    }

    fn decode_message_record(&self, mut record: MessageRecord) -> Option<MessageRecord> {
        record.thread_id = self.unscoped(&record.thread_id)?.to_string();
        if let Some(run_id) = record.produced_by_run_id.as_deref()
            && let Some(unscoped) = self.unscoped(run_id)
        {
            record.produced_by_run_id = Some(unscoped.to_string());
        }
        Some(record)
    }

    fn encode_message_query(&self, query: &MessageQuery) -> MessageQuery {
        let mut query = query.clone();
        query.run_id = query.run_id.as_deref().map(|id| self.scoped(id));
        query
    }
}

// ── ThreadStore ─────────────────────────────────────────────────────

/// Thread read/write persistence.
///
/// Thread metadata and messages are stored separately. Messages have a
/// single source of truth through `load_messages` / `save_messages`.
#[async_trait]
pub trait ThreadStore: Send + Sync {
    /// Load a thread by ID. Returns `None` if not found.
    async fn load_thread(&self, thread_id: &str) -> Result<Option<Thread>, StorageError>;

    /// Persist a thread (create or overwrite).
    ///
    /// This is a low-level persistence primitive. Callers that change
    /// parent-child relationships should use [`ThreadStore::save_thread_validated`]
    /// so hierarchy invariants are checked against current store state.
    async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError>;

    /// Persist a thread after validating parent-child hierarchy invariants.
    ///
    /// The default implementation validates and then delegates to
    /// [`ThreadStore::save_thread`]. It is not atomic across those steps.
    /// with a backend-native atomic or fenced implementation.
    async fn save_thread_validated(&self, thread: &Thread) -> Result<(), StorageError> {
        self.validate_thread_hierarchy(&thread.id, thread.parent_thread_id.as_deref())
            .await?;
        self.save_thread(thread).await
    }

    /// Delete a thread and its associated messages.
    ///
    /// This is a low-level delete primitive. Callers that need hierarchy-aware
    /// child handling should use [`ThreadStore::delete_thread_with_strategy`].
    async fn delete_thread(&self, thread_id: &str) -> Result<(), StorageError>;

    /// Delete a thread while managing direct and transitive children.
    ///
    /// The default implementation performs multiple low-level writes and is
    /// not atomic across child updates and the final delete. Production stores
    /// with concurrent writers should override this method with a transactional
    /// or otherwise fenced implementation.
    async fn delete_thread_with_strategy(
        &self,
        thread_id: &str,
        strategy: ChildThreadDeleteStrategy,
    ) -> Result<(), StorageError> {
        if self.load_thread(thread_id).await?.is_none() {
            return Err(StorageError::NotFound(thread_id.to_owned()));
        }

        match strategy {
            ChildThreadDeleteStrategy::Reject => {
                let children = self.list_child_threads(thread_id).await?;
                if !children.is_empty() {
                    return Err(StorageError::Validation(format!(
                        "thread '{thread_id}' has child threads; choose 'detach' or 'cascade'"
                    )));
                }
                self.delete_thread(thread_id).await
            }
            ChildThreadDeleteStrategy::Detach => {
                let mut children = self.list_child_threads(thread_id).await?;
                let updated_at = crate::now_ms();
                for child in &mut children {
                    child.parent_thread_id = None;
                    child.metadata.updated_at = Some(updated_at);
                    self.save_thread(child).await?;
                }
                self.delete_thread(thread_id).await
            }
            ChildThreadDeleteStrategy::Cascade => {
                let mut visited = std::collections::HashSet::new();
                let mut stack = vec![(thread_id.to_owned(), false)];
                let mut delete_order = Vec::new();

                while let Some((current_thread_id, expanded)) = stack.pop() {
                    if expanded {
                        delete_order.push(current_thread_id);
                        continue;
                    }

                    if !visited.insert(current_thread_id.clone()) {
                        return Err(StorageError::Validation(format!(
                            "thread hierarchy cycle detected while deleting '{thread_id}'"
                        )));
                    }

                    stack.push((current_thread_id.clone(), true));
                    let mut children = self.list_child_threads(&current_thread_id).await?;
                    children.sort_by(|left, right| left.id.cmp(&right.id));
                    for child in children.into_iter().rev() {
                        stack.push((child.id, false));
                    }
                }

                for id in delete_order {
                    self.delete_thread(&id).await?;
                }
                Ok(())
            }
        }
    }

    /// List thread IDs with pagination.
    async fn list_threads(&self, offset: usize, limit: usize) -> Result<Vec<String>, StorageError>;

    /// List thread IDs with first-class filters and page metadata.
    async fn list_threads_query(&self, query: &ThreadQuery) -> Result<ThreadPage, StorageError> {
        const SCAN_LIMIT: usize = 200;

        let mut offset = 0;
        let mut threads = Vec::new();
        loop {
            let ids = self.list_threads(offset, SCAN_LIMIT).await?;
            if ids.is_empty() {
                break;
            }
            let count = ids.len();
            for id in ids {
                if let Some(thread) = self.load_thread(&id).await? {
                    threads.push(thread);
                }
            }
            if count < SCAN_LIMIT {
                break;
            }
            offset += count;
        }

        Ok(paginate_threads(threads, query))
    }

    /// Load all direct child threads for a given parent thread.
    async fn list_child_threads(
        &self,
        parent_thread_id: &str,
    ) -> Result<Vec<Thread>, StorageError> {
        const PAGE_LIMIT: usize = 200;

        let mut offset = 0;
        let mut children = Vec::new();
        loop {
            let query = ThreadQuery {
                offset,
                limit: PAGE_LIMIT,
                resource_id: None,
                parent_filter: ThreadParentFilter::Parent(parent_thread_id.to_owned()),
            };
            let page = self.list_threads_query(&query).await?;
            let count = page.items.len();
            for id in page.items {
                if let Some(thread) = self.load_thread(&id).await? {
                    children.push(thread);
                }
            }
            if !page.has_more || count == 0 {
                break;
            }
            offset = page
                .next_cursor
                .as_deref()
                .and_then(|cursor| query.decode_cursor(cursor).ok())
                .unwrap_or(offset.saturating_add(count));
        }
        Ok(children)
    }

    /// Validate parent-child hierarchy invariants for a thread.
    async fn validate_thread_hierarchy(
        &self,
        thread_id: &str,
        parent_thread_id: Option<&str>,
    ) -> Result<(), StorageError> {
        let Some(parent_thread_id) = normalize_lineage_id(parent_thread_id) else {
            return Ok(());
        };
        if parent_thread_id == thread_id {
            return Err(StorageError::Validation(format!(
                "thread '{thread_id}' cannot parent itself"
            )));
        }

        let root_parent_thread_id = parent_thread_id.to_owned();
        let mut current_thread_id = root_parent_thread_id.clone();
        let mut visited = std::collections::HashSet::from([thread_id.to_owned()]);

        loop {
            if !visited.insert(current_thread_id.clone()) {
                return Err(StorageError::Validation(format!(
                    "thread hierarchy cycle detected at '{current_thread_id}'"
                )));
            }

            let Some(thread) = self.load_thread(&current_thread_id).await? else {
                let message = if current_thread_id == root_parent_thread_id {
                    format!("parent thread not found: {root_parent_thread_id}")
                } else {
                    format!("thread hierarchy references missing ancestor '{current_thread_id}'")
                };
                return Err(StorageError::Validation(message));
            };

            let Some(next_parent_thread_id) =
                normalize_lineage_id(thread.parent_thread_id.as_deref())
            else {
                return Ok(());
            };
            current_thread_id = next_parent_thread_id;
        }
    }

    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError>;

    async fn load_committed_messages(
        &self,
        thread_id: &str,
    ) -> Result<Option<Vec<Message>>, StorageError> {
        self.load_messages(thread_id).await
    }

    async fn load_message_records(
        &self,
        thread_id: &str,
    ) -> Result<Option<Vec<MessageRecord>>, StorageError> {
        let Some(messages) = self.load_messages(thread_id).await? else {
            return Ok(None);
        };
        Ok(Some(
            messages
                .into_iter()
                .enumerate()
                .map(|(index, message)| {
                    MessageRecord::from_message(thread_id.to_string(), index as u64 + 1, message)
                })
                .collect(),
        ))
    }

    /// List thread-owned message records with filtering and page metadata.
    async fn list_message_records(
        &self,
        thread_id: &str,
        query: &MessageQuery,
    ) -> Result<MessagePage, StorageError> {
        let Some(records) = self.load_message_records(thread_id).await? else {
            return Ok(MessagePage::empty());
        };
        Ok(paginate_message_records(records, query))
    }

    /// Append messages to a thread's durable log and return their records.
    async fn append_message_records(
        &self,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<Vec<MessageRecord>, StorageError> {
        let mut existing = self
            .load_committed_messages(thread_id)
            .await?
            .unwrap_or_default();
        message_append::validate_append_only_delta(&existing, messages)?;
        let start_seq = existing.len() as u64 + 1;
        existing.extend(messages.iter().cloned());
        self.save_messages(thread_id, &existing).await?;
        Ok(messages
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, message)| {
                MessageRecord::from_message(
                    thread_id.to_string(),
                    start_seq + index as u64,
                    message,
                )
            })
            .collect())
    }

    /// Load one message record by message ID.
    async fn load_message_record(
        &self,
        thread_id: &str,
        message_id: &str,
    ) -> Result<Option<MessageRecord>, StorageError> {
        let Some(records) = self.load_message_records(thread_id).await? else {
            return Ok(None);
        };
        Ok(records
            .into_iter()
            .find(|record| record.message_id == message_id))
    }

    /// Load message records by inclusive sequence range.
    async fn load_message_records_range(
        &self,
        thread_id: &str,
        range: MessageSeqRange,
    ) -> Result<Vec<MessageRecord>, StorageError> {
        let Some(records) = self.load_message_records(thread_id).await? else {
            return Ok(Vec::new());
        };
        Ok(records
            .into_iter()
            .filter(|record| record.seq >= range.from_seq && record.seq <= range.to_seq)
            .collect())
    }

    /// Persist messages for a thread (full overwrite).
    async fn save_messages(
        &self,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<(), StorageError>;

    /// Delete all messages for a thread. Returns `NotFound` if the thread does not exist.
    async fn delete_messages(&self, thread_id: &str) -> Result<(), StorageError>;

    /// Update only the metadata of an existing thread.
    /// Returns `NotFound` if the thread does not exist.
    async fn update_thread_metadata(
        &self,
        id: &str,
        metadata: crate::thread::ThreadMetadata,
    ) -> Result<(), StorageError>;
}

// ── RunStore ────────────────────────────────────────────────────────

/// Run record persistence.
#[async_trait]
pub trait RunStore: Send + Sync {
    /// Create a new run record.
    async fn create_run(&self, record: &RunRecord) -> Result<(), StorageError>;

    /// Load a run record by `run_id`.
    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError>;

    /// Find the latest run for a thread (by `updated_at`).
    async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError>;

    /// List runs with optional filtering and pagination.
    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, StorageError>;
}

// ── ThreadRunStore (convenience) ────────────────────────────────────

/// Atomic thread+run checkpoint persistence. ADR-0038 D7: prefer
/// [`CommitCoordinator::commit_checkpoint`](super::commit_coordinator::CommitCoordinator::commit_checkpoint)
/// for production writes; `checkpoint` is retained for conformance tests
/// and coordinator-internal use.
#[async_trait]
pub trait ThreadRunStore: ThreadStore + RunStore + Send + Sync {
    #[deprecated(since = "0.6.0", note = "use CommitCoordinator (ADR-0038 D7)")]
    async fn checkpoint(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), StorageError>;

    /// Append to the committed log and persist `run`, guarded by message count.
    #[allow(deprecated)]
    async fn checkpoint_append(
        &self,
        thread_id: &str,
        messages: &[Message],
        expected_version: Option<u64>,
        run: &RunRecord,
    ) -> Result<u64, StorageError> {
        let existing = self
            .load_committed_messages(thread_id)
            .await?
            .unwrap_or_default();
        let actual = existing.len() as u64;
        if let Some(expected) = expected_version
            && expected != actual
        {
            return Err(StorageError::VersionConflict { expected, actual });
        }
        let mut merged = existing;
        message_append::merge_checkpoint_append_messages(&mut merged, messages)?;
        let new_version = merged.len() as u64;
        self.checkpoint(thread_id, &merged, run).await?;
        Ok(new_version)
    }
}

#[async_trait]
impl ThreadStore for ScopedThreadRunStore {
    async fn load_thread(&self, thread_id: &str) -> Result<Option<Thread>, StorageError> {
        Ok(self
            .inner
            .load_thread(&self.scoped(thread_id))
            .await?
            .and_then(|thread| self.decode_thread(thread)))
    }

    async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError> {
        self.inner.save_thread(&self.encode_thread(thread)).await
    }

    async fn delete_thread(&self, thread_id: &str) -> Result<(), StorageError> {
        self.inner.delete_thread(&self.scoped(thread_id)).await
    }

    async fn list_threads(&self, offset: usize, limit: usize) -> Result<Vec<String>, StorageError> {
        const SCAN_LIMIT: usize = 200;

        let mut inner_offset = 0;
        let mut scoped_ids = Vec::new();
        loop {
            let ids = self.inner.list_threads(inner_offset, SCAN_LIMIT).await?;
            if ids.is_empty() {
                break;
            }
            let count = ids.len();
            scoped_ids.extend(
                ids.into_iter()
                    .filter_map(|id| self.unscoped(&id).map(str::to_string)),
            );
            if count < SCAN_LIMIT {
                break;
            }
            inner_offset += count;
        }

        Ok(scoped_ids.into_iter().skip(offset).take(limit).collect())
    }

    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError> {
        self.inner.load_messages(&self.scoped(thread_id)).await
    }

    async fn load_committed_messages(
        &self,
        thread_id: &str,
    ) -> Result<Option<Vec<Message>>, StorageError> {
        self.inner
            .load_committed_messages(&self.scoped(thread_id))
            .await
    }

    async fn load_message_records(
        &self,
        thread_id: &str,
    ) -> Result<Option<Vec<MessageRecord>>, StorageError> {
        Ok(self
            .inner
            .load_message_records(&self.scoped(thread_id))
            .await?
            .map(|records| {
                records
                    .into_iter()
                    .filter_map(|record| self.decode_message_record(record))
                    .collect()
            }))
    }

    async fn list_message_records(
        &self,
        thread_id: &str,
        query: &MessageQuery,
    ) -> Result<MessagePage, StorageError> {
        let query = self.encode_message_query(query);
        let mut page = self
            .inner
            .list_message_records(&self.scoped(thread_id), &query)
            .await?;
        page.records = page
            .records
            .into_iter()
            .filter_map(|record| self.decode_message_record(record))
            .collect();
        Ok(page)
    }

    async fn append_message_records(
        &self,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<Vec<MessageRecord>, StorageError> {
        Ok(self
            .inner
            .append_message_records(&self.scoped(thread_id), messages)
            .await?
            .into_iter()
            .filter_map(|record| self.decode_message_record(record))
            .collect())
    }

    async fn save_messages(
        &self,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<(), StorageError> {
        self.inner
            .save_messages(&self.scoped(thread_id), messages)
            .await
    }

    async fn delete_messages(&self, thread_id: &str) -> Result<(), StorageError> {
        self.inner.delete_messages(&self.scoped(thread_id)).await
    }

    async fn update_thread_metadata(
        &self,
        id: &str,
        metadata: crate::thread::ThreadMetadata,
    ) -> Result<(), StorageError> {
        self.inner
            .update_thread_metadata(&self.scoped(id), metadata)
            .await
    }
}

#[async_trait]
impl RunStore for ScopedThreadRunStore {
    async fn create_run(&self, record: &RunRecord) -> Result<(), StorageError> {
        self.inner.create_run(&self.encode_run(record)).await
    }

    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError> {
        Ok(self
            .inner
            .load_run(&self.scoped(run_id))
            .await?
            .and_then(|record| self.decode_run(record)))
    }

    async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError> {
        Ok(self
            .inner
            .latest_run(&self.scoped(thread_id))
            .await?
            .and_then(|record| self.decode_run(record)))
    }

    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, StorageError> {
        let inner_query = RunQuery {
            offset: 0,
            limit: usize::MAX,
            thread_id: query.thread_id.as_deref().map(|id| self.scoped(id)),
            status: query.status,
        };
        let inner_page = self.inner.list_runs(&inner_query).await?;
        let mut items: Vec<RunRecord> = inner_page
            .items
            .into_iter()
            .filter_map(|record| self.decode_run(record))
            .collect();
        let total = items.len();
        let start = query.offset.min(total);
        items = items.into_iter().skip(start).take(query.limit).collect();
        let has_more = query.limit > 0 && start + items.len() < total;
        Ok(RunPage {
            items,
            total,
            has_more,
        })
    }
}

#[async_trait]
impl ThreadRunStore for ScopedThreadRunStore {
    #[allow(deprecated)]
    async fn checkpoint(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), StorageError> {
        self.inner
            .checkpoint(&self.scoped(thread_id), messages, &self.encode_run(run))
            .await
    }

    async fn checkpoint_append(
        &self,
        thread_id: &str,
        messages: &[Message],
        expected_version: Option<u64>,
        run: &RunRecord,
    ) -> Result<u64, StorageError> {
        self.inner
            .checkpoint_append(
                &self.scoped(thread_id),
                messages,
                expected_version,
                &self.encode_run(run),
            )
            .await
    }
}

#[cfg(test)]
mod tests;
