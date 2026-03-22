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
    /// Thread ID -> ordered messages (for ThreadRunStore).
    thread_messages: RwLock<HashMap<String, Vec<Message>>>,
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
    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError> {
        let guard = self.thread_messages.read().await;
        Ok(guard.get(thread_id).cloned())
    }

    async fn checkpoint(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), StorageError> {
        let mut msg_guard = self.thread_messages.write().await;
        let mut run_guard = self.runs.write().await;
        msg_guard.insert(thread_id.to_owned(), messages.to_vec());
        run_guard.insert(run.run_id.clone(), run.clone());
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::lifecycle::RunStatus;

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

    fn make_mailbox_entry(id: &str, mailbox: &str) -> MailboxEntry {
        MailboxEntry {
            entry_id: id.to_string(),
            mailbox_id: mailbox.to_string(),
            payload: serde_json::json!({"text": id}),
            created_at: 1000,
        }
    }

    // ── ThreadStore ──

    #[tokio::test]
    async fn thread_store_save_and_load() {
        let store = InMemoryStore::new();
        let thread = Thread::with_id("t-1").with_message(Message::user("hello"));
        store.save_thread(&thread).await.unwrap();

        let loaded = store.load_thread("t-1").await.unwrap().unwrap();
        assert_eq!(loaded.id, "t-1");
        assert_eq!(loaded.message_count(), 1);
    }

    #[tokio::test]
    async fn thread_store_load_nonexistent() {
        let store = InMemoryStore::new();
        let result = store.load_thread("missing").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn thread_store_list_paginated() {
        let store = InMemoryStore::new();
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
    async fn thread_store_overwrite() {
        let store = InMemoryStore::new();
        let thread = Thread::with_id("t-1").with_message(Message::user("hello"));
        store.save_thread(&thread).await.unwrap();

        let updated = thread.with_message(Message::assistant("hi"));
        store.save_thread(&updated).await.unwrap();

        let loaded = store.load_thread("t-1").await.unwrap().unwrap();
        assert_eq!(loaded.message_count(), 2);
    }

    #[tokio::test]
    async fn thread_store_list_empty() {
        let store = InMemoryStore::new();
        let ids = store.list_threads(0, 10).await.unwrap();
        assert!(ids.is_empty());
    }

    #[tokio::test]
    async fn thread_store_list_sorted() {
        let store = InMemoryStore::new();
        store.save_thread(&Thread::with_id("c")).await.unwrap();
        store.save_thread(&Thread::with_id("a")).await.unwrap();
        store.save_thread(&Thread::with_id("b")).await.unwrap();

        let ids = store.list_threads(0, 10).await.unwrap();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    // ── RunStore ──

    #[tokio::test]
    async fn run_store_create_and_load() {
        let store = InMemoryStore::new();
        let run = make_run("run-1", "t-1", 100);
        store.create_run(&run).await.unwrap();

        let loaded = RunStore::load_run(&store, "run-1").await.unwrap().unwrap();
        assert_eq!(loaded.thread_id, "t-1");
    }

    #[tokio::test]
    async fn run_store_create_duplicate_errors() {
        let store = InMemoryStore::new();
        let run = make_run("run-1", "t-1", 100);
        store.create_run(&run).await.unwrap();
        let err = store.create_run(&run).await.unwrap_err();
        assert!(matches!(err, StorageError::AlreadyExists(_)));
    }

    #[tokio::test]
    async fn run_store_latest_run() {
        let store = InMemoryStore::new();
        store.create_run(&make_run("r1", "t-1", 100)).await.unwrap();
        store.create_run(&make_run("r2", "t-1", 200)).await.unwrap();
        store.create_run(&make_run("r3", "t-2", 300)).await.unwrap();

        let latest = RunStore::latest_run(&store, "t-1").await.unwrap().unwrap();
        assert_eq!(latest.run_id, "r2");
    }

    #[tokio::test]
    async fn run_store_list_with_filter() {
        let store = InMemoryStore::new();
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

    #[tokio::test]
    async fn run_store_list_with_status_filter() {
        let store = InMemoryStore::new();
        let mut done = make_run("r1", "t-1", 100);
        done.status = RunStatus::Done;
        store.create_run(&done).await.unwrap();
        store.create_run(&make_run("r2", "t-1", 200)).await.unwrap();

        let page = store
            .list_runs(&RunQuery {
                status: Some(RunStatus::Done),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].run_id, "r1");
    }

    #[tokio::test]
    async fn run_store_list_pagination() {
        let store = InMemoryStore::new();
        for i in 0..5 {
            store
                .create_run(&make_run(&format!("r{i}"), "t-1", i as u64 * 100))
                .await
                .unwrap();
        }
        let page = store
            .list_runs(&RunQuery {
                offset: 2,
                limit: 2,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.total, 5);
        assert_eq!(page.items.len(), 2);
        assert!(page.has_more);
    }

    #[tokio::test]
    async fn run_store_load_nonexistent() {
        let store = InMemoryStore::new();
        let result = RunStore::load_run(&store, "missing").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn run_store_latest_nonexistent_thread() {
        let store = InMemoryStore::new();
        let result = RunStore::latest_run(&store, "no-thread").await.unwrap();
        assert!(result.is_none());
    }

    // ── MailboxStore ──

    #[tokio::test]
    async fn mailbox_push_and_peek() {
        let store = InMemoryStore::new();
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
        let peeked_again = store.peek_messages("inbox-a", 10).await.unwrap();
        assert_eq!(peeked_again.len(), 2);
    }

    #[tokio::test]
    async fn mailbox_pop_removes_entries() {
        let store = InMemoryStore::new();
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
        let store = InMemoryStore::new();
        let popped = store.pop_messages("nonexistent", 10).await.unwrap();
        assert!(popped.is_empty());
    }

    #[tokio::test]
    async fn mailbox_peek_empty() {
        let store = InMemoryStore::new();
        let peeked = store.peek_messages("nonexistent", 10).await.unwrap();
        assert!(peeked.is_empty());
    }

    #[tokio::test]
    async fn mailbox_multiple_mailboxes() {
        let store = InMemoryStore::new();
        store
            .push_message(&make_mailbox_entry("e1", "inbox-a"))
            .await
            .unwrap();
        store
            .push_message(&make_mailbox_entry("e2", "inbox-b"))
            .await
            .unwrap();

        let a = store.peek_messages("inbox-a", 10).await.unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].entry_id, "e1");

        let b = store.peek_messages("inbox-b", 10).await.unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].entry_id, "e2");
    }

    #[tokio::test]
    async fn mailbox_pop_limited() {
        let store = InMemoryStore::new();
        for i in 0..5 {
            store
                .push_message(&make_mailbox_entry(&format!("e{i}"), "inbox"))
                .await
                .unwrap();
        }
        let popped = store.pop_messages("inbox", 3).await.unwrap();
        assert_eq!(popped.len(), 3);
        let remaining = store.peek_messages("inbox", 10).await.unwrap();
        assert_eq!(remaining.len(), 2);
    }

    #[tokio::test]
    async fn mailbox_entry_serde_roundtrip() {
        let entry = make_mailbox_entry("e1", "inbox-a");
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: MailboxEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.entry_id, "e1");
        assert_eq!(parsed.mailbox_id, "inbox-a");
    }

    // ── ThreadRunStore ──

    #[tokio::test]
    async fn checkpoint_persists_thread_and_run() {
        let store = InMemoryStore::new();
        let run = make_run("run-x", "thread-x", 42);
        let messages = vec![Message::user("u1"), Message::assistant("a1")];

        store.checkpoint("thread-x", &messages, &run).await.unwrap();

        let loaded_messages = store.load_messages("thread-x").await.unwrap().unwrap();
        assert_eq!(loaded_messages.len(), 2);
        assert_eq!(loaded_messages[0].text(), "u1");

        let loaded_run = ThreadRunStore::load_run(&store, "run-x")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded_run.thread_id, "thread-x");
        assert_eq!(loaded_run.updated_at, 42);
    }

    #[tokio::test]
    async fn checkpoint_overwrites_previous_messages() {
        let store = InMemoryStore::new();
        let run1 = make_run("run-1", "t-1", 100);
        store
            .checkpoint("t-1", &[Message::user("old")], &run1)
            .await
            .unwrap();

        let run2 = make_run("run-2", "t-1", 200);
        store
            .checkpoint("t-1", &[Message::user("new")], &run2)
            .await
            .unwrap();

        let msgs = store.load_messages("t-1").await.unwrap().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text(), "new");
    }

    #[tokio::test]
    async fn latest_run_by_thread() {
        let store = InMemoryStore::new();
        let msgs = vec![Message::user("m")];
        store
            .checkpoint("thread-1", &msgs, &make_run("run-1", "thread-1", 100))
            .await
            .unwrap();
        store
            .checkpoint("thread-1", &msgs, &make_run("run-2", "thread-1", 200))
            .await
            .unwrap();
        store
            .checkpoint("thread-2", &msgs, &make_run("run-3", "thread-2", 300))
            .await
            .unwrap();

        let latest = ThreadRunStore::latest_run(&store, "thread-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(latest.run_id, "run-2");
        let latest2 = ThreadRunStore::latest_run(&store, "thread-2")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(latest2.run_id, "run-3");
    }

    #[tokio::test]
    async fn load_messages_nonexistent_thread() {
        let store = InMemoryStore::new();
        let result = store.load_messages("missing").await.unwrap();
        assert!(result.is_none());
    }

    // ── Concurrent access ──

    #[tokio::test]
    async fn concurrent_thread_access() {
        use std::sync::Arc;

        let store = Arc::new(InMemoryStore::new());
        let handles: Vec<_> = (0..10)
            .map(|i| {
                let store = Arc::clone(&store);
                tokio::spawn(async move {
                    let thread = Thread::with_id(format!("thread-{i}"));
                    store.save_thread(&thread).await.unwrap();
                })
            })
            .collect();

        for handle in handles {
            handle.await.unwrap();
        }

        let ids = store.list_threads(0, 100).await.unwrap();
        assert_eq!(ids.len(), 10);
    }

    #[tokio::test]
    async fn concurrent_run_access() {
        use std::sync::Arc;

        let store = Arc::new(InMemoryStore::new());
        let handles: Vec<_> = (0..10)
            .map(|i| {
                let store = Arc::clone(&store);
                tokio::spawn(async move {
                    let run = make_run(&format!("run-{i}"), "t-1", i as u64 * 100);
                    store.create_run(&run).await.unwrap();
                })
            })
            .collect();

        for handle in handles {
            handle.await.unwrap();
        }

        let page = store.list_runs(&RunQuery::default()).await.unwrap();
        assert_eq!(page.total, 10);
    }

    #[tokio::test]
    async fn concurrent_mailbox_push_and_pop() {
        use std::sync::Arc;

        let store = Arc::new(InMemoryStore::new());

        // Push 20 entries concurrently
        let push_handles: Vec<_> = (0..20)
            .map(|i| {
                let store = Arc::clone(&store);
                tokio::spawn(async move {
                    store
                        .push_message(&make_mailbox_entry(&format!("e{i}"), "inbox"))
                        .await
                        .unwrap();
                })
            })
            .collect();

        for handle in push_handles {
            handle.await.unwrap();
        }

        let peeked = store.peek_messages("inbox", 100).await.unwrap();
        assert_eq!(peeked.len(), 20);

        // Pop all
        let popped = store.pop_messages("inbox", 100).await.unwrap();
        assert_eq!(popped.len(), 20);
        let remaining = store.peek_messages("inbox", 100).await.unwrap();
        assert!(remaining.is_empty());
    }
}
