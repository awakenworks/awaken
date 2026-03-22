//! In-memory implementations of [`ThreadStore`] and [`RunStore`] for testing.

use std::collections::HashMap;
use std::sync::RwLock;

use async_trait::async_trait;

use super::message::Message;
use super::storage::{RunRecord, RunStore, StorageError, ThreadRunStore, ThreadStore};

/// In-memory thread store backed by a `HashMap` behind a `RwLock`.
#[derive(Debug, Default)]
pub struct InMemoryThreadStore {
    threads: RwLock<HashMap<String, Vec<Message>>>,
}

impl InMemoryThreadStore {
    /// Create a new empty thread store.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ThreadStore for InMemoryThreadStore {
    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError> {
        let guard = self
            .threads
            .read()
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(guard.get(thread_id).cloned())
    }

    async fn append_messages(
        &self,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<(), StorageError> {
        let mut guard = self
            .threads
            .write()
            .map_err(|e| StorageError::Io(e.to_string()))?;
        guard
            .entry(thread_id.to_owned())
            .or_default()
            .extend(messages.iter().cloned());
        Ok(())
    }

    async fn replace_messages(
        &self,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<(), StorageError> {
        let mut guard = self
            .threads
            .write()
            .map_err(|e| StorageError::Io(e.to_string()))?;
        guard.insert(thread_id.to_owned(), messages.to_vec());
        Ok(())
    }

    async fn delete_thread(&self, thread_id: &str) -> Result<(), StorageError> {
        let mut guard = self
            .threads
            .write()
            .map_err(|e| StorageError::Io(e.to_string()))?;
        guard.remove(thread_id);
        Ok(())
    }
}

/// In-memory run store backed by a `HashMap` behind a `RwLock`.
#[derive(Debug, Default)]
pub struct InMemoryRunStore {
    runs: RwLock<HashMap<String, RunRecord>>,
}

#[derive(Debug, Default)]
struct ThreadRunData {
    threads: HashMap<String, Vec<Message>>,
    runs: HashMap<String, RunRecord>,
}

/// In-memory transactional thread+run store.
#[derive(Debug, Default)]
pub struct InMemoryThreadRunStore {
    inner: RwLock<ThreadRunData>,
}

impl InMemoryThreadRunStore {
    /// Create a new empty transactional store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl InMemoryRunStore {
    /// Create a new empty run store.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl RunStore for InMemoryRunStore {
    async fn save_run(&self, record: &RunRecord) -> Result<(), StorageError> {
        let mut guard = self
            .runs
            .write()
            .map_err(|e| StorageError::Io(e.to_string()))?;
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
        let latest = guard
            .values()
            .filter(|r| r.thread_id == thread_id)
            .max_by_key(|r| r.updated_at)
            .cloned();
        Ok(latest)
    }
}

#[async_trait]
impl ThreadRunStore for InMemoryThreadRunStore {
    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError> {
        let guard = self
            .inner
            .read()
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(guard.threads.get(thread_id).cloned())
    }

    async fn checkpoint(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), StorageError> {
        let mut guard = self
            .inner
            .write()
            .map_err(|e| StorageError::Io(e.to_string()))?;
        guard
            .threads
            .insert(thread_id.to_owned(), messages.to_vec());
        guard.runs.insert(run.run_id.clone(), run.clone());
        Ok(())
    }

    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError> {
        let guard = self
            .inner
            .read()
            .map_err(|e| StorageError::Io(e.to_string()))?;
        Ok(guard.runs.get(run_id).cloned())
    }

    async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError> {
        let guard = self
            .inner
            .read()
            .map_err(|e| StorageError::Io(e.to_string()))?;
        let latest = guard
            .runs
            .values()
            .filter(|r| r.thread_id == thread_id)
            .max_by_key(|r| r.updated_at)
            .cloned();
        Ok(latest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::lifecycle::RunStatus;

    fn make_run(run_id: &str, thread_id: &str, updated_at: u64) -> RunRecord {
        RunRecord {
            run_id: run_id.to_owned(),
            thread_id: thread_id.to_owned(),
            agent_id: "agent-1".to_owned(),
            parent_run_id: None,
            status: RunStatus::Running,
            termination_code: None,
            created_at: 1000,
            updated_at,
            steps: 0,
            input_tokens: 0,
            output_tokens: 0,
            state: None,
        }
    }

    // ── ThreadStore tests ──────────────────────────────────────────

    #[tokio::test]
    async fn thread_create_append_load_delete() {
        let store = InMemoryThreadStore::new();
        let tid = "thread-1";

        // Initially empty.
        assert!(store.load_messages(tid).await.unwrap().is_none());

        // Append first batch.
        let batch1 = vec![Message::user("hello"), Message::assistant("hi")];
        store.append_messages(tid, &batch1).await.unwrap();

        let loaded = store.load_messages(tid).await.unwrap().unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].text(), "hello");
        assert_eq!(loaded[1].text(), "hi");

        // Append second batch.
        let batch2 = vec![Message::user("more"), Message::tool("c1", "result")];
        store.append_messages(tid, &batch2).await.unwrap();

        let loaded = store.load_messages(tid).await.unwrap().unwrap();
        assert_eq!(loaded.len(), 4);
        assert_eq!(loaded[2].text(), "more");
        assert_eq!(loaded[3].text(), "result");

        // Delete.
        store.delete_thread(tid).await.unwrap();
        assert!(store.load_messages(tid).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn thread_delete_nonexistent_is_ok() {
        let store = InMemoryThreadStore::new();
        // Deleting a thread that was never created should succeed silently.
        store.delete_thread("no-such-thread").await.unwrap();
    }

    #[tokio::test]
    async fn thread_multiple_threads_independent() {
        let store = InMemoryThreadStore::new();

        store
            .append_messages("t1", &[Message::user("a")])
            .await
            .unwrap();
        store
            .append_messages("t2", &[Message::user("b"), Message::user("c")])
            .await
            .unwrap();

        let t1 = store.load_messages("t1").await.unwrap().unwrap();
        let t2 = store.load_messages("t2").await.unwrap().unwrap();

        assert_eq!(t1.len(), 1);
        assert_eq!(t1[0].text(), "a");
        assert_eq!(t2.len(), 2);
        assert_eq!(t2[0].text(), "b");

        // Deleting one does not affect the other.
        store.delete_thread("t1").await.unwrap();
        assert!(store.load_messages("t1").await.unwrap().is_none());
        assert!(store.load_messages("t2").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn thread_append_empty_slice() {
        let store = InMemoryThreadStore::new();
        store.append_messages("t1", &[]).await.unwrap();
        // Thread is created but with zero messages.
        let loaded = store.load_messages("t1").await.unwrap().unwrap();
        assert!(loaded.is_empty());
    }

    // ── RunStore tests ─────────────────────────────────────────────

    #[tokio::test]
    async fn run_save_load_update() {
        let store = InMemoryRunStore::new();

        let mut record = make_run("run-1", "thread-1", 100);
        store.save_run(&record).await.unwrap();

        let loaded = store.load_run("run-1").await.unwrap().unwrap();
        assert_eq!(loaded.run_id, "run-1");
        assert_eq!(loaded.status, RunStatus::Running);

        // Update the record.
        record.status = RunStatus::Done;
        record.steps = 5;
        record.updated_at = 200;
        store.save_run(&record).await.unwrap();

        let loaded = store.load_run("run-1").await.unwrap().unwrap();
        assert_eq!(loaded.status, RunStatus::Done);
        assert_eq!(loaded.steps, 5);
        assert_eq!(loaded.updated_at, 200);
    }

    #[tokio::test]
    async fn run_load_nonexistent() {
        let store = InMemoryRunStore::new();
        assert!(store.load_run("no-such-run").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn run_latest_run_by_thread() {
        let store = InMemoryRunStore::new();

        // No runs yet.
        assert!(store.latest_run("thread-1").await.unwrap().is_none());

        // Insert multiple runs for the same thread with different timestamps.
        store
            .save_run(&make_run("run-1", "thread-1", 100))
            .await
            .unwrap();
        store
            .save_run(&make_run("run-2", "thread-1", 300))
            .await
            .unwrap();
        store
            .save_run(&make_run("run-3", "thread-1", 200))
            .await
            .unwrap();

        let latest = store.latest_run("thread-1").await.unwrap().unwrap();
        assert_eq!(latest.run_id, "run-2");
        assert_eq!(latest.updated_at, 300);
    }

    #[tokio::test]
    async fn run_latest_run_filters_by_thread() {
        let store = InMemoryRunStore::new();

        store
            .save_run(&make_run("run-a", "thread-1", 500))
            .await
            .unwrap();
        store
            .save_run(&make_run("run-b", "thread-2", 600))
            .await
            .unwrap();

        let latest = store.latest_run("thread-1").await.unwrap().unwrap();
        assert_eq!(latest.run_id, "run-a");

        let latest = store.latest_run("thread-2").await.unwrap().unwrap();
        assert_eq!(latest.run_id, "run-b");

        assert!(store.latest_run("thread-3").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn run_record_serialization_roundtrip() {
        let record = make_run("run-1", "thread-1", 100);
        let json = serde_json::to_string(&record).unwrap();
        let parsed: RunRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.run_id, "run-1");
        assert_eq!(parsed.thread_id, "thread-1");
    }

    #[tokio::test]
    async fn thread_run_checkpoint_persists_both_atomically() {
        let store = InMemoryThreadRunStore::new();
        let run = make_run("run-x", "thread-x", 42);
        let messages = vec![Message::user("u1"), Message::assistant("a1")];

        store
            .checkpoint("thread-x", &messages, &run)
            .await
            .expect("checkpoint should succeed");

        let loaded_messages = store
            .load_messages("thread-x")
            .await
            .expect("load messages")
            .expect("thread exists");
        assert_eq!(loaded_messages.len(), 2);
        assert_eq!(loaded_messages[0].text(), "u1");

        let loaded_run = store
            .load_run("run-x")
            .await
            .expect("load run")
            .expect("run exists");
        assert_eq!(loaded_run.thread_id, "thread-x");
        assert_eq!(loaded_run.updated_at, 42);
    }
}
