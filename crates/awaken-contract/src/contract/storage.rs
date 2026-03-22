//! Storage traits for thread, run record, and mailbox persistence.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use super::lifecycle::RunStatus;
use super::message::Message;
use crate::state::PersistedState;
use crate::thread::Thread;

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
#[async_trait]
pub trait ThreadStore: Send + Sync {
    /// Load a thread by ID. Returns `None` if not found.
    async fn load_thread(&self, thread_id: &str) -> Result<Option<Thread>, StorageError>;

    /// Persist a thread (create or overwrite).
    async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError>;

    /// List thread IDs with pagination.
    async fn list_threads(&self, offset: usize, limit: usize) -> Result<Vec<String>, StorageError>;
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

// ── MailboxStore ────────────────────────────────────────────────────

/// Envelope for inter-thread message queue entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxEntry {
    /// Unique entry identifier.
    pub entry_id: String,
    /// Target mailbox address (typically a thread or agent ID).
    pub mailbox_id: String,
    /// Opaque message payload.
    pub payload: Value,
    /// Unix timestamp (millis) when the entry was enqueued.
    pub created_at: u64,
}

/// Inter-thread message queue persistence.
#[async_trait]
pub trait MailboxStore: Send + Sync {
    /// Push a message into a mailbox.
    async fn push_message(&self, entry: &MailboxEntry) -> Result<(), StorageError>;

    /// Pop up to `limit` messages from a mailbox (oldest first), removing them.
    async fn pop_messages(
        &self,
        mailbox_id: &str,
        limit: usize,
    ) -> Result<Vec<MailboxEntry>, StorageError>;

    /// Peek at up to `limit` messages in a mailbox without removing them.
    async fn peek_messages(
        &self,
        mailbox_id: &str,
        limit: usize,
    ) -> Result<Vec<MailboxEntry>, StorageError>;
}

// ── ThreadRunStore (convenience) ────────────────────────────────────

/// Atomic thread+run checkpoint persistence.
///
/// Implementations must persist thread messages and run record in one transaction.
/// This is a convenience trait extending ThreadStore + RunStore for atomic operations.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashMap};
    use std::sync::RwLock;

    // ── Mock ThreadStore ──

    #[derive(Debug, Default)]
    struct MockThreadStore {
        threads: RwLock<HashMap<String, Thread>>,
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
    }

    #[tokio::test]
    async fn thread_store_save_and_load() {
        let store = MockThreadStore::default();
        let thread = Thread::with_id("t-1").with_message(Message::user("hello"));

        store.save_thread(&thread).await.unwrap();
        let loaded = store.load_thread("t-1").await.unwrap().unwrap();
        assert_eq!(loaded.id, "t-1");
        assert_eq!(loaded.message_count(), 1);
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
            status: RunStatus::Running,
            termination_code: None,
            created_at: updated_at,
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

    // ── Mock MailboxStore ──

    #[derive(Debug, Default)]
    struct MockMailboxStore {
        entries: RwLock<BTreeMap<String, Vec<MailboxEntry>>>,
    }

    #[async_trait]
    impl MailboxStore for MockMailboxStore {
        async fn push_message(&self, entry: &MailboxEntry) -> Result<(), StorageError> {
            let mut guard = self
                .entries
                .write()
                .map_err(|e| StorageError::Io(e.to_string()))?;
            guard
                .entry(entry.mailbox_id.clone())
                .or_default()
                .push(entry.clone());
            Ok(())
        }

        async fn pop_messages(
            &self,
            mailbox_id: &str,
            limit: usize,
        ) -> Result<Vec<MailboxEntry>, StorageError> {
            let mut guard = self
                .entries
                .write()
                .map_err(|e| StorageError::Io(e.to_string()))?;
            let queue = match guard.get_mut(mailbox_id) {
                Some(q) => q,
                None => return Ok(Vec::new()),
            };
            let drain_count = limit.min(queue.len());
            Ok(queue.drain(..drain_count).collect())
        }

        async fn peek_messages(
            &self,
            mailbox_id: &str,
            limit: usize,
        ) -> Result<Vec<MailboxEntry>, StorageError> {
            let guard = self
                .entries
                .read()
                .map_err(|e| StorageError::Io(e.to_string()))?;
            Ok(guard
                .get(mailbox_id)
                .map(|q| q.iter().take(limit).cloned().collect())
                .unwrap_or_default())
        }
    }

    fn make_mailbox_entry(id: &str, mailbox: &str) -> MailboxEntry {
        MailboxEntry {
            entry_id: id.to_string(),
            mailbox_id: mailbox.to_string(),
            payload: serde_json::json!({"text": id}),
            created_at: 1000,
        }
    }

    #[tokio::test]
    async fn mailbox_push_and_peek() {
        let store = MockMailboxStore::default();
        store
            .push_message(&make_mailbox_entry("e1", "inbox-a"))
            .await
            .unwrap();
        store
            .push_message(&make_mailbox_entry("e2", "inbox-a"))
            .await
            .unwrap();

        let peeked = store.peek_messages("inbox-a", 10).await.unwrap();
        assert_eq!(peeked.len(), 2);
        // Peek does not remove
        let peeked_again = store.peek_messages("inbox-a", 10).await.unwrap();
        assert_eq!(peeked_again.len(), 2);
    }

    #[tokio::test]
    async fn mailbox_pop_removes_entries() {
        let store = MockMailboxStore::default();
        store
            .push_message(&make_mailbox_entry("e1", "inbox-a"))
            .await
            .unwrap();
        store
            .push_message(&make_mailbox_entry("e2", "inbox-a"))
            .await
            .unwrap();
        store
            .push_message(&make_mailbox_entry("e3", "inbox-a"))
            .await
            .unwrap();

        let popped = store.pop_messages("inbox-a", 2).await.unwrap();
        assert_eq!(popped.len(), 2);
        assert_eq!(popped[0].entry_id, "e1");
        assert_eq!(popped[1].entry_id, "e2");

        let remaining = store.peek_messages("inbox-a", 10).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].entry_id, "e3");
    }

    #[tokio::test]
    async fn mailbox_pop_empty() {
        let store = MockMailboxStore::default();
        let popped = store.pop_messages("nonexistent", 10).await.unwrap();
        assert!(popped.is_empty());
    }

    #[tokio::test]
    async fn mailbox_entry_serde_roundtrip() {
        let entry = make_mailbox_entry("e1", "inbox-a");
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: MailboxEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.entry_id, "e1");
        assert_eq!(parsed.mailbox_id, "inbox-a");
    }

    // ── RunRecord serde ──

    #[test]
    fn run_record_serde_roundtrip() {
        let run = make_run("r1", "t-1", 42);
        let json = serde_json::to_string(&run).unwrap();
        let parsed: RunRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.run_id, "r1");
        assert_eq!(parsed.thread_id, "t-1");
        assert_eq!(parsed.updated_at, 42);
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
