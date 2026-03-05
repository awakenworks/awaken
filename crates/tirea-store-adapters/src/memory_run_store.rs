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
    use tirea_contract::storage::{RunOrigin, RunRecordStatus};

    #[tokio::test]
    async fn upsert_load_and_list_runs() {
        let store = MemoryRunStore::new();
        let r1 = RunRecord::new(
            "run-1",
            "thread-1",
            RunOrigin::AgUi,
            RunRecordStatus::Submitted,
            1,
        );
        let r2 = RunRecord::new(
            "run-2",
            "thread-2",
            RunOrigin::AiSdk,
            RunRecordStatus::Working,
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
}
