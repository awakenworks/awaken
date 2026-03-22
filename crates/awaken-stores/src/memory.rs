//! In-memory storage backend for testing and local development.

use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{
    MailboxEntry, MailboxStore, RunPage, RunQuery, RunRecord, RunStore, StorageError,
    ThreadRunStore, ThreadStore,
};
use awaken_contract::thread::Thread;
use tokio::sync::RwLock;

/// In-memory storage implementing all four store traits.
///
/// Uses `tokio::sync::RwLock` for async-safe concurrent access.
/// Data lives only in memory and is lost when the store is dropped.
#[derive(Debug, Default)]
pub struct InMemoryStore {
    threads: RwLock<HashMap<String, Thread>>,
    runs: RwLock<HashMap<String, RunRecord>>,
    /// Thread ID -> ordered messages (single source of truth).
    messages: RwLock<HashMap<String, Vec<Message>>>,
    /// Mailbox ID -> ordered queue of entries.
    mailbox: RwLock<BTreeMap<String, Vec<MailboxEntry>>>,
}

impl InMemoryStore {
    /// Create a new empty in-memory store.
    pub fn new() -> Self {
        Self::default()
    }
}

// ── ThreadStore ─────────────────────────────────────────────────────

#[async_trait]
impl ThreadStore for InMemoryStore {
    async fn load_thread(&self, thread_id: &str) -> Result<Option<Thread>, StorageError> {
        let guard = self.threads.read().await;
        Ok(guard.get(thread_id).cloned())
    }

    async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError> {
        let mut guard = self.threads.write().await;
        guard.insert(thread.id.clone(), thread.clone());
        Ok(())
    }

    async fn list_threads(&self, offset: usize, limit: usize) -> Result<Vec<String>, StorageError> {
        let guard = self.threads.read().await;
        let mut ids: Vec<String> = guard.keys().cloned().collect();
        ids.sort();
        Ok(ids.into_iter().skip(offset).take(limit).collect())
    }

    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError> {
        let guard = self.messages.read().await;
        Ok(guard.get(thread_id).cloned())
    }

    async fn save_messages(
        &self,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<(), StorageError> {
        let mut guard = self.messages.write().await;
        guard.insert(thread_id.to_owned(), messages.to_vec());
        Ok(())
    }
}

// ── RunStore ────────────────────────────────────────────────────────

#[async_trait]
impl RunStore for InMemoryStore {
    async fn create_run(&self, record: &RunRecord) -> Result<(), StorageError> {
        let mut guard = self.runs.write().await;
        if guard.contains_key(&record.run_id) {
            return Err(StorageError::AlreadyExists(record.run_id.clone()));
        }
        guard.insert(record.run_id.clone(), record.clone());
        Ok(())
    }

    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError> {
        let guard = self.runs.read().await;
        Ok(guard.get(run_id).cloned())
    }

    async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError> {
        let guard = self.runs.read().await;
        Ok(guard
            .values()
            .filter(|r| r.thread_id == thread_id)
            .max_by_key(|r| r.updated_at)
            .cloned())
    }

    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, StorageError> {
        let guard = self.runs.read().await;
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

// ── MailboxStore ────────────────────────────────────────────────────

#[async_trait]
impl MailboxStore for InMemoryStore {
    async fn push_message(&self, entry: &MailboxEntry) -> Result<(), StorageError> {
        let mut guard = self.mailbox.write().await;
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
        let mut guard = self.mailbox.write().await;
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
        let guard = self.mailbox.read().await;
        Ok(guard
            .get(mailbox_id)
            .map(|q| q.iter().take(limit).cloned().collect())
            .unwrap_or_default())
    }
}

// ── ThreadRunStore ──────────────────────────────────────────────────

#[async_trait]
impl ThreadRunStore for InMemoryStore {
    async fn checkpoint(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), StorageError> {
        let mut msg_guard = self.messages.write().await;
        let mut run_guard = self.runs.write().await;
        msg_guard.insert(thread_id.to_owned(), messages.to_vec());
        run_guard.insert(run.run_id.clone(), run.clone());
        Ok(())
    }
}

// Unit tests removed: all scenarios are covered by integration tests
// in `tests/memory_store.rs`.
