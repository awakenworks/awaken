//! In-memory storage backend for testing and local development.

use std::collections::HashMap;

use async_trait::async_trait;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::profile_store::{ProfileEntry, ProfileOwner, ProfileStore};
use awaken_contract::contract::storage::{
    RunPage, RunQuery, RunRecord, RunStore, StorageError, ThreadRunStore, ThreadStore,
};
use awaken_contract::thread::Thread;
use serde_json::Value;
use tokio::sync::RwLock;

/// In-memory storage implementing all four store traits.
///
/// Uses `tokio::sync::RwLock` for async-safe concurrent access.
/// Data lives only in memory and is lost when the store is dropped.
#[derive(Debug, Default)]
pub struct InMemoryStore {
    threads: RwLock<HashMap<String, Thread>>,
    runs: RwLock<HashMap<String, RunRecord>>,
    /// Thread ID -> ordered messages (single source of truth).
    messages: RwLock<HashMap<String, Vec<Message>>>,
    /// Profile entries keyed by (owner, key).
    profiles: RwLock<HashMap<ProfileOwner, HashMap<String, ProfileEntry>>>,
}

impl InMemoryStore {
    /// Create a new empty in-memory store.
    pub fn new() -> Self {
        Self::default()
    }
}

// ── ThreadStore ─────────────────────────────────────────────────────

#[async_trait]
impl ThreadStore for InMemoryStore {
    async fn load_thread(&self, thread_id: &str) -> Result<Option<Thread>, StorageError> {
        let guard = self.threads.read().await;
        Ok(guard.get(thread_id).cloned())
    }

    async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError> {
        let mut guard = self.threads.write().await;
        guard.insert(thread.id.clone(), thread.clone());
        Ok(())
    }

    async fn delete_thread(&self, thread_id: &str) -> Result<(), StorageError> {
        let mut threads = self.threads.write().await;
        let mut messages = self.messages.write().await;
        threads.remove(thread_id);
        messages.remove(thread_id);
        Ok(())
    }

    async fn list_threads(&self, offset: usize, limit: usize) -> Result<Vec<String>, StorageError> {
        let guard = self.threads.read().await;
        let mut ids: Vec<String> = guard.keys().cloned().collect();
        ids.sort();
        Ok(ids.into_iter().skip(offset).take(limit).collect())
    }

    async fn load_messages(&self, thread_id: &str) -> Result<Option<Vec<Message>>, StorageError> {
        let guard = self.messages.read().await;
        Ok(guard.get(thread_id).cloned())
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
        let mut msg_guard = self.messages.write().await;
        let mut run_guard = self.runs.write().await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::lifecycle::RunStatus;
    use awaken_contract::contract::message::Message;
    use awaken_contract::contract::storage::{
        RunQuery, RunRecord, RunStore, ThreadRunStore, ThreadStore,
    };
    use awaken_contract::thread::Thread;

    fn make_run(run_id: &str, thread_id: &str, status: RunStatus) -> RunRecord {
        RunRecord {
            run_id: run_id.to_string(),
            thread_id: thread_id.to_string(),
            agent_id: "agent".to_string(),
            parent_run_id: None,
            status,
            termination_code: None,
            created_at: 100,
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

        let mut meta = awaken_contract::thread::ThreadMetadata::default();
        meta.title = Some("Updated".to_string());
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
        let entry = store.get(&owner, "lang").await.unwrap().unwrap();
        assert_eq!(entry.key, "lang");
        assert_eq!(entry.value, serde_json::json!("en"));
        assert!(entry.updated_at > 0);
    }

    #[tokio::test]
    async fn profile_get_missing() {
        let store = InMemoryStore::new();
        let result = store
            .get(&ProfileOwner::System, "nonexistent")
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
        let entry = store.get(&owner, "k").await.unwrap().unwrap();
        assert_eq!(entry.value, serde_json::json!(2));
    }

    #[tokio::test]
    async fn profile_delete_idempotent() {
        let store = InMemoryStore::new();
        let owner = ProfileOwner::Agent("bob".into());
        // Delete non-existent key is fine
        store.delete(&owner, "missing").await.unwrap();
        // Set then delete
        store.set(&owner, "k", serde_json::json!(1)).await.unwrap();
        store.delete(&owner, "k").await.unwrap();
        assert!(store.get(&owner, "k").await.unwrap().is_none());
        // Delete again is fine
        store.delete(&owner, "k").await.unwrap();
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

        let entries = store.list(&alice).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].key, "a");
        assert_eq!(entries[1].key, "b");

        // Bob's entries are isolated
        let bob_entries = store.list(&bob).await.unwrap();
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
        assert!(store.list(&alice).await.unwrap().is_empty());
        assert_eq!(store.list(&bob).await.unwrap().len(), 1);

        // Clear again is idempotent
        store.clear_owner(&alice).await.unwrap();
    }
}
