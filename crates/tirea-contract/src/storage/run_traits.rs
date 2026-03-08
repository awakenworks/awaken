use async_trait::async_trait;

use super::{RunPage, RunQuery, RunRecord, RunStoreError};

#[async_trait]
pub trait RunReader: Send + Sync {
    /// Load one run by run id.
    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, RunStoreError>;

    /// List runs with optional filtering and pagination.
    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, RunStoreError>;

    /// Resolve thread id from run id.
    async fn resolve_thread_id(&self, run_id: &str) -> Result<Option<String>, RunStoreError> {
        Ok(self.load_run(run_id).await?.map(|r| r.thread_id))
    }

    /// Load the most recent non-terminal run for a thread, if any.
    ///
    /// Returns the latest run whose status is not `Done`, ordered by
    /// `created_at` descending (with `updated_at` and `run_id` as tiebreakers).
    async fn load_current_run(&self, thread_id: &str) -> Result<Option<RunRecord>, RunStoreError>;
}

#[async_trait]
pub trait RunWriter: RunReader {
    /// Upsert one run record.
    async fn upsert_run(&self, record: &RunRecord) -> Result<(), RunStoreError>;

    /// Delete one run record by run id.
    async fn delete_run(&self, run_id: &str) -> Result<(), RunStoreError>;
}

/// Full run projection store trait.
pub trait RunStore: RunWriter {}

impl<T: RunWriter + ?Sized> RunStore for T {}
