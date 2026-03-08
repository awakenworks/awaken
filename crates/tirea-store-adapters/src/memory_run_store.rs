use async_trait::async_trait;
use tirea_contract::storage::{
    paginate_runs_in_memory, RunPage, RunQuery, RunReader, RunRecord, RunStoreError, RunWriter,
};

/// In-memory run projection store for tests and local development.
#[derive(Default)]
pub struct MemoryRunStore {
    entries: tokio::sync::RwLock<std::collections::HashMap<String, RunRecord>>,
}

impl MemoryRunStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl RunReader for MemoryRunStore {
    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, RunStoreError> {
        Ok(self.entries.read().await.get(run_id).cloned())
    }

    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, RunStoreError> {
        let entries = self.entries.read().await;
        let records: Vec<RunRecord> = entries.values().cloned().collect();
        Ok(paginate_runs_in_memory(&records, query))
    }

    async fn load_current_run(&self, thread_id: &str) -> Result<Option<RunRecord>, RunStoreError> {
        let entries = self.entries.read().await;
        Ok(entries
            .values()
            .filter(|r| r.thread_id == thread_id && !r.status.is_terminal())
            .max_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| a.updated_at.cmp(&b.updated_at))
                    .then_with(|| a.run_id.cmp(&b.run_id))
            })
            .cloned())
    }
}

#[async_trait]
impl RunWriter for MemoryRunStore {
    async fn upsert_run(&self, record: &RunRecord) -> Result<(), RunStoreError> {
        self.entries
            .write()
            .await
            .insert(record.run_id.clone(), record.clone());
        Ok(())
    }

    async fn delete_run(&self, run_id: &str) -> Result<(), RunStoreError> {
        self.entries.write().await.remove(run_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tirea_contract::storage::{RunOrigin, RunStatus};

    #[tokio::test]
    async fn upsert_load_and_list_runs() {
        let store = MemoryRunStore::new();
        let r1 = RunRecord::new(
            "run-1",
            "thread-1",
            "",
            RunOrigin::AgUi,
            RunStatus::Running,
            1,
        );
        let r2 = RunRecord::new(
            "run-2",
            "thread-2",
            "",
            RunOrigin::AiSdk,
            RunStatus::Waiting,
            2,
        );
        store.upsert_run(&r1).await.expect("upsert run-1");
        store.upsert_run(&r2).await.expect("upsert run-2");

        let loaded = store
            .load_run("run-1")
            .await
            .expect("load")
            .expect("exists");
        assert_eq!(loaded.thread_id, "thread-1");

        let page = store
            .list_runs(&RunQuery {
                thread_id: Some("thread-2".to_string()),
                ..Default::default()
            })
            .await
            .expect("list");
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].run_id, "run-2");
    }

    #[tokio::test]
    async fn load_current_run_returns_latest_non_terminal() {
        let store = MemoryRunStore::new();

        // Older completed run.
        let mut done = RunRecord::new("run-old", "t1", "", RunOrigin::AgUi, RunStatus::Done, 1);
        done.updated_at = 2;
        store.upsert_run(&done).await.unwrap();

        // Newer active run.
        let mut active = RunRecord::new(
            "run-active",
            "t1",
            "",
            RunOrigin::AgUi,
            RunStatus::Running,
            3,
        );
        active.updated_at = 4;
        store.upsert_run(&active).await.unwrap();

        let current = store.load_current_run("t1").await.unwrap();
        assert_eq!(
            current.as_ref().map(|r| r.run_id.as_str()),
            Some("run-active")
        );
    }

    #[tokio::test]
    async fn load_current_run_returns_none_when_all_terminal() {
        let store = MemoryRunStore::new();
        let done = RunRecord::new("run-d", "t2", "", RunOrigin::AiSdk, RunStatus::Done, 1);
        store.upsert_run(&done).await.unwrap();

        assert!(store.load_current_run("t2").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn load_current_run_tiebreaks_by_created_at_then_run_id() {
        let store = MemoryRunStore::new();

        // Two active runs with same created_at — run_id tiebreaker.
        let r1 = RunRecord::new("run-a", "t3", "", RunOrigin::AgUi, RunStatus::Running, 10);
        let r2 = RunRecord::new("run-b", "t3", "", RunOrigin::AgUi, RunStatus::Waiting, 10);
        store.upsert_run(&r1).await.unwrap();
        store.upsert_run(&r2).await.unwrap();

        let current = store.load_current_run("t3").await.unwrap().unwrap();
        assert_eq!(
            current.run_id, "run-b",
            "should pick lexicographically later run_id as tiebreaker"
        );
    }
}
