//! Async CRUD storage for namespaced JSON configuration documents.

use std::marker::PhantomData;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use serde::de::DeserializeOwned;
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

    /// Check whether an entry exists.
    async fn exists(&self, namespace: &str, id: &str) -> Result<bool, StorageError> {
        Ok(self.get(namespace, id).await?.is_some())
    }
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

/// Typed namespace descriptor for a configuration entity.
pub trait ConfigNamespace: 'static + Send + Sync {
    /// Storage namespace (for example `"agents"` or `"models"`).
    const NAMESPACE: &'static str;

    /// Typed value stored in this namespace.
    type Value: Serialize + DeserializeOwned + Send + Sync + 'static;

    /// Extract the stable ID from a typed value.
    fn id(value: &Self::Value) -> &str;
}

/// Typed wrapper over [`ConfigStore`] for a specific namespace.
pub struct ConfigRegistry<N: ConfigNamespace> {
    store: Arc<dyn ConfigStore>,
    _namespace: PhantomData<N>,
}

impl<N: ConfigNamespace> ConfigRegistry<N> {
    /// Create a typed registry backed by the given raw store.
    pub fn new(store: Arc<dyn ConfigStore>) -> Self {
        Self {
            store,
            _namespace: PhantomData,
        }
    }

    /// Get one typed entry by ID.
    pub async fn get(&self, id: &str) -> Result<Option<N::Value>, StorageError> {
        let Some(value) = self.store.get(N::NAMESPACE, id).await? else {
            return Ok(None);
        };
        let typed = serde_json::from_value(value)
            .map_err(|error| StorageError::Serialization(error.to_string()))?;
        Ok(Some(typed))
    }

    /// List typed entries in the namespace.
    pub async fn list(&self, offset: usize, limit: usize) -> Result<Vec<N::Value>, StorageError> {
        let values = self.store.list(N::NAMESPACE, offset, limit).await?;
        values
            .into_iter()
            .map(|(_, value)| {
                serde_json::from_value(value)
                    .map_err(|error| StorageError::Serialization(error.to_string()))
            })
            .collect()
    }

    /// Upsert one typed entry.
    pub async fn put(&self, value: &N::Value) -> Result<(), StorageError> {
        let id = N::id(value);
        let json = serde_json::to_value(value)
            .map_err(|error| StorageError::Serialization(error.to_string()))?;
        self.store.put(N::NAMESPACE, id, &json).await
    }

    /// Delete one typed entry by ID.
    pub async fn delete(&self, id: &str) -> Result<(), StorageError> {
        self.store.delete(N::NAMESPACE, id).await
    }

    /// Check whether an entry exists.
    pub async fn exists(&self, id: &str) -> Result<bool, StorageError> {
        self.store.exists(N::NAMESPACE, id).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tokio::sync::RwLock;

    use super::*;

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

    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    struct TestEntity {
        id: String,
        label: String,
    }

    struct TestNamespace;

    impl ConfigNamespace for TestNamespace {
        const NAMESPACE: &'static str = "tests";
        type Value = TestEntity;

        fn id(value: &Self::Value) -> &str {
            &value.id
        }
    }

    #[tokio::test]
    async fn typed_registry_round_trip() {
        let store = Arc::new(MemoryConfigStore::default());
        let registry = ConfigRegistry::<TestNamespace>::new(store);
        let entity = TestEntity {
            id: "alpha".into(),
            label: "first".into(),
        };

        registry.put(&entity).await.unwrap();

        assert_eq!(registry.get("alpha").await.unwrap(), Some(entity));
    }

    #[tokio::test]
    async fn typed_registry_lists_sorted_entries() {
        let store = Arc::new(MemoryConfigStore::default());
        let registry = ConfigRegistry::<TestNamespace>::new(store);

        registry
            .put(&TestEntity {
                id: "bravo".into(),
                label: "second".into(),
            })
            .await
            .unwrap();
        registry
            .put(&TestEntity {
                id: "alpha".into(),
                label: "first".into(),
            })
            .await
            .unwrap();

        let items = registry.list(0, 10).await.unwrap();
        assert_eq!(items[0].id, "alpha");
        assert_eq!(items[1].id, "bravo");
    }
}
