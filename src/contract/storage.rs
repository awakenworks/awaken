//! Storage traits for thread message and run record persistence.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::lifecycle::RunStatus;
use super::message::Message;
use crate::state::PersistedState;

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
    /// Current status of the run.
    pub status: RunStatus,
    /// Application-defined termination code.
    pub termination_code: Option<String>,
    /// Unix timestamp (seconds) when the run was created.
    pub created_at: u64,
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

/// Atomic thread+run checkpoint persistence.
///
/// Implementations must persist thread messages and run record in one transaction.
#[async_trait]
pub trait ThreadRunStore: Send + Sync {
    /// Load all messages for a thread. Returns `None` if the thread does not exist.
    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError>;

    /// Persist thread messages and run record atomically.
    async fn checkpoint(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), StorageError>;

    /// Load a run record by `run_id`.
    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError>;

    /// Find the latest run for a thread (by `updated_at`).
    async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError>;
}
