//! In-memory storage backend for testing and local development.

use std::collections::HashMap;

use async_trait::async_trait;
use awaken_contract::contract::config_store::{
    ConfigChangeEvent, ConfigChangeKind, ConfigChangeNotifier, ConfigChangeSubscriber, ConfigStore,
    extract_meta_revision,
};
use awaken_contract::contract::message::Message;
use awaken_contract::contract::profile_store::{ProfileEntry, ProfileOwner, ProfileStore};
use awaken_contract::contract::storage::{
    MessagePage, MessageQuery, RunPage, RunQuery, RunRecord, RunStore, StorageError, ThreadPage,
    ThreadQuery, ThreadRunStore, ThreadStore, checkpoint_parent_thread_id,
    paginate_message_records, paginate_threads,
};
use awaken_contract::thread::{Thread, normalize_lineage_id};
use serde_json::Value;
use tokio::sync::RwLock;

/// In-memory storage implementing all four store traits.
///
/// Uses `tokio::sync::RwLock` for async-safe concurrent access.
/// Data lives only in memory and is lost when the store is dropped.
#[derive(Debug)]
pub struct InMemoryStore {
    threads: RwLock<HashMap<String, Thread>>,
    runs: RwLock<HashMap<String, RunRecord>>,
    /// Thread ID -> ordered messages (single source of truth).
    messages: RwLock<HashMap<String, Vec<Message>>>,
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
            messages: RwLock::new(HashMap::new()),
            profiles: RwLock::new(HashMap::new()),
            configs: RwLock::new(HashMap::new()),
            config_change_tx,
        }
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
        threads.remove(thread_id);
        messages.remove(thread_id);
        Ok(())
    }

    async fn delete_thread_with_strategy(
        &self,
        thread_id: &str,
        strategy: awaken_contract::contract::storage::ChildThreadDeleteStrategy,
    ) -> Result<(), StorageError> {
        let mut threads = self.threads.write().await;
        let mut messages = self.messages.write().await;
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
        Ok(())
    }

    async fn load_run(&self, run_id: &str) -> Result<Option<RunRecord>, StorageError> {
        let guard = self.runs.read().await;
        Ok(guard.get(run_id).cloned())
    }

    async fn latest_run(&self, thread_id: &str) -> Result<Option<RunRecord>, StorageError> {
        let guard = self.runs.read().await;
        Ok(guard
            .values()
            .filter(|r| r.thread_id == thread_id)
            .max_by_key(|r| r.updated_at)
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
        Ok(())
    }
}

// ── ProfileStore ────────────────────────────────────────────────────

fn current_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_millis() as u64
}

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
mod tests {
    use super::*;
    use awaken_contract::contract::lifecycle::RunStatus;
    use awaken_contract::contract::message::Message;
    use awaken_contract::contract::storage::{
        RunQuery, RunRecord, RunStore, ThreadRunStore, ThreadStore,
    };
    use awaken_contract::thread::Thread;
    use std::sync::Arc;
    use tokio::sync::Barrier;

    fn make_run(run_id: &str, thread_id: &str, status: RunStatus) -> RunRecord {
        RunRecord {
            run_id: run_id.to_string(),
            thread_id: thread_id.to_string(),
            agent_id: "agent".to_string(),
            parent_run_id: None,
            request: None,
            input: None,
            output: None,
            status,
            termination_reason: None,
            final_output: None,
            error_payload: None,
            dispatch_id: None,
            session_id: None,
            transport_request_id: None,
            waiting: None,
            outcome: None,
            created_at: 100,
            started_at: None,
            finished_at: None,
            updated_at: 100,
            steps: 0,
            input_tokens: 0,
            output_tokens: 0,
            state: None,
        }
    }

    // ── ThreadStore ──

    #[tokio::test]
    async fn thread_save_and_load() {
        let store = InMemoryStore::new();
        let thread = Thread::new();
        store.save_thread(&thread).await.unwrap();
        let loaded = store.load_thread(&thread.id).await.unwrap().unwrap();
        assert_eq!(loaded.id, thread.id);
    }

    #[tokio::test]
    async fn thread_load_missing_returns_none() {
        let store = InMemoryStore::new();
        assert!(store.load_thread("no-such").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn thread_delete_removes_thread_and_messages() {
        let store = InMemoryStore::new();
        let thread = Thread::new();
        store.save_thread(&thread).await.unwrap();
        store
            .save_messages(&thread.id, &[Message::user("hello")])
            .await
            .unwrap();

        store.delete_thread(&thread.id).await.unwrap();
        assert!(store.load_thread(&thread.id).await.unwrap().is_none());
        assert!(store.load_messages(&thread.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn thread_list_with_pagination() {
        let store = InMemoryStore::new();
        for i in 0..5 {
            let mut t = Thread::new();
            t.id = format!("t-{i:02}");
            store.save_thread(&t).await.unwrap();
        }
        let page = store.list_threads(1, 2).await.unwrap();
        assert_eq!(page.len(), 2);
    }

    #[tokio::test]
    async fn save_thread_validated_serializes_concurrent_cycle_updates() {
        let store = Arc::new(InMemoryStore::new());
        store.save_thread(&Thread::with_id("a")).await.unwrap();
        store.save_thread(&Thread::with_id("b")).await.unwrap();

        let read_guard = store.threads.read().await;
        let barrier = Arc::new(Barrier::new(3));
        let spawn_update = |thread_id: &'static str, parent_thread_id: &'static str| {
            let store = store.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                store
                    .save_thread_validated(
                        &Thread::with_id(thread_id).with_parent_thread_id(parent_thread_id),
                    )
                    .await
            })
        };

        let left = spawn_update("a", "b");
        let right = spawn_update("b", "a");
        barrier.wait().await;
        tokio::task::yield_now().await;
        drop(read_guard);

        let left = left.await.unwrap();
        let right = right.await.unwrap();
        assert_ne!(left.is_ok(), right.is_ok());

        let a = store.load_thread("a").await.unwrap().unwrap();
        let b = store.load_thread("b").await.unwrap().unwrap();
        assert!(
            !(a.parent_thread_id.as_deref() == Some("b")
                && b.parent_thread_id.as_deref() == Some("a"))
        );
    }

    #[tokio::test]
    async fn messages_save_and_load() {
        let store = InMemoryStore::new();
        let msgs = vec![Message::user("hi"), Message::assistant("hello")];
        store.save_messages("t-1", &msgs).await.unwrap();
        let loaded = store.load_messages("t-1").await.unwrap().unwrap();
        assert_eq!(loaded.len(), 2);
    }

    #[tokio::test]
    async fn messages_load_missing_returns_none() {
        let store = InMemoryStore::new();
        assert!(store.load_messages("no-such").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_messages_requires_existing_thread() {
        let store = InMemoryStore::new();
        let err = store.delete_messages("no-such").await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    #[tokio::test]
    async fn delete_messages_for_existing_thread() {
        let store = InMemoryStore::new();
        let thread = Thread::new();
        store.save_thread(&thread).await.unwrap();
        store
            .save_messages(&thread.id, &[Message::user("hi")])
            .await
            .unwrap();

        store.delete_messages(&thread.id).await.unwrap();
        assert!(store.load_messages(&thread.id).await.unwrap().is_none());
    }

    // ── RunStore ──

    #[tokio::test]
    async fn run_create_and_load() {
        let store = InMemoryStore::new();
        let run = make_run("r-1", "t-1", RunStatus::Running);
        store.create_run(&run).await.unwrap();
        let loaded = store.load_run("r-1").await.unwrap().unwrap();
        assert_eq!(loaded.thread_id, "t-1");
    }

    #[tokio::test]
    async fn run_create_duplicate_returns_already_exists() {
        let store = InMemoryStore::new();
        let run = make_run("r-1", "t-1", RunStatus::Running);
        store.create_run(&run).await.unwrap();
        let err = store.create_run(&run).await.unwrap_err();
        assert!(matches!(err, StorageError::AlreadyExists(_)));
    }

    #[tokio::test]
    async fn run_load_missing_returns_none() {
        let store = InMemoryStore::new();
        assert!(store.load_run("no-such").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn run_latest_returns_most_recently_updated() {
        let store = InMemoryStore::new();
        let mut run1 = make_run("r-1", "t-1", RunStatus::Running);
        run1.updated_at = 100;
        let mut run2 = make_run("r-2", "t-1", RunStatus::Done);
        run2.updated_at = 200;
        store.create_run(&run1).await.unwrap();
        store.create_run(&run2).await.unwrap();

        let latest = store.latest_run("t-1").await.unwrap().unwrap();
        assert_eq!(latest.run_id, "r-2");
    }

    #[tokio::test]
    async fn run_list_filters_by_thread_and_status() {
        let store = InMemoryStore::new();
        store
            .create_run(&make_run("r-1", "t-1", RunStatus::Running))
            .await
            .unwrap();
        store
            .create_run(&make_run("r-2", "t-1", RunStatus::Done))
            .await
            .unwrap();
        store
            .create_run(&make_run("r-3", "t-2", RunStatus::Running))
            .await
            .unwrap();

        let query = RunQuery {
            thread_id: Some("t-1".to_string()),
            status: Some(RunStatus::Running),
            offset: 0,
            limit: 100,
        };
        let page = store.list_runs(&query).await.unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].run_id, "r-1");
    }

    // ── Concurrent mutations ──

    #[tokio::test]
    async fn concurrent_thread_mutations_are_safe() {
        let store = std::sync::Arc::new(InMemoryStore::new());
        let mut handles = Vec::new();
        for i in 0..10 {
            let s = store.clone();
            handles.push(tokio::spawn(async move {
                let mut t = Thread::new();
                t.id = format!("concurrent-{i}");
                s.save_thread(&t).await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let threads = store.list_threads(0, 100).await.unwrap();
        assert_eq!(threads.len(), 10);
    }

    #[tokio::test]
    async fn concurrent_run_mutations_are_safe() {
        let store = std::sync::Arc::new(InMemoryStore::new());
        let mut handles = Vec::new();
        for i in 0..10 {
            let s = store.clone();
            handles.push(tokio::spawn(async move {
                let run = make_run(&format!("r-{i}"), "t-1", RunStatus::Running);
                s.create_run(&run).await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let page = store
            .list_runs(&RunQuery {
                thread_id: None,
                status: None,
                offset: 0,
                limit: 200,
            })
            .await
            .unwrap();
        assert_eq!(page.items.len(), 10);
    }

    // ── Checkpoint atomicity ──

    #[tokio::test]
    async fn checkpoint_saves_messages_and_run_together() {
        let store = InMemoryStore::new();
        let msgs = vec![Message::user("checkpoint")];
        let run = make_run("r-cp", "t-1", RunStatus::Running);

        store.checkpoint("t-1", &msgs, &run).await.unwrap();

        let loaded_msgs = store.load_messages("t-1").await.unwrap().unwrap();
        assert_eq!(loaded_msgs.len(), 1);
        let loaded_run = store.load_run("r-cp").await.unwrap().unwrap();
        assert_eq!(loaded_run.thread_id, "t-1");
    }

    // ── Large payload ──

    #[tokio::test]
    async fn large_payload_handling() {
        let store = InMemoryStore::new();
        let large_text = "x".repeat(1_000_000);
        let msgs = vec![Message::user(&large_text)];
        store.save_messages("t-large", &msgs).await.unwrap();
        let loaded = store.load_messages("t-large").await.unwrap().unwrap();
        assert_eq!(loaded.len(), 1);
    }

    // ── Update thread metadata ──

    #[tokio::test]
    async fn update_thread_metadata_on_missing_thread_returns_not_found() {
        let store = InMemoryStore::new();
        let err = store
            .update_thread_metadata("no-such", Default::default())
            .await
            .unwrap_err();
        assert!(matches!(err, StorageError::NotFound(_)));
    }

    #[tokio::test]
    async fn update_thread_metadata_success() {
        let store = InMemoryStore::new();
        let thread = Thread::new();
        store.save_thread(&thread).await.unwrap();

        let meta = awaken_contract::thread::ThreadMetadata {
            title: Some("Updated".to_string()),
            ..Default::default()
        };
        store
            .update_thread_metadata(&thread.id, meta)
            .await
            .unwrap();

        let loaded = store.load_thread(&thread.id).await.unwrap().unwrap();
        assert_eq!(loaded.metadata.title.as_deref(), Some("Updated"));
    }

    // ── ProfileStore ──

    #[tokio::test]
    async fn profile_set_and_get() {
        let store = InMemoryStore::new();
        let owner = ProfileOwner::Agent("alice".into());
        store
            .set(&owner, "lang", serde_json::json!("en"))
            .await
            .unwrap();
        let entry = ProfileStore::get(&store, &owner, "lang")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entry.key, "lang");
        assert_eq!(entry.value, serde_json::json!("en"));
        assert!(entry.updated_at > 0);
    }

    #[tokio::test]
    async fn profile_get_missing() {
        let store = InMemoryStore::new();
        let result = ProfileStore::get(&store, &ProfileOwner::System, "nonexistent")
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn profile_upsert_overwrites() {
        let store = InMemoryStore::new();
        let owner = ProfileOwner::System;
        store.set(&owner, "k", serde_json::json!(1)).await.unwrap();
        store.set(&owner, "k", serde_json::json!(2)).await.unwrap();
        let entry = ProfileStore::get(&store, &owner, "k")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entry.value, serde_json::json!(2));
    }

    #[tokio::test]
    async fn profile_delete_idempotent() {
        let store = InMemoryStore::new();
        let owner = ProfileOwner::Agent("bob".into());
        // Delete non-existent key is fine
        ProfileStore::delete(&store, &owner, "missing")
            .await
            .unwrap();
        // Set then delete
        store.set(&owner, "k", serde_json::json!(1)).await.unwrap();
        ProfileStore::delete(&store, &owner, "k").await.unwrap();
        assert!(
            ProfileStore::get(&store, &owner, "k")
                .await
                .unwrap()
                .is_none()
        );
        // Delete again is fine
        ProfileStore::delete(&store, &owner, "k").await.unwrap();
    }

    #[tokio::test]
    async fn profile_list_sorted_and_isolated() {
        let store = InMemoryStore::new();
        let alice = ProfileOwner::Agent("alice".into());
        let bob = ProfileOwner::Agent("bob".into());
        store
            .set(&alice, "b", serde_json::json!("second"))
            .await
            .unwrap();
        store
            .set(&alice, "a", serde_json::json!("first"))
            .await
            .unwrap();
        store
            .set(&bob, "x", serde_json::json!("other"))
            .await
            .unwrap();

        let entries = ProfileStore::list(&store, &alice).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].key, "a");
        assert_eq!(entries[1].key, "b");

        // Bob's entries are isolated
        let bob_entries = ProfileStore::list(&store, &bob).await.unwrap();
        assert_eq!(bob_entries.len(), 1);
        assert_eq!(bob_entries[0].key, "x");
    }

    #[tokio::test]
    async fn profile_clear_owner() {
        let store = InMemoryStore::new();
        let alice = ProfileOwner::Agent("alice".into());
        let bob = ProfileOwner::Agent("bob".into());
        store.set(&alice, "a", serde_json::json!(1)).await.unwrap();
        store.set(&alice, "b", serde_json::json!(2)).await.unwrap();
        store.set(&bob, "c", serde_json::json!(3)).await.unwrap();

        store.clear_owner(&alice).await.unwrap();
        assert!(ProfileStore::list(&store, &alice).await.unwrap().is_empty());
        assert_eq!(ProfileStore::list(&store, &bob).await.unwrap().len(), 1);

        // Clear again is idempotent
        store.clear_owner(&alice).await.unwrap();
    }

    // ── ConfigChangeNotifier ──

    // ── ConfigStore::put_if_revision ──

    #[tokio::test]
    async fn put_if_revision_succeeds_when_revision_matches() {
        use awaken_contract::contract::config_store::ConfigStore;
        let store = InMemoryStore::new();

        // First write: no existing record, expected=0 → insert at revision 1.
        let value_r1 = serde_json::json!({"spec": {"id": "a"}, "meta": {"source": {"kind": "user"}, "revision": 1}});
        store
            .put_if_revision("ns", "a", &value_r1, 0)
            .await
            .unwrap();
        let stored = ConfigStore::get(&store, "ns", "a").await.unwrap().unwrap();
        assert_eq!(stored["meta"]["revision"], 1);

        // Second write: expected=1 → update to revision 2.
        let value_r2 = serde_json::json!({"spec": {"id": "a"}, "meta": {"source": {"kind": "user"}, "revision": 2}});
        store
            .put_if_revision("ns", "a", &value_r2, 1)
            .await
            .unwrap();
        let stored = ConfigStore::get(&store, "ns", "a").await.unwrap().unwrap();
        assert_eq!(stored["meta"]["revision"], 2);
    }

    #[tokio::test]
    async fn put_if_revision_returns_conflict_on_mismatch() {
        use awaken_contract::contract::storage::StorageError;
        let store = InMemoryStore::new();

        // Insert a record at revision 1.
        let value_r1 =
            serde_json::json!({"spec": {}, "meta": {"source": {"kind": "user"}, "revision": 1}});
        store.put("ns", "b", &value_r1).await.unwrap();

        // Try with wrong expected revision.
        let err = store
            .put_if_revision("ns", "b", &value_r1, 5)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            StorageError::VersionConflict {
                expected: 5,
                actual: 1
            }
        ));
    }

    #[tokio::test]
    async fn put_if_absent_inserts_once_and_reports_existing() {
        use awaken_contract::contract::config_store::ConfigStore;
        use awaken_contract::contract::storage::StorageError;

        let store = InMemoryStore::new();
        let value = serde_json::json!({
            "spec": {"id": "new"},
            "meta": {"source": {"kind": "user"}, "revision": 1}
        });

        store.put_if_absent("ns", "new", &value).await.unwrap();

        let err = store.put_if_absent("ns", "new", &value).await.unwrap_err();
        assert!(matches!(err, StorageError::AlreadyExists(id) if id == "ns/new"));

        let stored = ConfigStore::get(&store, "ns", "new")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored, value);
    }

    #[tokio::test]
    async fn delete_if_revision_removes_only_matching_revision() {
        use awaken_contract::contract::config_store::ConfigStore;
        use awaken_contract::contract::storage::StorageError;

        let store = InMemoryStore::new();
        let value = serde_json::json!({
            "spec": {"id": "delete-me"},
            "meta": {"source": {"kind": "user"}, "revision": 3}
        });
        store.put("ns", "delete-me", &value).await.unwrap();

        let err = store
            .delete_if_revision("ns", "delete-me", 2)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            StorageError::VersionConflict {
                expected: 2,
                actual: 3
            }
        ));
        assert!(
            ConfigStore::get(&store, "ns", "delete-me")
                .await
                .unwrap()
                .is_some()
        );

        store
            .delete_if_revision("ns", "delete-me", 3)
            .await
            .unwrap();
        assert!(
            ConfigStore::get(&store, "ns", "delete-me")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn put_if_revision_handles_concurrent_writers() {
        use awaken_contract::contract::config_store::ConfigStore;
        use awaken_contract::contract::storage::StorageError;
        use std::sync::Arc;

        let store = Arc::new(InMemoryStore::new());

        // Seed with revision 0 (absence treated as 0).
        const N: usize = 20;
        let mut handles = Vec::with_capacity(N);
        for _ in 0..N {
            let s = store.clone();
            handles.push(tokio::spawn(async move {
                let value = serde_json::json!({"spec": {}, "meta": {"source": {"kind": "user"}, "revision": 1}});
                s.put_if_revision("ns", "concurrent", &value, 0).await
            }));
        }

        let results: Vec<_> = futures::future::join_all(handles)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        let successes = results.iter().filter(|r| r.is_ok()).count();
        let conflicts = results
            .iter()
            .filter(|r| matches!(r, Err(StorageError::VersionConflict { expected: 0, .. })))
            .count();

        assert_eq!(successes, 1, "exactly one writer should succeed");
        assert_eq!(conflicts, N - 1, "all others should get VersionConflict");

        let stored = ConfigStore::get(store.as_ref(), "ns", "concurrent")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored["meta"]["revision"], 1);
    }

    #[tokio::test]
    async fn config_change_notifier_emits_on_put_and_delete() {
        use awaken_contract::contract::config_store::{
            ConfigChangeKind, ConfigChangeNotifier, ConfigStore,
        };
        let store = InMemoryStore::new();
        let mut sub = store.subscribe().await.unwrap();

        store
            .put("agents", "a1", &serde_json::json!({"hello": "world"}))
            .await
            .unwrap();
        let event = sub.next().await.unwrap();
        assert_eq!(event.namespace, "agents");
        assert_eq!(event.id, "a1");
        assert!(matches!(event.kind, ConfigChangeKind::Put));

        ConfigStore::delete(&store, "agents", "a1").await.unwrap();
        let event = sub.next().await.unwrap();
        assert_eq!(event.namespace, "agents");
        assert_eq!(event.id, "a1");
        assert!(matches!(event.kind, ConfigChangeKind::Delete));
    }

    #[tokio::test]
    async fn config_change_notifier_supports_multiple_subscribers() {
        use awaken_contract::contract::config_store::ConfigChangeNotifier;
        let store = InMemoryStore::new();
        let mut sub_a = store.subscribe().await.unwrap();
        let mut sub_b = store.subscribe().await.unwrap();

        store
            .put("tools", "echo", &serde_json::json!({}))
            .await
            .unwrap();

        let a = sub_a.next().await.unwrap();
        let b = sub_b.next().await.unwrap();
        assert_eq!(a.namespace, "tools");
        assert_eq!(b.namespace, "tools");
        assert_eq!(a.id, "echo");
        assert_eq!(b.id, "echo");
    }
}
