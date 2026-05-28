//! Thread CRUD operations.

use std::collections::HashMap;

use awaken_server_contract::contract::storage::{
    ChildThreadDeleteStrategy, StorageError, ThreadStore,
};
use awaken_server_contract::thread::Thread;
use serde_json::Value;

/// Parameters for creating a thread.
#[derive(Debug, Clone, Default)]
pub struct CreateThreadOptions {
    pub title: Option<String>,
    pub resource_id: Option<String>,
    pub parent_thread_id: Option<String>,
}

/// Parameters for patching a thread.
#[derive(Debug, Clone, Default)]
pub struct UpdateThreadOptions {
    pub title: Option<String>,
    pub resource_id: Option<Option<String>>,
    pub parent_thread_id: Option<Option<String>>,
    pub custom: Option<HashMap<String, Value>>,
}

/// Parameters for deleting a thread.
#[derive(Debug, Clone, Copy, Default)]
pub struct DeleteThreadOptions {
    pub child_strategy: ChildThreadDeleteStrategy,
}

/// Create a new thread.
pub async fn create_thread(
    store: &dyn ThreadStore,
    title: Option<String>,
) -> Result<Thread, StorageError> {
    create_thread_with_options(
        store,
        CreateThreadOptions {
            title,
            ..CreateThreadOptions::default()
        },
    )
    .await
}

/// Create a new thread with lineage metadata.
pub async fn create_thread_with_options(
    store: &dyn ThreadStore,
    options: CreateThreadOptions,
) -> Result<Thread, StorageError> {
    let now = now_ms();
    let mut thread = Thread::new();
    if let Some(t) = options.title {
        thread.metadata.title = Some(t);
    }
    thread.resource_id = options.resource_id;
    thread.parent_thread_id = options.parent_thread_id;
    thread.normalize_lineage();
    thread.touch(now);
    store.save_thread_validated(&thread).await?;
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
    update_thread(
        store,
        thread_id,
        UpdateThreadOptions {
            title: Some(title),
            ..UpdateThreadOptions::default()
        },
    )
    .await
}

/// Update thread fields and validate hierarchy changes.
pub async fn update_thread(
    store: &dyn ThreadStore,
    thread_id: &str,
    options: UpdateThreadOptions,
) -> Result<Thread, StorageError> {
    let mut thread = store
        .load_thread(thread_id)
        .await?
        .ok_or_else(|| StorageError::NotFound(thread_id.to_string()))?;

    if let Some(title) = options.title {
        thread.metadata.title = Some(title);
    }
    if let Some(resource_id) = options.resource_id {
        thread.resource_id = resource_id;
    }
    if let Some(parent_thread_id) = options.parent_thread_id {
        thread.parent_thread_id = parent_thread_id;
    }
    thread.normalize_lineage();
    if let Some(custom) = options.custom {
        for (key, value) in custom {
            thread.metadata.custom.insert(key, value);
        }
    }
    thread.touch(now_ms());
    store.save_thread_validated(&thread).await?;
    Ok(thread)
}

/// Delete a thread while managing its child threads.
pub async fn delete_thread(
    store: &dyn ThreadStore,
    thread_id: &str,
    options: DeleteThreadOptions,
) -> Result<(), StorageError> {
    store
        .delete_thread_with_strategy(thread_id, options.child_strategy)
        .await
}

use awaken_server_contract::now_ms;

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_server_contract::contract::storage::ThreadStore;

    /// Simple in-memory thread store for testing.
    #[derive(Default)]
    struct MockThreadStore {
        threads: std::sync::RwLock<std::collections::HashMap<String, Thread>>,
        messages: std::sync::RwLock<
            std::collections::HashMap<
                String,
                Vec<awaken_server_contract::contract::message::Message>,
            >,
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

        async fn delete_thread(&self, thread_id: &str) -> Result<(), StorageError> {
            self.threads.write().unwrap().remove(thread_id);
            self.messages.write().unwrap().remove(thread_id);
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
        ) -> Result<Option<Vec<awaken_server_contract::contract::message::Message>>, StorageError>
        {
            Ok(self.messages.read().unwrap().get(thread_id).cloned())
        }

        async fn save_messages(
            &self,
            thread_id: &str,
            messages: &[awaken_server_contract::contract::message::Message],
        ) -> Result<(), StorageError> {
            self.messages
                .write()
                .unwrap()
                .insert(thread_id.to_owned(), messages.to_vec());
            Ok(())
        }

        async fn delete_messages(&self, thread_id: &str) -> Result<(), StorageError> {
            if !self.threads.read().unwrap().contains_key(thread_id) {
                return Err(StorageError::NotFound(thread_id.to_owned()));
            }
            self.messages.write().unwrap().remove(thread_id);
            Ok(())
        }

        async fn update_thread_metadata(
            &self,
            id: &str,
            metadata: awaken_server_contract::thread::ThreadMetadata,
        ) -> Result<(), StorageError> {
            let mut guard = self.threads.write().unwrap();
            let thread = guard
                .get_mut(id)
                .ok_or_else(|| StorageError::NotFound(id.to_owned()))?;
            thread.metadata = metadata;
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

    #[tokio::test]
    async fn create_thread_rejects_missing_parent_thread() {
        let store = MockThreadStore::default();

        let error = create_thread_with_options(
            &store,
            CreateThreadOptions {
                title: Some("Child".to_string()),
                resource_id: None,
                parent_thread_id: Some("missing-parent".to_string()),
            },
        )
        .await
        .expect_err("missing parent should fail");

        assert!(
            matches!(error, StorageError::Validation(message) if message == "parent thread not found: missing-parent")
        );
    }

    #[tokio::test]
    async fn create_thread_normalizes_lineage_fields() {
        let store = MockThreadStore::default();
        store.save_thread(&Thread::with_id("parent")).await.unwrap();

        let thread = create_thread_with_options(
            &store,
            CreateThreadOptions {
                title: Some("Child".to_string()),
                resource_id: Some(" resource-a ".to_string()),
                parent_thread_id: Some(" parent ".to_string()),
            },
        )
        .await
        .expect("create child");

        assert_eq!(thread.resource_id.as_deref(), Some("resource-a"));
        assert_eq!(thread.parent_thread_id.as_deref(), Some("parent"));
    }

    #[tokio::test]
    async fn update_thread_supports_detaching_parent() {
        let store = MockThreadStore::default();
        store
            .save_thread(&Thread::with_id("parent"))
            .await
            .expect("seed parent");
        let created = create_thread_with_options(
            &store,
            CreateThreadOptions {
                title: Some("Child".to_string()),
                resource_id: Some("resource-a".to_string()),
                parent_thread_id: Some("parent".to_string()),
            },
        )
        .await
        .expect("create child");

        let updated = update_thread(
            &store,
            &created.id,
            UpdateThreadOptions {
                resource_id: Some(None),
                parent_thread_id: Some(None),
                ..UpdateThreadOptions::default()
            },
        )
        .await
        .expect("detach child");

        assert_eq!(updated.resource_id, None);
        assert_eq!(updated.parent_thread_id, None);
    }

    #[tokio::test]
    async fn update_thread_normalizes_lineage_fields() {
        let store = MockThreadStore::default();
        let created = create_thread_with_options(
            &store,
            CreateThreadOptions {
                title: Some("Child".to_string()),
                resource_id: None,
                parent_thread_id: None,
            },
        )
        .await
        .expect("create child");

        let updated = update_thread(
            &store,
            &created.id,
            UpdateThreadOptions {
                resource_id: Some(Some(" resource-a ".to_string())),
                parent_thread_id: Some(Some("   ".to_string())),
                ..UpdateThreadOptions::default()
            },
        )
        .await
        .expect("normalize lineage");

        assert_eq!(updated.resource_id.as_deref(), Some("resource-a"));
        assert_eq!(updated.parent_thread_id, None);
    }

    #[tokio::test]
    async fn update_thread_rejects_cycle() {
        let store = MockThreadStore::default();
        store.save_thread(&Thread::with_id("root")).await.unwrap();
        store
            .save_thread(&Thread::with_id("child").with_parent_thread_id("root"))
            .await
            .unwrap();

        let error = update_thread(
            &store,
            "root",
            UpdateThreadOptions {
                parent_thread_id: Some(Some("child".to_string())),
                ..UpdateThreadOptions::default()
            },
        )
        .await
        .expect_err("cycle should be rejected");

        assert!(
            matches!(error, StorageError::Validation(message) if message.contains("cycle detected"))
        );
    }

    #[tokio::test]
    async fn delete_thread_default_detaches_children() {
        let store = MockThreadStore::default();
        store.save_thread(&Thread::with_id("root")).await.unwrap();
        store
            .save_thread(&Thread::with_id("child").with_parent_thread_id("root"))
            .await
            .unwrap();

        delete_thread(&store, "root", DeleteThreadOptions::default())
            .await
            .unwrap();

        assert!(store.load_thread("root").await.unwrap().is_none());
        assert_eq!(
            store
                .load_thread("child")
                .await
                .unwrap()
                .and_then(|thread| thread.parent_thread_id),
            None
        );
    }

    #[tokio::test]
    async fn delete_thread_supports_reject_strategy() {
        let store = MockThreadStore::default();
        store.save_thread(&Thread::with_id("root")).await.unwrap();
        store
            .save_thread(&Thread::with_id("child").with_parent_thread_id("root"))
            .await
            .unwrap();

        let error = delete_thread(
            &store,
            "root",
            DeleteThreadOptions {
                child_strategy: ChildThreadDeleteStrategy::Reject,
            },
        )
        .await
        .expect_err("reject strategy should fail");

        assert!(
            matches!(error, StorageError::Validation(message) if message.contains("child threads"))
        );
    }

    #[tokio::test]
    async fn delete_thread_supports_cascade_strategy() {
        let store = MockThreadStore::default();
        store.save_thread(&Thread::with_id("root")).await.unwrap();
        store
            .save_thread(&Thread::with_id("child").with_parent_thread_id("root"))
            .await
            .unwrap();
        store
            .save_thread(&Thread::with_id("grandchild").with_parent_thread_id("child"))
            .await
            .unwrap();

        delete_thread(
            &store,
            "root",
            DeleteThreadOptions {
                child_strategy: ChildThreadDeleteStrategy::Cascade,
            },
        )
        .await
        .unwrap();

        assert!(store.load_thread("root").await.unwrap().is_none());
        assert!(store.load_thread("child").await.unwrap().is_none());
        assert!(store.load_thread("grandchild").await.unwrap().is_none());
    }
}
