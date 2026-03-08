use crate::thread::Thread;
use crate::thread::ThreadChangeSet;
use crate::thread::Version;
use async_trait::async_trait;

use super::{
    paginate_in_memory, Committed, MessagePage, MessageQuery, RunPage, RunQuery, RunRecord,
    ThreadHead, ThreadListPage, ThreadListQuery, ThreadStoreError, VersionPrecondition,
};

#[async_trait]
pub trait ThreadReader: Send + Sync {
    /// Load an Thread and its current version.
    async fn load(&self, thread_id: &str) -> Result<Option<ThreadHead>, ThreadStoreError>;

    /// Load an Thread without version info. Convenience wrapper.
    async fn load_thread(&self, thread_id: &str) -> Result<Option<Thread>, ThreadStoreError> {
        Ok(self.load(thread_id).await?.map(|h| h.thread))
    }

    /// Load paginated messages for an Thread.
    async fn load_messages(
        &self,
        thread_id: &str,
        query: &MessageQuery,
    ) -> Result<MessagePage, ThreadStoreError> {
        let head = self
            .load(thread_id)
            .await?
            .ok_or_else(|| ThreadStoreError::NotFound(thread_id.to_string()))?;
        Ok(paginate_in_memory(&head.thread.messages, query))
    }

    /// List Thread ids.
    async fn list_threads(
        &self,
        query: &ThreadListQuery,
    ) -> Result<ThreadListPage, ThreadStoreError>;

    /// List all Thread ids with default paging.
    async fn list(&self) -> Result<Vec<String>, ThreadStoreError> {
        let page = self
            .list_threads(&ThreadListQuery {
                offset: 0,
                limit: 200,
                resource_id: None,
                parent_thread_id: None,
            })
            .await?;
        Ok(page.items)
    }

    /// List Thread ids with explicit query.
    async fn list_paginated(
        &self,
        query: &ThreadListQuery,
    ) -> Result<ThreadListPage, ThreadStoreError> {
        self.list_threads(query).await
    }

    /// Return total message count.
    async fn message_count(&self, thread_id: &str) -> Result<usize, ThreadStoreError> {
        let head = self
            .load(thread_id)
            .await?
            .ok_or_else(|| ThreadStoreError::NotFound(thread_id.to_string()))?;
        Ok(head.thread.messages.len())
    }

    // ----- run index (populated from `RunMeta` in changesets) -----

    /// Load one run record by run id.
    async fn load_run(&self, _run_id: &str) -> Result<Option<RunRecord>, ThreadStoreError> {
        Ok(None)
    }

    /// List runs with optional filtering and pagination.
    async fn list_runs(&self, _query: &RunQuery) -> Result<RunPage, ThreadStoreError> {
        Ok(RunPage {
            items: vec![],
            total: 0,
            has_more: false,
        })
    }

    /// Load the most recent non-terminal run for a thread, if any.
    async fn active_run_for_thread(
        &self,
        _thread_id: &str,
    ) -> Result<Option<RunRecord>, ThreadStoreError> {
        Ok(None)
    }
}

#[async_trait]
pub trait ThreadWriter: ThreadReader {
    /// Create a new Thread.
    async fn create(&self, thread: &Thread) -> Result<Committed, ThreadStoreError>;

    /// Append an ThreadChangeSet to an existing Thread.
    async fn append(
        &self,
        thread_id: &str,
        delta: &ThreadChangeSet,
        precondition: VersionPrecondition,
    ) -> Result<Committed, ThreadStoreError>;

    /// Delete an Thread.
    async fn delete(&self, thread_id: &str) -> Result<(), ThreadStoreError>;

    /// Upsert or replace the current persisted Thread.
    ///
    /// Implementations must provide atomic semantics suitable for their backend.
    async fn save(&self, thread: &Thread) -> Result<(), ThreadStoreError>;
}

#[async_trait]
pub trait ThreadSync: ThreadWriter {
    /// Load delta list appended after a specific version.
    async fn load_deltas(
        &self,
        thread_id: &str,
        after_version: Version,
    ) -> Result<Vec<ThreadChangeSet>, ThreadStoreError>;
}

/// Full storage trait.
pub trait ThreadStore: ThreadWriter {}

impl<T: ThreadWriter + ?Sized> ThreadStore for T {}
