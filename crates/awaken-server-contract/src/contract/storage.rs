pub use awaken_runtime_contract::contract::storage::*;

use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::contract::message::{Message, MessageRecord};
use awaken_runtime_contract::thread::{Thread, ThreadMetadata};

use crate::contract::scope::{ScopeId, scoped_key, unscoped_key};

#[derive(Clone)]
pub struct ScopedThreadRunStore {
    inner: Arc<dyn ThreadRunStore>,
    scope_id: ScopeId,
}

impl ScopedThreadRunStore {
    pub fn new(inner: Arc<dyn ThreadRunStore>, scope_id: ScopeId) -> Self {
        Self { inner, scope_id }
    }

    pub fn scope_id(&self) -> &ScopeId {
        &self.scope_id
    }

    pub fn inner(&self) -> &dyn ThreadRunStore {
        self.inner.as_ref()
    }

    fn scoped(&self, id: &str) -> String {
        scoped_key(&self.scope_id, id)
    }

    fn unscoped<'a>(&self, id: &'a str) -> Option<&'a str> {
        unscoped_key(&self.scope_id, id)
    }

    fn encode_thread(&self, thread: &Thread) -> Thread {
        let mut thread = thread.clone();
        thread.id = self.scoped(&thread.id);
        thread.parent_thread_id = thread.parent_thread_id.as_deref().map(|id| self.scoped(id));
        thread
    }

    fn decode_thread(&self, mut thread: Thread) -> Option<Thread> {
        thread.id = self.unscoped(&thread.id)?.to_string();
        thread.parent_thread_id = match thread.parent_thread_id.as_deref() {
            Some(id) => Some(self.unscoped(id)?.to_string()),
            None => None,
        };
        Some(thread)
    }

    fn encode_run(&self, run: &RunRecord) -> RunRecord {
        let mut run = run.clone();
        run.run_id = self.scoped(&run.run_id);
        run.thread_id = self.scoped(&run.thread_id);
        run.parent_run_id = run.parent_run_id.as_deref().map(|id| self.scoped(id));
        if let Some(input) = run.input.as_mut() {
            input.thread_id = self.scoped(&input.thread_id);
        }
        if let Some(output) = run.output.as_mut() {
            output.thread_id = self.scoped(&output.thread_id);
        }
        if let Some(request) = run.request.as_mut() {
            request.parent_thread_id = request
                .parent_thread_id
                .as_deref()
                .map(|id| self.scoped(id));
        }
        run
    }

    fn decode_run(&self, mut run: RunRecord) -> Option<RunRecord> {
        run.run_id = self.unscoped(&run.run_id)?.to_string();
        run.thread_id = self.unscoped(&run.thread_id)?.to_string();
        run.parent_run_id = match run.parent_run_id.as_deref() {
            Some(id) => Some(self.unscoped(id)?.to_string()),
            None => None,
        };
        if let Some(input) = run.input.as_mut() {
            input.thread_id = self.unscoped(&input.thread_id)?.to_string();
        }
        if let Some(output) = run.output.as_mut() {
            output.thread_id = self.unscoped(&output.thread_id)?.to_string();
        }
        if let Some(request) = run.request.as_mut() {
            request.parent_thread_id = match request.parent_thread_id.as_deref() {
                Some(id) => Some(self.unscoped(id)?.to_string()),
                None => None,
            };
        }
        Some(run)
    }

    fn decode_message_record(&self, mut record: MessageRecord) -> Option<MessageRecord> {
        record.thread_id = self.unscoped(&record.thread_id)?.to_string();
        if let Some(run_id) = record.produced_by_run_id.as_deref()
            && let Some(unscoped) = self.unscoped(run_id)
        {
            record.produced_by_run_id = Some(unscoped.to_string());
        }
        Some(record)
    }

    fn encode_message_query(&self, query: &MessageQuery) -> MessageQuery {
        let mut query = query.clone();
        query.run_id = query.run_id.as_deref().map(|id| self.scoped(id));
        query
    }
}

#[async_trait]
impl ThreadStore for ScopedThreadRunStore {
    async fn load_thread(&self, thread_id: &str) -> Result<Option<Thread>, StorageError> {
        Ok(self
            .inner
            .load_thread(&self.scoped(thread_id))
            .await?
            .and_then(|thread| self.decode_thread(thread)))
    }

    async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError> {
        self.inner.save_thread(&self.encode_thread(thread)).await
    }

    async fn delete_thread(&self, thread_id: &str) -> Result<(), StorageError> {
        self.inner.delete_thread(&self.scoped(thread_id)).await
    }

    async fn list_threads(&self, offset: usize, limit: usize) -> Result<Vec<String>, StorageError> {
        const SCAN_LIMIT: usize = 200;

        let mut inner_offset = 0;
        let mut scoped_ids = Vec::new();
        loop {
            let ids = self.inner.list_threads(inner_offset, SCAN_LIMIT).await?;
            if ids.is_empty() {
                break;
            }
            let count = ids.len();
            scoped_ids.extend(
                ids.into_iter()
                    .filter_map(|id| self.unscoped(&id).map(str::to_string)),
            );
            if count < SCAN_LIMIT {
                break;
            }
            inner_offset += count;
        }

        Ok(scoped_ids.into_iter().skip(offset).take(limit).collect())
    }

    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError> {
        self.inner.load_messages(&self.scoped(thread_id)).await
    }

    async fn load_committed_messages(
        &self,
        thread_id: &str,
    ) -> Result<Option<Vec<Message>>, StorageError> {
        self.inner
            .load_committed_messages(&self.scoped(thread_id))
            .await
    }

    async fn load_message_records(
        &self,
        thread_id: &str,
    ) -> Result<Option<Vec<MessageRecord>>, StorageError> {
        Ok(self
            .inner
            .load_message_records(&self.scoped(thread_id))
            .await?
            .map(|records| {
                records
                    .into_iter()
                    .filter_map(|record| self.decode_message_record(record))
                    .collect()
            }))
    }

    async fn list_message_records(
        &self,
        thread_id: &str,
        query: &MessageQuery,
    ) -> Result<MessagePage, StorageError> {
        let query = self.encode_message_query(query);
        let mut page = self
            .inner
            .list_message_records(&self.scoped(thread_id), &query)
            .await?;
        page.records = page
            .records
            .into_iter()
            .filter_map(|record| self.decode_message_record(record))
            .collect();
        Ok(page)
    }

    async fn append_message_records(
        &self,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<Vec<MessageRecord>, StorageError> {
        Ok(self
            .inner
            .append_message_records(&self.scoped(thread_id), messages)
            .await?
            .into_iter()
            .filter_map(|record| self.decode_message_record(record))
            .collect())
    }

    async fn save_messages(
        &self,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<(), StorageError> {
        self.inner
            .save_messages(&self.scoped(thread_id), messages)
            .await
    }

    async fn delete_messages(&self, thread_id: &str) -> Result<(), StorageError> {
        self.inner.delete_messages(&self.scoped(thread_id)).await
    }

    async fn update_thread_metadata(
        &self,
        id: &str,
        metadata: ThreadMetadata,
    ) -> Result<(), StorageError> {
        self.inner
            .update_thread_metadata(&self.scoped(id), metadata)
            .await
    }
}

#[async_trait]
impl RunStore for ScopedThreadRunStore {
    async fn create_run(&self, record: &RunRecord) -> Result<(), StorageError> {
        self.inner.create_run(&self.encode_run(record)).await
    }

    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError> {
        Ok(self
            .inner
            .load_run(&self.scoped(run_id))
            .await?
            .and_then(|record| self.decode_run(record)))
    }

    async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError> {
        Ok(self
            .inner
            .latest_run(&self.scoped(thread_id))
            .await?
            .and_then(|record| self.decode_run(record)))
    }

    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, StorageError> {
        let inner_query = RunQuery {
            offset: 0,
            limit: usize::MAX,
            thread_id: query.thread_id.as_deref().map(|id| self.scoped(id)),
            status: query.status,
        };
        let inner_page = self.inner.list_runs(&inner_query).await?;
        let mut items: Vec<RunRecord> = inner_page
            .items
            .into_iter()
            .filter_map(|record| self.decode_run(record))
            .collect();
        let total = items.len();
        let start = query.offset.min(total);
        items = items.into_iter().skip(start).take(query.limit).collect();
        let has_more = query.limit > 0 && start + items.len() < total;
        Ok(RunPage {
            items,
            total,
            has_more,
        })
    }
}

#[async_trait]
impl ThreadRunStore for ScopedThreadRunStore {
    #[allow(deprecated)]
    async fn checkpoint(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), StorageError> {
        self.inner
            .checkpoint(&self.scoped(thread_id), messages, &self.encode_run(run))
            .await
    }

    async fn checkpoint_append(
        &self,
        thread_id: &str,
        messages: &[Message],
        expected_version: Option<u64>,
        run: &RunRecord,
    ) -> Result<u64, StorageError> {
        self.inner
            .checkpoint_append(
                &self.scoped(thread_id),
                messages,
                expected_version,
                &self.encode_run(run),
            )
            .await
    }
}
