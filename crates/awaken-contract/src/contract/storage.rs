//! Storage traits for thread, run record, and persistence.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::lifecycle::{RunStatus, TerminationReason};
use super::message::{Message, MessageRecord};
use super::suspension::{ToolCallResume, ToolCallResumeMode};
use super::tool::ToolDescriptor;
use crate::state::PersistedState;
use crate::thread::Thread;
use serde_json::Value;

// ── errors ──────────────────────────────────────────────────────────

/// Errors returned by storage operations.
#[derive(Debug, Error)]
pub enum StorageError {
    /// The requested entity was not found.
    #[error("not found: {0}")]
    NotFound(String),
    /// An entity with the given key already exists.
    #[error("already exists: {0}")]
    AlreadyExists(String),
    /// Optimistic concurrency conflict.
    #[error("version conflict: expected {expected}, actual {actual}")]
    VersionConflict {
        /// The version the caller expected.
        expected: u64,
        /// The actual current version.
        actual: u64,
    },
    /// An I/O error occurred.
    #[error("io error: {0}")]
    Io(String),
    /// A serialization or deserialization error occurred.
    #[error("serialization error: {0}")]
    Serialization(String),
}

// ── run record ──────────────────────────────────────────────────────

/// Origin of a run request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunRequestOrigin {
    /// HTTP API, SDK.
    #[default]
    User,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    /// Unique run identifier.
    pub run_id: String,
    /// The thread this run belongs to.
    pub thread_id: String,
    /// The agent that executed this run.
    pub agent_id: String,
    /// Parent run identifier for nested/handoff runs.
    pub parent_run_id: Option<String>,
    /// Snapshot of the user intent/request that owns this run.
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
#[derive(Debug, Clone)]
pub struct MessageQuery {
    /// Number of items to skip.
    pub offset: usize,
    /// Maximum number of items to return.
    pub limit: usize,
}

impl Default for MessageQuery {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: 50,
        }
    }
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
    async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError>;

    /// Delete a thread and its associated messages.
    async fn delete_thread(&self, thread_id: &str) -> Result<(), StorageError>;

    /// List thread IDs with pagination.
    async fn list_threads(&self, offset: usize, limit: usize) -> Result<Vec<String>, StorageError>;

    /// Load all messages for a thread. Returns `None` if no messages exist.
    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError>;

    /// Load thread-owned message records with stable 1-based sequence numbers.
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

    /// Append messages to a thread's durable log and return their records.
    async fn append_message_records(
        &self,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<Vec<MessageRecord>, StorageError> {
        let mut existing = self.load_messages(thread_id).await?.unwrap_or_default();
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

/// Atomic thread+run checkpoint persistence.
///
/// Extends [`ThreadStore`] + [`RunStore`] with a transactional checkpoint
/// that persists thread messages and run record together. Read methods
/// (`load_messages`, `load_run`, `latest_run`) are inherited from the
/// supertraits — implementations should not duplicate them.
#[async_trait]
pub trait ThreadRunStore: ThreadStore + RunStore + Send + Sync {
    /// Persist thread messages and run record atomically.
    async fn checkpoint(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), StorageError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::RwLock;

    // ── Mock ThreadStore ──

    #[derive(Debug, Default)]
    struct MockThreadStore {
        threads: RwLock<HashMap<String, Thread>>,
        messages: RwLock<HashMap<String, Vec<Message>>>,
    }

    #[async_trait]
    impl ThreadStore for MockThreadStore {
        async fn load_thread(&self, thread_id: &str) -> Result<Option<Thread>, StorageError> {
            let guard = self
                .threads
                .read()
                .map_err(|e| StorageError::Io(e.to_string()))?;
            Ok(guard.get(thread_id).cloned())
        }

        async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError> {
            let mut guard = self
                .threads
                .write()
                .map_err(|e| StorageError::Io(e.to_string()))?;
            guard.insert(thread.id.clone(), thread.clone());
            Ok(())
        }

        async fn delete_thread(&self, thread_id: &str) -> Result<(), StorageError> {
            let mut threads = self
                .threads
                .write()
                .map_err(|e| StorageError::Io(e.to_string()))?;
            let mut messages = self
                .messages
                .write()
                .map_err(|e| StorageError::Io(e.to_string()))?;
            threads.remove(thread_id);
            messages.remove(thread_id);
            Ok(())
        }

        async fn list_threads(
            &self,
            offset: usize,
            limit: usize,
        ) -> Result<Vec<String>, StorageError> {
            let guard = self
                .threads
                .read()
                .map_err(|e| StorageError::Io(e.to_string()))?;
            let mut ids: Vec<String> = guard.keys().cloned().collect();
            ids.sort();
            Ok(ids.into_iter().skip(offset).take(limit).collect())
        }

        async fn load_messages(
            &self,
            thread_id: &str,
        ) -> Result<Option<Vec<Message>>, StorageError> {
            let guard = self
                .messages
                .read()
                .map_err(|e| StorageError::Io(e.to_string()))?;
            Ok(guard.get(thread_id).cloned())
        }

        async fn save_messages(
            &self,
            thread_id: &str,
            messages: &[Message],
        ) -> Result<(), StorageError> {
            let mut guard = self
                .messages
                .write()
                .map_err(|e| StorageError::Io(e.to_string()))?;
            guard.insert(thread_id.to_owned(), messages.to_vec());
            Ok(())
        }

        async fn delete_messages(&self, thread_id: &str) -> Result<(), StorageError> {
            let threads = self
                .threads
                .read()
                .map_err(|e| StorageError::Io(e.to_string()))?;
            if !threads.contains_key(thread_id) {
                return Err(StorageError::NotFound(thread_id.to_owned()));
            }
            drop(threads);
            let mut guard = self
                .messages
                .write()
                .map_err(|e| StorageError::Io(e.to_string()))?;
            guard.remove(thread_id);
            Ok(())
        }

        async fn update_thread_metadata(
            &self,
            id: &str,
            metadata: crate::thread::ThreadMetadata,
        ) -> Result<(), StorageError> {
            let mut guard = self
                .threads
                .write()
                .map_err(|e| StorageError::Io(e.to_string()))?;
            let thread = guard
                .get_mut(id)
                .ok_or_else(|| StorageError::NotFound(id.to_owned()))?;
            thread.metadata = metadata;
            Ok(())
        }
    }

    #[tokio::test]
    async fn thread_store_save_and_load() {
        let store = MockThreadStore::default();
        let thread = Thread::with_id("t-1");

        store.save_thread(&thread).await.unwrap();
        let loaded = store.load_thread("t-1").await.unwrap().unwrap();
        assert_eq!(loaded.id, "t-1");
    }

    #[tokio::test]
    async fn thread_store_load_nonexistent() {
        let store = MockThreadStore::default();
        let result = store.load_thread("missing").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn thread_store_list_paginated() {
        let store = MockThreadStore::default();
        for i in 0..5 {
            let thread = Thread::with_id(format!("t-{i}"));
            store.save_thread(&thread).await.unwrap();
        }
        let page1 = store.list_threads(0, 3).await.unwrap();
        assert_eq!(page1.len(), 3);
        let page2 = store.list_threads(3, 3).await.unwrap();
        assert_eq!(page2.len(), 2);
    }

    #[tokio::test]
    async fn thread_store_save_and_load_messages() {
        let store = MockThreadStore::default();
        let msgs = vec![
            Message::user("hello"),
            Message::assistant("hi").with_metadata(crate::contract::message::MessageMetadata {
                run_id: Some("run-1".to_string()),
                step_index: Some(0),
            }),
        ];
        store.save_messages("t-1", &msgs).await.unwrap();

        let loaded = store.load_messages("t-1").await.unwrap().unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].text(), "hello");
        let records = store.load_message_records("t-1").await.unwrap().unwrap();
        assert_eq!(records[0].thread_id, "t-1");
        assert_eq!(records[0].seq, 1);
        assert_eq!(records[1].seq, 2);
        assert_eq!(records[1].produced_by_run_id.as_deref(), Some("run-1"));
    }

    #[tokio::test]
    async fn thread_store_load_messages_nonexistent() {
        let store = MockThreadStore::default();
        let result = store.load_messages("missing").await.unwrap();
        assert!(result.is_none());
    }

    // ── Mock RunStore ──

    #[derive(Debug, Default)]
    struct MockRunStore {
        runs: RwLock<HashMap<String, RunRecord>>,
    }

    #[async_trait]
    impl RunStore for MockRunStore {
        async fn create_run(&self, record: &RunRecord) -> Result<(), StorageError> {
            let mut guard = self
                .runs
                .write()
                .map_err(|e| StorageError::Io(e.to_string()))?;
            if guard.contains_key(&record.run_id) {
                return Err(StorageError::AlreadyExists(record.run_id.clone()));
            }
            guard.insert(record.run_id.clone(), record.clone());
            Ok(())
        }

        async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError> {
            let guard = self
                .runs
                .read()
                .map_err(|e| StorageError::Io(e.to_string()))?;
            Ok(guard.get(run_id).cloned())
        }

        async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError> {
            let guard = self
                .runs
                .read()
                .map_err(|e| StorageError::Io(e.to_string()))?;
            Ok(guard
                .values()
                .filter(|r| r.thread_id == thread_id)
                .max_by_key(|r| r.updated_at)
                .cloned())
        }

        async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, StorageError> {
            let guard = self
                .runs
                .read()
                .map_err(|e| StorageError::Io(e.to_string()))?;
            let mut filtered: Vec<RunRecord> = guard
                .values()
                .filter(|r| query.thread_id.as_deref().is_none_or(|t| r.thread_id == t))
                .filter(|r| query.status.is_none_or(|s| r.status == s))
                .cloned()
                .collect();
            filtered.sort_by_key(|r| r.created_at);
            let total = filtered.len();
            let offset = query.offset.min(total);
            let limit = query.limit.clamp(1, 200);
            let items: Vec<RunRecord> = filtered.into_iter().skip(offset).take(limit).collect();
            let has_more = offset + items.len() < total;
            Ok(RunPage {
                items,
                total,
                has_more,
            })
        }
    }

    fn make_run(run_id: &str, thread_id: &str, updated_at: u64) -> RunRecord {
        RunRecord {
            run_id: run_id.to_owned(),
            thread_id: thread_id.to_owned(),
            agent_id: "agent-1".to_owned(),
            parent_run_id: None,
            request: None,
            input: None,
            output: None,
            status: RunStatus::Running,
            termination_reason: None,
            final_output: None,
            error_payload: None,
            dispatch_id: None,
            session_id: None,
            transport_request_id: None,
            waiting: None,
            outcome: None,
            created_at: updated_at,
            started_at: None,
            finished_at: None,
            updated_at,
            steps: 0,
            input_tokens: 0,
            output_tokens: 0,
            state: None,
        }
    }

    #[tokio::test]
    async fn run_store_create_and_load() {
        let store = MockRunStore::default();
        let run = make_run("run-1", "t-1", 100);
        store.create_run(&run).await.unwrap();

        let loaded = store.load_run("run-1").await.unwrap().unwrap();
        assert_eq!(loaded.thread_id, "t-1");
    }

    #[tokio::test]
    async fn run_store_create_duplicate_errors() {
        let store = MockRunStore::default();
        let run = make_run("run-1", "t-1", 100);
        store.create_run(&run).await.unwrap();
        let err = store.create_run(&run).await.unwrap_err();
        assert!(matches!(err, StorageError::AlreadyExists(_)));
    }

    #[tokio::test]
    async fn run_store_latest_run() {
        let store = MockRunStore::default();
        store.create_run(&make_run("r1", "t-1", 100)).await.unwrap();
        store.create_run(&make_run("r2", "t-1", 200)).await.unwrap();
        store.create_run(&make_run("r3", "t-2", 300)).await.unwrap();

        let latest = store.latest_run("t-1").await.unwrap().unwrap();
        assert_eq!(latest.run_id, "r2");
    }

    #[tokio::test]
    async fn run_store_list_with_filter() {
        let store = MockRunStore::default();
        store.create_run(&make_run("r1", "t-1", 100)).await.unwrap();
        store.create_run(&make_run("r2", "t-1", 200)).await.unwrap();
        store.create_run(&make_run("r3", "t-2", 300)).await.unwrap();

        let page = store
            .list_runs(&RunQuery {
                thread_id: Some("t-1".to_string()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(page.items.len(), 2);
    }

    // ── RunRecord serde ──

    #[test]
    fn run_record_serde_roundtrip() {
        let mut run = make_run("r1", "t-1", 42);
        run.input = Some(RunMessageInput {
            thread_id: "t-1".to_string(),
            range: MessageSeqRange::new(1, 2),
            trigger_message_ids: vec!["m-1".to_string()],
            selected_message_ids: Vec::new(),
            context_policy: None,
            compacted_snapshot_id: None,
        });
        run.output = Some(RunMessageOutput {
            thread_id: "t-1".to_string(),
            range: MessageSeqRange::new(3, 4),
            message_ids: vec!["m-3".to_string(), "m-4".to_string()],
        });
        let json = serde_json::to_string(&run).unwrap();
        let parsed: RunRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.run_id, "r1");
        assert_eq!(parsed.thread_id, "t-1");
        assert_eq!(parsed.updated_at, 42);
        assert_eq!(parsed.input.unwrap().range.unwrap().len(), 2);
        assert_eq!(
            parsed.output.unwrap().message_ids,
            vec!["m-3".to_string(), "m-4".to_string()]
        );
    }

    #[test]
    fn message_seq_range_rejects_empty_or_zero_based_ranges() {
        assert!(MessageSeqRange::new(0, 1).is_none());
        assert!(MessageSeqRange::new(2, 1).is_none());
        let range = MessageSeqRange::new(2, 4).unwrap();
        assert_eq!(range.len(), 3);
        assert!(!range.is_empty());
    }

    #[test]
    fn run_record_waiting_reason_prefers_structured_state() {
        let mut run = make_run("r1", "t-1", 42);
        run.status = RunStatus::Waiting;
        run.waiting = Some(RunWaitingState {
            reason: WaitingReason::ToolPermission,
            ticket_ids: vec!["ticket-1".to_string()],
            tickets: Vec::new(),
            since_dispatch_id: None,
            message: None,
        });

        assert_eq!(run.waiting_reason(), Some(WaitingReason::ToolPermission));
        assert!(run.is_resumable_waiting());
        assert!(!run.is_background_task_waiting());
    }

    #[test]
    fn run_record_waiting_reason_uses_structured_state() {
        let mut run = make_run("r1", "t-1", 42);
        run.status = RunStatus::Waiting;
        run.waiting = Some(RunWaitingState {
            reason: WaitingReason::BackgroundTasks,
            ticket_ids: Vec::new(),
            tickets: Vec::new(),
            since_dispatch_id: None,
            message: None,
        });
        assert_eq!(run.waiting_reason(), Some(WaitingReason::BackgroundTasks));
        assert!(run.is_background_task_waiting());

        run.waiting.as_mut().unwrap().reason = WaitingReason::ToolPermission;
        assert_eq!(run.waiting_reason(), Some(WaitingReason::ToolPermission));

        run.waiting.as_mut().unwrap().reason = WaitingReason::UserInput;
        assert_eq!(run.waiting_reason(), Some(WaitingReason::UserInput));
    }

    #[test]
    fn run_record_done_ignores_waiting_state() {
        let mut run = make_run("r1", "t-1", 42);
        run.status = RunStatus::Done;
        run.waiting = Some(RunWaitingState {
            reason: WaitingReason::BackgroundTasks,
            ticket_ids: Vec::new(),
            tickets: Vec::new(),
            since_dispatch_id: None,
            message: None,
        });

        assert_eq!(run.waiting_reason(), None);
        assert!(!run.is_resumable_waiting());
        assert!(!run.is_background_task_waiting());
    }

    #[test]
    fn run_request_origin_serde_roundtrip() {
        for origin in [
            RunRequestOrigin::User,
            RunRequestOrigin::A2A,
            RunRequestOrigin::Internal,
        ] {
            let json = serde_json::to_string(&origin).unwrap();
            let parsed: RunRequestOrigin = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, origin);
        }
    }

    // ── Query types ──

    #[test]
    fn message_query_default() {
        let q = MessageQuery::default();
        assert_eq!(q.offset, 0);
        assert_eq!(q.limit, 50);
    }

    #[test]
    fn run_query_default() {
        let q = RunQuery::default();
        assert_eq!(q.offset, 0);
        assert_eq!(q.limit, 50);
        assert!(q.thread_id.is_none());
        assert!(q.status.is_none());
    }

    #[test]
    fn run_page_serde_roundtrip() {
        let page = RunPage {
            items: vec![make_run("r1", "t-1", 100)],
            total: 1,
            has_more: false,
        };
        let json = serde_json::to_string(&page).unwrap();
        let parsed: RunPage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.total, 1);
        assert!(!parsed.has_more);
    }

    #[test]
    fn storage_error_display() {
        assert_eq!(
            StorageError::NotFound("x".into()).to_string(),
            "not found: x"
        );
        assert_eq!(
            StorageError::AlreadyExists("x".into()).to_string(),
            "already exists: x"
        );
        assert_eq!(
            StorageError::VersionConflict {
                expected: 1,
                actual: 2,
            }
            .to_string(),
            "version conflict: expected 1, actual 2"
        );
    }
}
