use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::contract::message::Message;
use awaken_runtime_contract::contract::storage::{
    RunRecord, RunStore, StorageError, ThreadRunStore, ThreadStore,
};
use awaken_runtime_contract::thread::Thread;

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
}

pub(crate) struct ThreadRunCheckpointStore {
    inner: Arc<dyn ThreadRunStore>,
}

impl ThreadRunCheckpointStore {
    pub(crate) fn new(inner: Arc<dyn ThreadRunStore>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl RuntimeCheckpointStore for ThreadRunCheckpointStore {
    async fn load_thread(&self, thread_id: &str) -> Result<Option<Thread>, StorageError> {
        ThreadStore::load_thread(self.inner.as_ref(), thread_id).await
    }

    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError> {
        ThreadStore::load_messages(self.inner.as_ref(), thread_id).await
    }

    async fn load_committed_messages(
        &self,
        thread_id: &str,
    ) -> Result<Option<Vec<Message>>, StorageError> {
        ThreadStore::load_committed_messages(self.inner.as_ref(), thread_id).await
    }

    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError> {
        RunStore::load_run(self.inner.as_ref(), run_id).await
    }

    async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError> {
        RunStore::latest_run(self.inner.as_ref(), thread_id).await
    }
}
