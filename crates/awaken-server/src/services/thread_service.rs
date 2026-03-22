//! Thread CRUD operations.

use awaken_contract::contract::storage::{StorageError, ThreadStore};
use awaken_contract::thread::Thread;

/// Create a new thread.
pub async fn create_thread(
    store: &dyn ThreadStore,
    title: Option<String>,
) -> Result<Thread, StorageError> {
    let mut thread = Thread::new();
    if let Some(t) = title {
        thread.metadata.title = Some(t);
    }
    thread.metadata.created_at = Some(now_millis());
    thread.metadata.updated_at = Some(now_millis());
    store.save_thread(&thread).await?;
    Ok(thread)
}

/// Get a thread by ID.
pub async fn get_thread(
    store: &dyn ThreadStore,
    thread_id: &str,
) -> Result<Option<Thread>, StorageError> {
    store.load_thread(thread_id).await
}

/// List thread IDs with pagination.
pub async fn list_threads(
    store: &dyn ThreadStore,
    offset: usize,
    limit: usize,
) -> Result<Vec<String>, StorageError> {
    store.list_threads(offset, limit).await
}

/// Update thread title.
pub async fn update_thread_title(
    store: &dyn ThreadStore,
    thread_id: &str,
    title: String,
) -> Result<Thread, StorageError> {
    let mut thread = store
        .load_thread(thread_id)
        .await?
        .ok_or_else(|| StorageError::NotFound(thread_id.to_string()))?;
    thread.metadata.title = Some(title);
    thread.metadata.updated_at = Some(now_millis());
    store.save_thread(&thread).await?;
    Ok(thread)
}

fn now_millis() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::storage::ThreadStore;

    /// Simple in-memory thread store for testing.
    #[derive(Default)]
    struct MockThreadStore {
        threads: std::sync::RwLock<std::collections::HashMap<String, Thread>>,
        messages: std::sync::RwLock<
            std::collections::HashMap<String, Vec<awaken_contract::contract::message::Message>>,
        >,
    }

    #[async_trait::async_trait]
    impl ThreadStore for MockThreadStore {
        async fn load_thread(&self, thread_id: &str) -> Result<Option<Thread>, StorageError> {
            Ok(self.threads.read().unwrap().get(thread_id).cloned())
        }

        async fn save_thread(&self, thread: &Thread) -> Result<(), StorageError> {
            self.threads
                .write()
                .unwrap()
                .insert(thread.id.clone(), thread.clone());
            Ok(())
        }

        async fn list_threads(
            &self,
            offset: usize,
            limit: usize,
        ) -> Result<Vec<String>, StorageError> {
            let guard = self.threads.read().unwrap();
            let mut ids: Vec<String> = guard.keys().cloned().collect();
            ids.sort();
            Ok(ids.into_iter().skip(offset).take(limit).collect())
        }

        async fn load_messages(
            &self,
            thread_id: &str,
        ) -> Result<Option<Vec<awaken_contract::contract::message::Message>>, StorageError>
        {
            Ok(self.messages.read().unwrap().get(thread_id).cloned())
        }

        async fn save_messages(
            &self,
            thread_id: &str,
            messages: &[awaken_contract::contract::message::Message],
        ) -> Result<(), StorageError> {
            self.messages
                .write()
                .unwrap()
                .insert(thread_id.to_owned(), messages.to_vec());
            Ok(())
        }
    }

    #[tokio::test]
    async fn create_thread_assigns_uuid_v7_id() {
        let store = MockThreadStore::default();
        let thread = create_thread(&store, Some("Test".to_string()))
            .await
            .unwrap();
        assert_eq!(thread.id.len(), 36);
        assert_eq!(&thread.id[14..15], "7");
        assert_eq!(thread.metadata.title.as_deref(), Some("Test"));
        assert!(thread.metadata.created_at.is_some());
    }

    #[tokio::test]
    async fn create_thread_without_title() {
        let store = MockThreadStore::default();
        let thread = create_thread(&store, None).await.unwrap();
        assert!(thread.metadata.title.is_none());
    }

    #[tokio::test]
    async fn get_thread_returns_none_for_missing() {
        let store = MockThreadStore::default();
        let result = get_thread(&store, "missing").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn get_thread_returns_existing() {
        let store = MockThreadStore::default();
        let created = create_thread(&store, Some("T".to_string())).await.unwrap();
        let loaded = get_thread(&store, &created.id).await.unwrap().unwrap();
        assert_eq!(loaded.id, created.id);
        assert_eq!(loaded.metadata.title.as_deref(), Some("T"));
    }

    #[tokio::test]
    async fn list_threads_paginated() {
        let store = MockThreadStore::default();
        for _ in 0..5 {
            create_thread(&store, None).await.unwrap();
        }
        let page1 = list_threads(&store, 0, 3).await.unwrap();
        assert_eq!(page1.len(), 3);
        let page2 = list_threads(&store, 3, 3).await.unwrap();
        assert_eq!(page2.len(), 2);
    }

    #[tokio::test]
    async fn update_thread_title_works() {
        let store = MockThreadStore::default();
        let created = create_thread(&store, Some("Old".to_string()))
            .await
            .unwrap();
        let updated = update_thread_title(&store, &created.id, "New".to_string())
            .await
            .unwrap();
        assert_eq!(updated.metadata.title.as_deref(), Some("New"));
    }

    #[tokio::test]
    async fn update_thread_title_not_found() {
        let store = MockThreadStore::default();
        let result = update_thread_title(&store, "missing", "Title".to_string()).await;
        assert!(matches!(result, Err(StorageError::NotFound(_))));
    }
}
