//! Server/store-owned thread & run persistence traits.
//!
//! Moved out of `awaken-runtime-contract`: the runtime perceives durable
//! storage only through the narrow `RuntimeCheckpointStore` read port and the
//! `CommitCoordinator` write boundary. The full CRUD + query surface
//! (`ThreadStore`, `RunStore`, `ThreadRunStore`) is a server/store concern and
//! lives here. Storage data types, query/page types, and pagination helpers
//! stay in runtime-contract and are pulled in via the glob below.

use crate::contract::storage::*;
use async_trait::async_trait;
use awaken_runtime_contract::contract::message::{Message, MessageRecord};
use awaken_runtime_contract::thread::{Thread, normalize_lineage_id};

// ── ThreadStore ─────────────────────────────────────────────────────

/// Thread read/write persistence.
///
/// Thread metadata and messages are stored separately. Messages have a
/// single source of truth through `load_messages` / `save_messages`.
#[async_trait]
pub trait ThreadStore: Send + Sync {
    /// Load a thread by ID. Returns `None` if not found.
    async fn load_thread(&self, thread_id: &str) -> Result<Option<Thread>, StorageError>;

    /// Persist a thread (create or overwrite).
    ///
    /// This is a low-level persistence primitive. Callers that change
    /// parent-child relationships should use [`ThreadStore::save_thread_validated`]
    /// so hierarchy invariants are checked against current store state.
    async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError>;

    /// Persist a thread after validating parent-child hierarchy invariants.
    ///
    /// The default implementation validates and then delegates to
    /// [`ThreadStore::save_thread`]. It is not atomic across those steps.
    /// with a backend-native atomic or fenced implementation.
    async fn save_thread_validated(&self, thread: &Thread) -> Result<(), StorageError> {
        self.validate_thread_hierarchy(&thread.id, thread.parent_thread_id.as_deref())
            .await?;
        self.save_thread(thread).await
    }

    /// Delete a thread and its associated messages.
    ///
    /// This is a low-level delete primitive. Callers that need hierarchy-aware
    /// child handling should use [`ThreadStore::delete_thread_with_strategy`].
    async fn delete_thread(&self, thread_id: &str) -> Result<(), StorageError>;

    /// Persist thread-scoped state for `thread_id` (overwrite the prior value).
    ///
    /// Default is a no-op for stores that do not yet persist thread-scoped
    /// state; the runtime then keeps state on the run record only. Production
    /// stores override this and pair it with [`ThreadStore::load_thread_state`].
    async fn save_thread_state(
        &self,
        thread_id: &str,
        state: &awaken_runtime_contract::state::PersistedState,
    ) -> Result<(), StorageError> {
        let _ = (thread_id, state);
        Ok(())
    }

    /// Load thread-scoped state for `thread_id`, if any. Default `None`.
    async fn load_thread_state(
        &self,
        thread_id: &str,
    ) -> Result<Option<awaken_runtime_contract::state::PersistedState>, StorageError> {
        let _ = thread_id;
        Ok(None)
    }

    /// Delete a thread while managing direct and transitive children.
    ///
    /// The default implementation performs multiple low-level writes and is
    /// not atomic across child updates and the final delete. Production stores
    /// with concurrent writers should override this method with a transactional
    /// or otherwise fenced implementation.
    async fn delete_thread_with_strategy(
        &self,
        thread_id: &str,
        strategy: ChildThreadDeleteStrategy,
    ) -> Result<(), StorageError> {
        if self.load_thread(thread_id).await?.is_none() {
            return Err(StorageError::NotFound(thread_id.to_owned()));
        }

        match strategy {
            ChildThreadDeleteStrategy::Reject => {
                let children = self.list_child_threads(thread_id).await?;
                if !children.is_empty() {
                    return Err(StorageError::Validation(format!(
                        "thread '{thread_id}' has child threads; choose 'detach' or 'cascade'"
                    )));
                }
                self.delete_thread(thread_id).await
            }
            ChildThreadDeleteStrategy::Detach => {
                let mut children = self.list_child_threads(thread_id).await?;
                let updated_at = crate::now_ms();
                for child in &mut children {
                    child.parent_thread_id = None;
                    child.metadata.updated_at = Some(updated_at);
                    self.save_thread(child).await?;
                }
                self.delete_thread(thread_id).await
            }
            ChildThreadDeleteStrategy::Cascade => {
                let mut visited = std::collections::HashSet::new();
                let mut stack = vec![(thread_id.to_owned(), false)];
                let mut delete_order = Vec::new();

                while let Some((current_thread_id, expanded)) = stack.pop() {
                    if expanded {
                        delete_order.push(current_thread_id);
                        continue;
                    }

                    if !visited.insert(current_thread_id.clone()) {
                        return Err(StorageError::Validation(format!(
                            "thread hierarchy cycle detected while deleting '{thread_id}'"
                        )));
                    }

                    stack.push((current_thread_id.clone(), true));
                    let mut children = self.list_child_threads(&current_thread_id).await?;
                    children.sort_by(|left, right| left.id.cmp(&right.id));
                    for child in children.into_iter().rev() {
                        stack.push((child.id, false));
                    }
                }

                for id in delete_order {
                    self.delete_thread(&id).await?;
                }
                Ok(())
            }
        }
    }

    /// List thread IDs with pagination.
    async fn list_threads(&self, offset: usize, limit: usize) -> Result<Vec<String>, StorageError>;

    /// List thread IDs with first-class filters and page metadata.
    async fn list_threads_query(&self, query: &ThreadQuery) -> Result<ThreadPage, StorageError> {
        const SCAN_LIMIT: usize = 200;

        let mut offset = 0;
        let mut threads = Vec::new();
        loop {
            let ids = self.list_threads(offset, SCAN_LIMIT).await?;
            if ids.is_empty() {
                break;
            }
            let count = ids.len();
            for id in ids {
                if let Some(thread) = self.load_thread(&id).await? {
                    threads.push(thread);
                }
            }
            if count < SCAN_LIMIT {
                break;
            }
            offset += count;
        }

        Ok(paginate_threads(threads, query))
    }

    /// Load all direct child threads for a given parent thread.
    async fn list_child_threads(
        &self,
        parent_thread_id: &str,
    ) -> Result<Vec<Thread>, StorageError> {
        const PAGE_LIMIT: usize = 200;

        let mut offset = 0;
        let mut children = Vec::new();
        loop {
            let query = ThreadQuery {
                offset,
                limit: PAGE_LIMIT,
                resource_id: None,
                parent_filter: ThreadParentFilter::Parent(parent_thread_id.to_owned()),
                id_prefix: None,
            };
            let page = self.list_threads_query(&query).await?;
            let count = page.items.len();
            for id in page.items {
                if let Some(thread) = self.load_thread(&id).await? {
                    children.push(thread);
                }
            }
            if !page.has_more || count == 0 {
                break;
            }
            offset = page
                .next_cursor
                .as_deref()
                .and_then(|cursor| query.decode_cursor(cursor).ok())
                .unwrap_or(offset.saturating_add(count));
        }
        Ok(children)
    }

    /// Validate parent-child hierarchy invariants for a thread.
    async fn validate_thread_hierarchy(
        &self,
        thread_id: &str,
        parent_thread_id: Option<&str>,
    ) -> Result<(), StorageError> {
        let Some(parent_thread_id) = normalize_lineage_id(parent_thread_id) else {
            return Ok(());
        };
        if parent_thread_id == thread_id {
            return Err(StorageError::Validation(format!(
                "thread '{thread_id}' cannot parent itself"
            )));
        }

        let root_parent_thread_id = parent_thread_id.to_owned();
        let mut current_thread_id = root_parent_thread_id.clone();
        let mut visited = std::collections::HashSet::from([thread_id.to_owned()]);

        loop {
            if !visited.insert(current_thread_id.clone()) {
                return Err(StorageError::Validation(format!(
                    "thread hierarchy cycle detected at '{current_thread_id}'"
                )));
            }

            let Some(thread) = self.load_thread(&current_thread_id).await? else {
                let message = if current_thread_id == root_parent_thread_id {
                    format!("parent thread not found: {root_parent_thread_id}")
                } else {
                    format!("thread hierarchy references missing ancestor '{current_thread_id}'")
                };
                return Err(StorageError::Validation(message));
            };

            let Some(next_parent_thread_id) =
                normalize_lineage_id(thread.parent_thread_id.as_deref())
            else {
                return Ok(());
            };
            current_thread_id = next_parent_thread_id;
        }
    }

    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError>;

    async fn load_committed_messages(
        &self,
        thread_id: &str,
    ) -> Result<Option<Vec<Message>>, StorageError> {
        self.load_messages(thread_id).await
    }

    async fn load_message_records(
        &self,
        thread_id: &str,
    ) -> Result<Option<Vec<MessageRecord>>, StorageError> {
        let Some(messages) = self.load_messages(thread_id).await? else {
            return Ok(None);
        };
        Ok(Some(
            messages
                .into_iter()
                .enumerate()
                .map(|(index, message)| {
                    MessageRecord::from_message(thread_id.to_string(), index as u64 + 1, message)
                })
                .collect(),
        ))
    }

    /// List thread-owned message records with filtering and page metadata.
    async fn list_message_records(
        &self,
        thread_id: &str,
        query: &MessageQuery,
    ) -> Result<MessagePage, StorageError> {
        let Some(records) = self.load_message_records(thread_id).await? else {
            return Ok(MessagePage::empty());
        };
        Ok(paginate_message_records(records, query))
    }

    /// Append messages to a thread's durable log and return their records.
    async fn append_message_records(
        &self,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<Vec<MessageRecord>, StorageError> {
        let mut existing = self
            .load_committed_messages(thread_id)
            .await?
            .unwrap_or_default();
        message_append::validate_append_only_delta(&existing, messages)?;
        let start_seq = existing.len() as u64 + 1;
        existing.extend(messages.iter().cloned());
        self.save_messages(thread_id, &existing).await?;
        Ok(messages
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, message)| {
                MessageRecord::from_message(
                    thread_id.to_string(),
                    start_seq + index as u64,
                    message,
                )
            })
            .collect())
    }

    /// Load one message record by message ID.
    async fn load_message_record(
        &self,
        thread_id: &str,
        message_id: &str,
    ) -> Result<Option<MessageRecord>, StorageError> {
        let Some(records) = self.load_message_records(thread_id).await? else {
            return Ok(None);
        };
        Ok(records
            .into_iter()
            .find(|record| record.message_id == message_id))
    }

    /// Load message records by inclusive sequence range.
    async fn load_message_records_range(
        &self,
        thread_id: &str,
        range: MessageSeqRange,
    ) -> Result<Vec<MessageRecord>, StorageError> {
        let Some(records) = self.load_message_records(thread_id).await? else {
            return Ok(Vec::new());
        };
        Ok(records
            .into_iter()
            .filter(|record| record.seq >= range.from_seq && record.seq <= range.to_seq)
            .collect())
    }

    /// Persist messages for a thread (full overwrite).
    async fn save_messages(
        &self,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<(), StorageError>;

    /// Delete all messages for a thread. Returns `NotFound` if the thread does not exist.
    async fn delete_messages(&self, thread_id: &str) -> Result<(), StorageError>;

    /// Update only the metadata of an existing thread.
    /// Returns `NotFound` if the thread does not exist.
    async fn update_thread_metadata(
        &self,
        id: &str,
        metadata: crate::thread::ThreadMetadata,
    ) -> Result<(), StorageError>;
}

// ── RunStore ────────────────────────────────────────────────────────

/// Run record persistence.
#[async_trait]
pub trait RunStore: Send + Sync {
    /// Create a new run record.
    async fn create_run(&self, record: &RunRecord) -> Result<(), StorageError>;

    /// Load a run record by `run_id`.
    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError>;

    /// Find the latest run for a thread (by `updated_at`).
    async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError>;

    /// List runs with optional filtering and pagination.
    async fn list_runs(&self, query: &RunQuery) -> Result<RunPage, StorageError>;
}

// ── ThreadRunStore (convenience) ────────────────────────────────────

/// Atomic thread+run checkpoint persistence. ADR-0038 D7: prefer
/// [`CommitCoordinator::commit_checkpoint`](super::commit_coordinator::CommitCoordinator::commit_checkpoint)
/// for production writes; `checkpoint` is retained for conformance tests
/// and coordinator-internal use.
#[async_trait]
pub trait ThreadRunStore: ThreadStore + RunStore + Send + Sync {
    #[deprecated(since = "0.6.0", note = "use CommitCoordinator (ADR-0038 D7)")]
    async fn checkpoint(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), StorageError>;

    /// Append to the committed log and persist `run`, guarded by message count.
    #[allow(deprecated)]
    async fn checkpoint_append(
        &self,
        thread_id: &str,
        messages: &[Message],
        expected_version: Option<u64>,
        run: &RunRecord,
    ) -> Result<u64, StorageError> {
        let existing = self
            .load_committed_messages(thread_id)
            .await?
            .unwrap_or_default();
        let actual = existing.len() as u64;
        if let Some(expected) = expected_version
            && expected != actual
        {
            return Err(StorageError::VersionConflict { expected, actual });
        }
        let mut merged = existing;
        message_append::merge_checkpoint_append_messages(&mut merged, messages)?;
        let new_version = merged.len() as u64;
        self.checkpoint(thread_id, &merged, run).await?;
        Ok(new_version)
    }

    /// Read a consistent [`CheckpointSnapshot`] for resume (ADR-0038 C5).
    ///
    /// The default composes the committed-message, latest-run, and
    /// thread-state reads and applies the committed-history view filter.
    /// Backends that can read atomically (a transaction or lock spanning all
    /// three) override this to avoid torn reads against a concurrent commit.
    async fn load_checkpoint(
        &self,
        thread_id: &str,
    ) -> Result<Option<CheckpointSnapshot>, StorageError> {
        let committed = ThreadStore::load_committed_messages(self, thread_id).await?;
        let latest_run = RunStore::latest_run(self, thread_id).await?;
        if committed.is_none() && latest_run.is_none() {
            return Ok(None);
        }
        let raw = committed.unwrap_or_default();
        let message_version = raw.len() as u64;
        let messages =
            awaken_runtime_contract::contract::message::effective_committed_view(raw, thread_id);
        let thread_state = ThreadStore::load_thread_state(self, thread_id).await?;
        Ok(Some(CheckpointSnapshot {
            messages,
            message_version,
            latest_run,
            thread_state,
        }))
    }
}

/// Adapts a [`ThreadRunStore`] into a [`RuntimeCheckpointStore`] for the agent
/// loop. Exposed so embedders/tests can supply a checkpoint reader backed by
/// any `ThreadRunStore`.
pub struct ThreadRunCheckpointStore {
    inner: std::sync::Arc<dyn ThreadRunStore>,
}

impl ThreadRunCheckpointStore {
    pub fn new(inner: std::sync::Arc<dyn ThreadRunStore>) -> Self {
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

    async fn load_thread_state(
        &self,
        thread_id: &str,
    ) -> Result<Option<awaken_runtime_contract::state::PersistedState>, StorageError> {
        ThreadStore::load_thread_state(self.inner.as_ref(), thread_id).await
    }

    async fn load_checkpoint(
        &self,
        thread_id: &str,
    ) -> Result<Option<CheckpointSnapshot>, StorageError> {
        // Delegate to the store's (possibly atomic) consistent read.
        ThreadRunStore::load_checkpoint(self.inner.as_ref(), thread_id).await
    }
}

#[cfg(test)]
#[path = "store_traits_tests.rs"]
mod tests;
