//! Run-record data + the narrow runtime checkpoint read port.
//!
//! The full thread/run CRUD query/page/pagination vocabulary is a server/store
//! concern and lives in `awaken-server-contract`. runtime-contract keeps only
//! what the runtime engine consumes: the `RunRecord` data model (named by the
//! commit plan) and the `RuntimeCheckpointStore` resume read port.
use super::lifecycle::{RunStatus, TerminationReason};
use super::message::Message;
use super::suspension::{ToolCallResume, ToolCallResumeMode};
use super::tool::ToolDescriptor;
use crate::state::PersistedState;
use crate::thread::Thread;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

mod error;
pub mod message_append;

pub use error::StorageError;

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
