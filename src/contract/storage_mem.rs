//! In-memory transactional implementation of [`ThreadRunStore`] for testing.

use std::collections::HashMap;
use std::sync::RwLock;

use async_trait::async_trait;

use super::message::Message;
use super::storage::{RunRecord, StorageError, ThreadRunStore};

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

    #[tokio::test]
    async fn checkpoint_persists_thread_and_run_atomically() {
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

    #[tokio::test]
    async fn latest_run_by_thread() {
        let store = InMemoryThreadRunStore::new();
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

        let latest = store.latest_run("thread-1").await.unwrap().unwrap();
        assert_eq!(latest.run_id, "run-2");
        let latest2 = store.latest_run("thread-2").await.unwrap().unwrap();
        assert_eq!(latest2.run_id, "run-3");
    }
}
