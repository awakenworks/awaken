//! Async CRUD storage for namespaced JSON configuration documents.

use async_trait::async_trait;
use serde_json::Value;

use super::storage::StorageError;

/// Async CRUD store for namespaced JSON configuration documents.
#[async_trait]
pub trait ConfigStore: Send + Sync {
    /// Get a single entry by namespace and ID.
    async fn get(&self, namespace: &str, id: &str) -> Result<Option<Value>, StorageError>;

    /// List entries in a namespace ordered by ID.
    async fn list(
        &self,
        namespace: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<(String, Value)>, StorageError>;

    /// Create or overwrite an entry.
    async fn put(&self, namespace: &str, id: &str, value: &Value) -> Result<(), StorageError>;

    /// Delete an entry. Missing entries are not an error.
    async fn delete(&self, namespace: &str, id: &str) -> Result<(), StorageError>;

    /// Create an entry only when it does not already exist.
    ///
    /// Production stores should override this with an atomic implementation.
    /// The default implementation is best-effort and exists for lightweight
    /// test stores.
    async fn put_if_absent(
        &self,
        namespace: &str,
        id: &str,
        value: &Value,
    ) -> Result<(), StorageError> {
        if self.exists(namespace, id).await? {
            return Err(StorageError::AlreadyExists(format!("{namespace}/{id}")));
        }
        self.put(namespace, id, value).await
    }

    /// Check whether an entry exists.
    async fn exists(&self, namespace: &str, id: &str) -> Result<bool, StorageError> {
        Ok(self.get(namespace, id).await?.is_some())
    }

    /// Atomic compare-and-set on the record's `meta.revision`.
    ///
    /// Reads the existing record at `(namespace, id)`, extracts the integer
    /// at JSON path `meta.revision` (defaulting to 0 if absent), and:
    ///
    /// - if it equals `expected_revision`, replaces the record with `value`
    ///   and returns `Ok(())`.
    /// - if it differs, returns
    ///   `Err(StorageError::VersionConflict { expected, actual })`.
    /// - if no existing record AND `expected_revision == 0`, creates the
    ///   record (treats absence as revision 0).
    ///
    /// The new revision is the caller's responsibility: write `value` with
    /// `meta.revision = expected_revision + 1` before calling this method.
    /// The store does not modify the value it stores; it only enforces the
    /// CAS predicate.
    ///
    /// **Implementations MUST guarantee atomicity** with respect to
    /// concurrent `put` / `put_if_revision` / `delete` against the same
    /// `(namespace, id)`. The default implementation here is best-effort
    /// (read-then-put, racy across replicas) and is provided so existing
    /// `ConfigStore` impls compile without modification — production-grade
    /// stores must override.
    async fn put_if_revision(
        &self,
        namespace: &str,
        id: &str,
        value: &Value,
        expected_revision: u64,
    ) -> Result<(), StorageError> {
        let actual = self
            .get(namespace, id)
            .await?
            .as_ref()
            .and_then(extract_meta_revision)
            .unwrap_or(0);
        if actual != expected_revision {
            return Err(StorageError::VersionConflict {
                expected: expected_revision,
                actual,
            });
        }
        self.put(namespace, id, value).await
    }

    /// Delete only when the current record's `meta.revision` matches
    /// `expected_revision`.
    ///
    /// Production stores must make this atomic with concurrent writes to the
    /// same `(namespace, id)`. The default implementation is best-effort for
    /// simple test stores.
    async fn delete_if_revision(
        &self,
        namespace: &str,
        id: &str,
        expected_revision: u64,
    ) -> Result<(), StorageError> {
        let actual = self
            .get(namespace, id)
            .await?
            .as_ref()
            .and_then(extract_meta_revision)
            .unwrap_or(0);
        if actual != expected_revision {
            return Err(StorageError::VersionConflict {
                expected: expected_revision,
                actual,
            });
        }
        self.delete(namespace, id).await
    }
}

/// Extract the `meta.revision` integer from a stored config document.
///
/// Returns `None` if the path does not exist or is not an integer (e.g. legacy
/// bare-spec documents that predate the envelope format).
pub fn extract_meta_revision(value: &Value) -> Option<u64> {
    value
        .get("meta")
        .and_then(|m| m.get("revision"))
        .and_then(|r| r.as_u64())
}

/// Type of config mutation that was published by a store notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigChangeKind {
    Put,
    Delete,
}

/// A config change notification emitted by a store implementation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConfigChangeEvent {
    pub namespace: String,
    pub id: String,
    pub kind: ConfigChangeKind,
}

/// Blocking/streaming receiver for store-native config change notifications.
#[async_trait]
pub trait ConfigChangeSubscriber: Send {
    async fn next(&mut self) -> Result<ConfigChangeEvent, StorageError>;
}

/// Optional native notification capability for a [`ConfigStore`].
///
/// Stores that can push change events (for example PostgreSQL LISTEN/NOTIFY)
/// should implement this in addition to [`ConfigStore`]. Callers should still
/// keep a polling fallback because notifications may be delayed or unavailable.
#[async_trait]
pub trait ConfigChangeNotifier: Send + Sync {
    async fn subscribe(&self) -> Result<Box<dyn ConfigChangeSubscriber>, StorageError>;
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use tokio::sync::RwLock;

    use super::*;

    // ── extract_meta_revision ────────────────────────────────────────

    #[test]
    fn extract_meta_revision_returns_some_for_valid_path() {
        let value = serde_json::json!({"meta": {"revision": 42}});
        assert_eq!(extract_meta_revision(&value), Some(42));
    }

    #[test]
    fn extract_meta_revision_returns_none_for_missing_meta() {
        let value = serde_json::json!({"spec": {"id": "foo"}});
        assert_eq!(extract_meta_revision(&value), None);
    }

    #[test]
    fn extract_meta_revision_returns_none_for_missing_revision_key() {
        let value = serde_json::json!({"meta": {"source": {"kind": "user"}}});
        assert_eq!(extract_meta_revision(&value), None);
    }

    #[test]
    fn extract_meta_revision_returns_none_for_wrong_type() {
        let value = serde_json::json!({"meta": {"revision": "not-a-number"}});
        assert_eq!(extract_meta_revision(&value), None);
    }

    #[test]
    fn extract_meta_revision_returns_none_for_bare_spec() {
        // Legacy bare-spec shape (no envelope).
        let value = serde_json::json!({"id": "agent-1", "name": "Alice"});
        assert_eq!(extract_meta_revision(&value), None);
    }

    #[derive(Debug, Default)]
    struct MemoryConfigStore {
        data: RwLock<HashMap<String, HashMap<String, Value>>>,
    }

    #[async_trait]
    impl ConfigStore for MemoryConfigStore {
        async fn get(&self, namespace: &str, id: &str) -> Result<Option<Value>, StorageError> {
            let data = self.data.read().await;
            Ok(data.get(namespace).and_then(|ns| ns.get(id)).cloned())
        }

        async fn list(
            &self,
            namespace: &str,
            offset: usize,
            limit: usize,
        ) -> Result<Vec<(String, Value)>, StorageError> {
            let data = self.data.read().await;
            let Some(namespace_data) = data.get(namespace) else {
                return Ok(Vec::new());
            };
            let mut items: Vec<_> = namespace_data
                .iter()
                .map(|(id, value)| (id.clone(), value.clone()))
                .collect();
            items.sort_by(|left, right| left.0.cmp(&right.0));
            Ok(items.into_iter().skip(offset).take(limit).collect())
        }

        async fn put(&self, namespace: &str, id: &str, value: &Value) -> Result<(), StorageError> {
            let mut data = self.data.write().await;
            data.entry(namespace.to_string())
                .or_default()
                .insert(id.to_string(), value.clone());
            Ok(())
        }

        async fn delete(&self, namespace: &str, id: &str) -> Result<(), StorageError> {
            let mut data = self.data.write().await;
            if let Some(namespace_data) = data.get_mut(namespace) {
                namespace_data.remove(id);
            }
            Ok(())
        }
    }

    #[tokio::test]
    async fn config_store_round_trip() {
        let store: Arc<dyn ConfigStore> = Arc::new(MemoryConfigStore::default());
        let value = serde_json::json!({"id": "alpha", "label": "first"});

        store.put("tests", "alpha", &value).await.unwrap();

        assert_eq!(store.get("tests", "alpha").await.unwrap(), Some(value));
    }

    #[tokio::test]
    async fn config_store_lists_sorted_entries() {
        let store: Arc<dyn ConfigStore> = Arc::new(MemoryConfigStore::default());

        store
            .put(
                "tests",
                "bravo",
                &serde_json::json!({"id": "bravo", "label": "second"}),
            )
            .await
            .unwrap();
        store
            .put(
                "tests",
                "alpha",
                &serde_json::json!({"id": "alpha", "label": "first"}),
            )
            .await
            .unwrap();

        let items = store.list("tests", 0, 10).await.unwrap();
        assert_eq!(items[0].0, "alpha");
        assert_eq!(items[1].0, "bravo");
    }
}
