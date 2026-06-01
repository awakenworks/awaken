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
    /// Validate storage-level invariants before a run is persisted.
    pub fn validate_for_persist(&self) -> Result<(), StorageError> {
        require_non_empty("run_id", &self.run_id)?;
        require_non_empty("thread_id", &self.thread_id)?;
        require_non_empty("agent_id", &self.agent_id)?;

        if let Some(activation) = &self.activation {
            activation.validate()?;
            if activation.intent.thread_id != self.thread_id {
                return Err(StorageError::Validation(format!(
                    "run activation thread_id '{}' must match run thread_id '{}'",
                    activation.intent.thread_id, self.thread_id
                )));
            }
        }

        if let Some(input) = &self.input {
            validate_run_message_input(&self.thread_id, input)?;
        }
        if let Some(output) = &self.output {
            validate_run_message_output(&self.thread_id, output)?;
        }

        match self.status {
            RunStatus::Created | RunStatus::Running => {
                if self.waiting.is_some() {
                    return Err(StorageError::Validation(format!(
                        "{:?} run '{}' must not carry waiting state",
                        self.status, self.run_id
                    )));
                }
                if self.outcome.is_some() {
                    return Err(StorageError::Validation(format!(
                        "{:?} run '{}' must not carry terminal outcome",
                        self.status, self.run_id
                    )));
                }
                if self.finished_at.is_some() {
                    return Err(StorageError::Validation(format!(
                        "{:?} run '{}' must not carry finished_at",
                        self.status, self.run_id
                    )));
                }
            }
            RunStatus::Waiting => {
                if self.waiting.is_none() {
                    return Err(StorageError::Validation(format!(
                        "waiting run '{}' must carry waiting state",
                        self.run_id
                    )));
                }
                if self.outcome.is_some() {
                    return Err(StorageError::Validation(format!(
                        "waiting run '{}' must not carry terminal outcome",
                        self.run_id
                    )));
                }
                if self.finished_at.is_some() {
                    return Err(StorageError::Validation(format!(
                        "waiting run '{}' must not carry finished_at",
                        self.run_id
                    )));
                }
            }
            RunStatus::Done => {
                if self.waiting.is_some() {
                    return Err(StorageError::Validation(format!(
                        "done run '{}' must not carry waiting state",
                        self.run_id
                    )));
                }
                if self.finished_at.is_none() {
                    return Err(StorageError::Validation(format!(
                        "done run '{}' must carry finished_at",
                        self.run_id
                    )));
                }
                if let Some(outcome) = &self.outcome {
                    if self
                        .termination_reason
                        .as_ref()
                        .is_some_and(|reason| reason != &outcome.termination_reason)
                    {
                        return Err(StorageError::Validation(format!(
                            "done run '{}' termination_reason must match outcome.termination_reason",
                            self.run_id
                        )));
                    }
                    if self
                        .final_output
                        .as_ref()
                        .is_some_and(|output| Some(output) != outcome.final_output.as_ref())
                    {
                        return Err(StorageError::Validation(format!(
                            "done run '{}' final_output must match outcome.final_output",
                            self.run_id
                        )));
                    }
                    if self
                        .error_payload
                        .as_ref()
                        .is_some_and(|payload| Some(payload) != outcome.error_payload.as_ref())
                    {
                        return Err(StorageError::Validation(format!(
                            "done run '{}' error_payload must match outcome.error_payload",
                            self.run_id
                        )));
                    }
                }
            }
        }

        Ok(())
    }

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

fn require_non_empty(field: &str, value: &str) -> Result<(), StorageError> {
    if value.trim().is_empty() {
        return Err(StorageError::Validation(format!(
            "{field} must not be empty"
        )));
    }
    Ok(())
}

fn validate_seq_range(field: &str, range: MessageSeqRange) -> Result<(), StorageError> {
    if range.from_seq == 0 || range.from_seq > range.to_seq {
        return Err(StorageError::Validation(format!(
            "{field} range must be non-empty and 1-based"
        )));
    }
    Ok(())
}

fn validate_run_message_input(
    run_thread_id: &str,
    input: &RunMessageInput,
) -> Result<(), StorageError> {
    if input.thread_id != run_thread_id {
        return Err(StorageError::Validation(format!(
            "run input thread_id '{}' must match run thread_id '{}'",
            input.thread_id, run_thread_id
        )));
    }
    if let Some(range) = input.range {
        validate_seq_range("run input", range)?;
    }
    Ok(())
}

fn validate_run_message_output(
    run_thread_id: &str,
    output: &RunMessageOutput,
) -> Result<(), StorageError> {
    if output.thread_id != run_thread_id {
        return Err(StorageError::Validation(format!(
            "run output thread_id '{}' must match run thread_id '{}'",
            output.thread_id, run_thread_id
        )));
    }
    if let Some(range) = output.range {
        validate_seq_range("run output", range)?;
        if range.len() as usize != output.message_ids.len() {
            return Err(StorageError::Validation(format!(
                "run output message_ids length {} must match range length {}",
                output.message_ids.len(),
                range.len()
            )));
        }
    }
    Ok(())
}

// ── consistent checkpoint snapshot ───────────────────────────────────

/// A single consistent read of a thread's resume state.
///
/// Pairs the committed message view with the latest run record, the
/// thread-scoped state, and the committed message version (record count) that
/// the next append must guard against. Reading these together — under one
/// transaction / lock per backend — avoids the torn reads a resume path would
/// otherwise risk by stitching separate `load_messages`/`latest_run`/
/// `load_thread_state` calls while a concurrent commit advances the thread.
///
/// This is the read-side counterpart to [`ThreadCommit`](crate::contract::commit_coordinator::ThreadCommit):
/// `commit_checkpoint(checkpoint)` writes, `load_checkpoint(thread)` reads back.
#[derive(Debug, Clone, Default)]
pub struct CheckpointSnapshot {
    /// Effective committed message view (read-time filters applied).
    pub messages: Vec<Message>,
    /// Committed message version — the count the next append guards against.
    pub message_version: u64,
    /// Latest run record for the thread, if any.
    pub latest_run: Option<RunRecord>,
    /// Thread-scoped persisted state, if any has been written.
    pub thread_state: Option<PersistedState>,
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

    /// Thread-scoped persisted state for `thread_id`, if any has been written.
    ///
    /// Default returns `None` (stores that do not persist thread-scoped state).
    /// Backends override this to return the last committed thread-scoped state,
    /// which the runtime merges (by `KeyScope`) with the resumed run's state.
    async fn load_thread_state(
        &self,
        thread_id: &str,
    ) -> Result<Option<crate::state::PersistedState>, StorageError> {
        let _ = thread_id;
        Ok(None)
    }

    /// Read a consistent [`CheckpointSnapshot`] for resume.
    ///
    /// The default composes the individual resume reads and applies the
    /// committed-history view filter. Backends that can read atomically
    /// (a transaction or lock spanning messages + run + thread state) override
    /// this to avoid torn reads against a concurrent commit; the default is
    /// adequate for single-process stores whose reads are already serialized.
    /// Returns `None` when the thread has no committed messages and no run.
    async fn load_checkpoint(
        &self,
        thread_id: &str,
    ) -> Result<Option<CheckpointSnapshot>, StorageError> {
        let committed = self.load_committed_messages(thread_id).await?;
        let latest_run = self.latest_run(thread_id).await?;
        if committed.is_none() && latest_run.is_none() {
            return Ok(None);
        }
        let raw = committed.unwrap_or_default();
        let message_version = raw.len() as u64;
        let messages = super::message::effective_committed_view(raw, thread_id);
        let thread_state = self.load_thread_state(thread_id).await?;
        Ok(Some(CheckpointSnapshot {
            messages,
            message_version,
            latest_run,
            thread_state,
        }))
    }
}

#[cfg(test)]
mod tests;
