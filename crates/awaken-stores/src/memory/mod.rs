//! In-memory storage backend for testing and local development.
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use awaken_contract::contract::config_store::{
    ConfigChangeEvent, ConfigChangeKind, ConfigChangeNotifier, ConfigChangeSubscriber, ConfigStore,
    extract_meta_revision,
};
use awaken_contract::contract::message::{
    Message, PendingMessageRecord, strip_unpaired_tool_calls_from_view,
};
use awaken_contract::contract::profile_store::{ProfileEntry, ProfileOwner, ProfileStore};
use awaken_contract::contract::storage::{
    MessagePage, MessageQuery, RunPage, RunQuery, RunRecord, RunStore, StorageError, ThreadPage,
    ThreadQuery, ThreadRunStore, ThreadStore, checkpoint_parent_thread_id, message_append,
    paginate_message_records, paginate_threads,
};
use awaken_contract::thread::{Thread, normalize_lineage_id};
use serde_json::Value;
use tokio::sync::RwLock;

mod pending;

/// In-memory storage implementing all four store traits.
///
/// Uses `tokio::sync::RwLock` for async-safe concurrent access.
/// Data lives only in memory and is lost when the store is dropped.
#[derive(Debug)]
pub struct InMemoryStore {
    pub(crate) threads: RwLock<HashMap<String, Thread>>,
    pub(crate) runs: RwLock<HashMap<String, RunRecord>>,
    /// Monotonic per-run insertion sequence used as a `latest_run` tie-break.
    /// `created_at`/`updated_at` are second-precision, so multiple runs in
    /// the same wall-clock second otherwise yield non-deterministic
    /// ordering and silently break agent-binding inference.
    pub(crate) run_insertion: RwLock<HashMap<String, u64>>,
    pub(crate) run_seq: AtomicU64,
    /// Thread ID -> ordered messages (single source of truth).
    pub(crate) messages: RwLock<HashMap<String, Vec<Message>>>,
    /// Thread ID -> ordered pending messages not yet consumed by a run.
    pub(crate) pending_messages: RwLock<HashMap<String, Vec<PendingMessageRecord>>>,
    /// Profile entries keyed by (owner, key).
    profiles: RwLock<HashMap<ProfileOwner, HashMap<String, ProfileEntry>>>,
    /// Config entries keyed by namespace then ID.
    configs: RwLock<HashMap<String, HashMap<String, Value>>>,
    /// Broadcast sender for config change notifications.
    config_change_tx: tokio::sync::broadcast::Sender<ConfigChangeEvent>,
}

impl InMemoryStore {
    /// Create a new empty in-memory store.
    pub fn new() -> Self {
        let (config_change_tx, _) = tokio::sync::broadcast::channel(256);
        Self {
            threads: RwLock::new(HashMap::new()),
            runs: RwLock::new(HashMap::new()),
            run_insertion: RwLock::new(HashMap::new()),
            run_seq: AtomicU64::new(0),
            messages: RwLock::new(HashMap::new()),
            pending_messages: RwLock::new(HashMap::new()),
            profiles: RwLock::new(HashMap::new()),
            configs: RwLock::new(HashMap::new()),
            config_change_tx,
        }
    }

    fn next_run_seq(&self) -> u64 {
        self.run_seq.fetch_add(1, Ordering::Relaxed)
    }
}

impl Default for InMemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

fn validate_thread_hierarchy_map(
    threads: &HashMap<String, Thread>,
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

    let root_parent_thread_id = parent_thread_id.clone();
    let mut current_thread_id = parent_thread_id;
    let mut visited = std::collections::HashSet::from([thread_id.to_owned()]);

    loop {
        if !visited.insert(current_thread_id.clone()) {
            return Err(StorageError::Validation(format!(
                "thread hierarchy cycle detected at '{current_thread_id}'"
            )));
        }

        let Some(thread) = threads.get(&current_thread_id) else {
            let message = if current_thread_id == root_parent_thread_id {
                format!("parent thread not found: {root_parent_thread_id}")
            } else {
                format!("thread hierarchy references missing ancestor '{current_thread_id}'")
            };
            return Err(StorageError::Validation(message));
        };

        let Some(next_parent_thread_id) = normalize_lineage_id(thread.parent_thread_id.as_deref())
        else {
            return Ok(());
        };
        current_thread_id = next_parent_thread_id;
    }
}

fn collect_child_ids(threads: &HashMap<String, Thread>, parent_thread_id: &str) -> Vec<String> {
    let mut child_ids: Vec<String> = threads
        .values()
        .filter(|thread| thread.parent_thread_id.as_deref() == Some(parent_thread_id))
        .map(|thread| thread.id.clone())
        .collect();
    child_ids.sort();
    child_ids
}

// ── ThreadStore ─────────────────────────────────────────────────────

#[async_trait]
impl ThreadStore for InMemoryStore {
    async fn load_thread(&self, thread_id: &str) -> Result<Option<Thread>, StorageError> {
        let guard = self.threads.read().await;
        Ok(guard.get(thread_id).cloned())
    }

    async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError> {
        let mut normalized = thread.clone();
        normalized.normalize_lineage();
        let mut guard = self.threads.write().await;
        guard.insert(thread.id.clone(), normalized);
        Ok(())
    }

    async fn save_thread_validated(&self, thread: &Thread) -> Result<(), StorageError> {
        let mut normalized = thread.clone();
        normalized.normalize_lineage();
        let mut guard = self.threads.write().await;
        validate_thread_hierarchy_map(
            &guard,
            &normalized.id,
            normalized.parent_thread_id.as_deref(),
        )?;
        guard.insert(normalized.id.clone(), normalized);
        Ok(())
    }

    async fn delete_thread(&self, thread_id: &str) -> Result<(), StorageError> {
        let mut threads = self.threads.write().await;
        let mut messages = self.messages.write().await;
        let mut pending_messages = self.pending_messages.write().await;
        threads.remove(thread_id);
        messages.remove(thread_id);
        pending_messages.remove(thread_id);
        Ok(())
    }

    async fn delete_thread_with_strategy(
        &self,
        thread_id: &str,
        strategy: awaken_contract::contract::storage::ChildThreadDeleteStrategy,
    ) -> Result<(), StorageError> {
        let mut threads = self.threads.write().await;
        let mut messages = self.messages.write().await;
        let mut pending_messages = self.pending_messages.write().await;
        if !threads.contains_key(thread_id) {
            return Err(StorageError::NotFound(thread_id.to_owned()));
        }

        match strategy {
            awaken_contract::contract::storage::ChildThreadDeleteStrategy::Reject => {
                if !collect_child_ids(&threads, thread_id).is_empty() {
                    return Err(StorageError::Validation(format!(
                        "thread '{thread_id}' has child threads; choose 'detach' or 'cascade'"
                    )));
                }
                threads.remove(thread_id);
                messages.remove(thread_id);
                pending_messages.remove(thread_id);
            }
            awaken_contract::contract::storage::ChildThreadDeleteStrategy::Detach => {
                let updated_at = current_millis();
                for child_id in collect_child_ids(&threads, thread_id) {
                    if let Some(child) = threads.get_mut(&child_id) {
                        child.parent_thread_id = None;
                        child.normalize_lineage();
                        child.metadata.updated_at = Some(updated_at);
                    }
                }
                threads.remove(thread_id);
                messages.remove(thread_id);
                pending_messages.remove(thread_id);
            }
            awaken_contract::contract::storage::ChildThreadDeleteStrategy::Cascade => {
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
                    for child_id in collect_child_ids(&threads, &current_thread_id)
                        .into_iter()
                        .rev()
                    {
                        stack.push((child_id, false));
                    }
                }

                for id in delete_order {
                    threads.remove(&id);
                    messages.remove(&id);
                    pending_messages.remove(&id);
                }
            }
        }

        Ok(())
    }

    async fn list_threads(&self, offset: usize, limit: usize) -> Result<Vec<String>, StorageError> {
        let guard = self.threads.read().await;
        let mut threads: Vec<Thread> = guard.values().cloned().collect();
        awaken_contract::contract::storage::sort_threads_by_recent_activity(&mut threads);
        Ok(threads
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|thread| thread.id)
            .collect())
    }

    async fn list_threads_query(&self, query: &ThreadQuery) -> Result<ThreadPage, StorageError> {
        let guard = self.threads.read().await;
        let threads: Vec<Thread> = guard.values().cloned().collect();
        Ok(paginate_threads(threads, query))
    }

    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError> {
        let guard = self.messages.read().await;
        Ok(guard.get(thread_id).cloned().map(|mut messages| {
            strip_unpaired_tool_calls_from_view(&mut messages);
            messages
        }))
    }

    async fn load_committed_messages(
        &self,
        thread_id: &str,
    ) -> Result<Option<Vec<Message>>, StorageError> {
        let guard = self.messages.read().await;
        Ok(guard.get(thread_id).cloned())
    }

    async fn list_message_records(
        &self,
        thread_id: &str,
        query: &MessageQuery,
    ) -> Result<MessagePage, StorageError> {
        let guard = self.messages.read().await;
        let Some(messages) = guard.get(thread_id) else {
            return Ok(MessagePage::empty());
        };
        let mut messages = messages.clone();
        strip_unpaired_tool_calls_from_view(&mut messages);
        let records = messages
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, message)| {
                awaken_contract::contract::message::MessageRecord::from_message(
                    thread_id.to_owned(),
                    index as u64 + 1,
                    message,
                )
            })
            .collect();
        Ok(paginate_message_records(records, query))
    }

    async fn save_messages(
        &self,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<(), StorageError> {
        let mut guard = self.messages.write().await;
        guard.insert(thread_id.to_owned(), messages.to_vec());
        Ok(())
    }

    /// Atomic append: holds the messages write lock across the whole
    /// read-modify-write, so concurrent writers (including separate `Mailbox`
    /// instances sharing this store) never lose an append. Overrides the
    /// non-atomic default `load → extend → save` (ADR-0042 D4/D5).
    async fn append_message_records(
        &self,
        thread_id: &str,
        messages: &[Message],
    ) -> Result<Vec<awaken_contract::contract::message::MessageRecord>, StorageError> {
        let mut guard = self.messages.write().await;
        let existing = guard.entry(thread_id.to_owned()).or_default();
        message_append::validate_append_only_delta(existing, messages)?;
        let start_seq = existing.len() as u64 + 1;
        existing.extend(messages.iter().cloned());
        Ok(messages
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, message)| {
                awaken_contract::contract::message::MessageRecord::from_message(
                    thread_id.to_owned(),
                    start_seq + index as u64,
                    message,
                )
            })
            .collect())
    }

    async fn delete_messages(&self, thread_id: &str) -> Result<(), StorageError> {
        let threads = self.threads.read().await;
        if !threads.contains_key(thread_id) {
            return Err(StorageError::NotFound(thread_id.to_owned()));
        }
        drop(threads);
        let mut guard = self.messages.write().await;
        guard.remove(thread_id);
        Ok(())
    }

    async fn update_thread_metadata(
        &self,
        id: &str,
        metadata: awaken_contract::thread::ThreadMetadata,
    ) -> Result<(), StorageError> {
        let mut guard = self.threads.write().await;
        let thread = guard
            .get_mut(id)
            .ok_or_else(|| StorageError::NotFound(id.to_owned()))?;
        thread.metadata = metadata;
        Ok(())
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
        self.run_insertion
            .write()
            .await
            .insert(record.run_id.clone(), self.next_run_seq());
        Ok(())
    }

    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError> {
        let guard = self.runs.read().await;
        Ok(guard.get(run_id).cloned())
    }

    async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError> {
        let runs = self.runs.read().await;
        let insertion = self.run_insertion.read().await;
        // Tie-break on insertion sequence so that two runs that landed in
        // the same wall-clock second still have a deterministic order.
        Ok(runs
            .values()
            .filter(|r| r.thread_id == thread_id)
            .max_by_key(|r| (r.updated_at, insertion.get(&r.run_id).copied().unwrap_or(0)))
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

// ── ThreadRunStore ──────────────────────────────────────────────────

#[async_trait]
impl ThreadRunStore for InMemoryStore {
    async fn checkpoint(
        &self,
        thread_id: &str,
        messages: &[Message],
        run: &RunRecord,
    ) -> Result<(), StorageError> {
        let now = current_millis();
        let mut thread_guard = self.threads.write().await;
        let existing_thread = thread_guard.get(thread_id).cloned();
        validate_thread_hierarchy_map(
            &thread_guard,
            thread_id,
            checkpoint_parent_thread_id(existing_thread.as_ref(), run),
        )?;
        let mut msg_guard = self.messages.write().await;
        let mut run_guard = self.runs.write().await;
        let mut thread = existing_thread.unwrap_or_else(|| Thread::with_id(thread_id));
        thread.touch(now);
        thread.apply_run_projection(run);
        thread.normalize_lineage();
        thread_guard.insert(thread_id.to_owned(), thread);
        msg_guard.insert(thread_id.to_owned(), messages.to_vec());
        run_guard.insert(run.run_id.clone(), run.clone());
        // Refresh the insertion order on every checkpoint so the run that
        // most recently advanced becomes the new `latest_run` even when
        // updated_at ties with an earlier run on the same thread.
        self.run_insertion
            .write()
            .await
            .insert(run.run_id.clone(), self.next_run_seq());
        Ok(())
    }

    /// Atomic, version-guarded committed append: holds the thread/message/run
    /// write locks across the read-check-append-write so concurrent writers
    /// (including separate `Mailbox` instances sharing this store) never lose
    /// an append (ADR-0042 D5). A stale `expected_version` leaves all state
    /// untouched.
    async fn checkpoint_append(
        &self,
        thread_id: &str,
        messages: &[Message],
        expected_version: Option<u64>,
        run: &RunRecord,
    ) -> Result<u64, StorageError> {
        let now = current_millis();
        let mut thread_guard = self.threads.write().await;
        let existing_thread = thread_guard.get(thread_id).cloned();
        validate_thread_hierarchy_map(
            &thread_guard,
            thread_id,
            checkpoint_parent_thread_id(existing_thread.as_ref(), run),
        )?;
        let mut msg_guard = self.messages.write().await;
        let mut run_guard = self.runs.write().await;
        let actual = msg_guard
            .get(thread_id)
            .map(|messages| messages.len() as u64)
            .unwrap_or(0);
        if let Some(expected) = expected_version
            && expected != actual
        {
            return Err(StorageError::VersionConflict { expected, actual });
        }
        let committed = msg_guard.entry(thread_id.to_owned()).or_default();
        message_append::merge_checkpoint_append_messages(committed, messages)?;
        let new_version = committed.len() as u64;
        let mut thread = existing_thread.unwrap_or_else(|| Thread::with_id(thread_id));
        thread.touch(now);
        thread.apply_run_projection(run);
        thread.normalize_lineage();
        thread_guard.insert(thread_id.to_owned(), thread);
        run_guard.insert(run.run_id.clone(), run.clone());
        self.run_insertion
            .write()
            .await
            .insert(run.run_id.clone(), self.next_run_seq());
        Ok(new_version)
    }
}

// ── ProfileStore ────────────────────────────────────────────────────

use crate::current_millis;

#[async_trait]
impl ProfileStore for InMemoryStore {
    async fn get(
        &self,
        owner: &ProfileOwner,
        key: &str,
    ) -> Result<Option<ProfileEntry>, StorageError> {
        let guard = self.profiles.read().await;
        Ok(guard.get(owner).and_then(|inner| inner.get(key)).cloned())
    }

    async fn set(&self, owner: &ProfileOwner, key: &str, value: Value) -> Result<(), StorageError> {
        let mut guard = self.profiles.write().await;
        let inner = guard.entry(owner.clone()).or_default();
        inner.insert(
            key.to_owned(),
            ProfileEntry {
                key: key.to_owned(),
                value,
                updated_at: current_millis(),
            },
        );
        Ok(())
    }

    async fn delete(&self, owner: &ProfileOwner, key: &str) -> Result<(), StorageError> {
        let mut guard = self.profiles.write().await;
        if let Some(inner) = guard.get_mut(owner) {
            inner.remove(key);
        }
        Ok(())
    }

    async fn list(&self, owner: &ProfileOwner) -> Result<Vec<ProfileEntry>, StorageError> {
        let guard = self.profiles.read().await;
        let mut entries: Vec<ProfileEntry> = guard
            .get(owner)
            .map(|inner| inner.values().cloned().collect())
            .unwrap_or_default();
        entries.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(entries)
    }

    async fn clear_owner(&self, owner: &ProfileOwner) -> Result<(), StorageError> {
        let mut guard = self.profiles.write().await;
        guard.remove(owner);
        Ok(())
    }
}

// ── ConfigStore ─────────────────────────────────────────────────────

#[async_trait]
impl ConfigStore for InMemoryStore {
    async fn get(&self, namespace: &str, id: &str) -> Result<Option<Value>, StorageError> {
        let guard = self.configs.read().await;
        Ok(guard
            .get(namespace)
            .and_then(|entries| entries.get(id))
            .cloned())
    }

    async fn list(
        &self,
        namespace: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<(String, Value)>, StorageError> {
        let guard = self.configs.read().await;
        let Some(entries) = guard.get(namespace) else {
            return Ok(Vec::new());
        };
        let mut items: Vec<_> = entries
            .iter()
            .map(|(id, value)| (id.clone(), value.clone()))
            .collect();
        items.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(items.into_iter().skip(offset).take(limit).collect())
    }

    async fn put(&self, namespace: &str, id: &str, value: &Value) -> Result<(), StorageError> {
        let mut guard = self.configs.write().await;
        guard
            .entry(namespace.to_string())
            .or_default()
            .insert(id.to_string(), value.clone());
        drop(guard);
        let _ = self.config_change_tx.send(ConfigChangeEvent {
            namespace: namespace.to_string(),
            id: id.to_string(),
            kind: ConfigChangeKind::Put,
        });
        Ok(())
    }

    async fn put_if_absent(
        &self,
        namespace: &str,
        id: &str,
        value: &Value,
    ) -> Result<(), StorageError> {
        let mut guard = self.configs.write().await;
        let entries = guard.entry(namespace.to_string()).or_default();
        if entries.contains_key(id) {
            return Err(StorageError::AlreadyExists(format!("{namespace}/{id}")));
        }
        entries.insert(id.to_string(), value.clone());
        drop(guard);
        let _ = self.config_change_tx.send(ConfigChangeEvent {
            namespace: namespace.to_string(),
            id: id.to_string(),
            kind: ConfigChangeKind::Put,
        });
        Ok(())
    }

    async fn delete(&self, namespace: &str, id: &str) -> Result<(), StorageError> {
        let mut guard = self.configs.write().await;
        if let Some(entries) = guard.get_mut(namespace) {
            entries.remove(id);
        }
        drop(guard);
        let _ = self.config_change_tx.send(ConfigChangeEvent {
            namespace: namespace.to_string(),
            id: id.to_string(),
            kind: ConfigChangeKind::Delete,
        });
        Ok(())
    }

    async fn put_if_revision(
        &self,
        namespace: &str,
        id: &str,
        value: &Value,
        expected_revision: u64,
    ) -> Result<(), StorageError> {
        let mut guard = self.configs.write().await;
        let actual = guard
            .get(namespace)
            .and_then(|entries| entries.get(id))
            .and_then(extract_meta_revision)
            .unwrap_or(0);
        if actual != expected_revision {
            return Err(StorageError::VersionConflict {
                expected: expected_revision,
                actual,
            });
        }
        guard
            .entry(namespace.to_string())
            .or_default()
            .insert(id.to_string(), value.clone());
        drop(guard);
        let _ = self.config_change_tx.send(ConfigChangeEvent {
            namespace: namespace.to_string(),
            id: id.to_string(),
            kind: ConfigChangeKind::Put,
        });
        Ok(())
    }

    async fn delete_if_revision(
        &self,
        namespace: &str,
        id: &str,
        expected_revision: u64,
    ) -> Result<(), StorageError> {
        let mut guard = self.configs.write().await;
        let actual = guard
            .get(namespace)
            .and_then(|entries| entries.get(id))
            .and_then(extract_meta_revision)
            .unwrap_or(0);
        if actual != expected_revision {
            return Err(StorageError::VersionConflict {
                expected: expected_revision,
                actual,
            });
        }
        if let Some(entries) = guard.get_mut(namespace) {
            entries.remove(id);
        }
        drop(guard);
        let _ = self.config_change_tx.send(ConfigChangeEvent {
            namespace: namespace.to_string(),
            id: id.to_string(),
            kind: ConfigChangeKind::Delete,
        });
        Ok(())
    }
}

// ── ConfigChangeNotifier ────────────────────────────────────────────

#[async_trait]
impl ConfigChangeNotifier for InMemoryStore {
    async fn subscribe(&self) -> Result<Box<dyn ConfigChangeSubscriber>, StorageError> {
        Ok(Box::new(InMemoryConfigChangeSubscriber {
            rx: self.config_change_tx.subscribe(),
        }))
    }
}

struct InMemoryConfigChangeSubscriber {
    rx: tokio::sync::broadcast::Receiver<ConfigChangeEvent>,
}

#[async_trait]
impl ConfigChangeSubscriber for InMemoryConfigChangeSubscriber {
    async fn next(&mut self) -> Result<ConfigChangeEvent, StorageError> {
        match self.rx.recv().await {
            Ok(event) => Ok(event),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(skipped, "in-memory config notifier lagged");
                Ok(ConfigChangeEvent {
                    namespace: String::new(),
                    id: String::new(),
                    kind: ConfigChangeKind::Put,
                })
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                Err(StorageError::Io("config change channel closed".into()))
            }
        }
    }
}

#[cfg(test)]
mod tests;
