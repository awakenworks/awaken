use std::collections::HashMap;
use std::sync::RwLock;

use crate::contract::storage::{RunPage, RunQuery, RunStore, ThreadRunStore, ThreadStore};
use async_trait::async_trait;
use awaken_runtime_contract::contract::lifecycle::RunStatus;
use awaken_runtime_contract::contract::message::Message;
use awaken_runtime_contract::contract::storage::{RunRecord, StorageError};
use awaken_runtime_contract::thread::{Thread, ThreadMetadata};
use serde_json::{Value, json};

use crate::contract::config_store::{ConfigStore, ScopedConfigStore};
use crate::contract::storage::ScopedThreadRunStore;
use crate::contract::versioned_registry::{
    ConfigRevisionRef, PublishOutcome, RegistryPublication, RegistryResourcePublish,
    ScopedVersionedRegistry, TypedVersionedRegistry, VersionRef, VersionedRecord,
    VersionedRegistryError, VersionedRegistryStore, VersionedResourceState, registry_content_hash,
};
use crate::{RequestSurface, ScopeError, ScopeId};

#[test]
fn server_scope_types_are_reexported() {
    let scope = ScopeId::new("workspace-a").expect("valid scope");
    assert_eq!(scope.as_str(), "workspace-a");
    assert_eq!(RequestSurface::Admin, RequestSurface::Admin);
}

#[test]
fn server_store_traits_are_reexported() {
    fn assert_object_safe<T: ?Sized>() {}
    assert_object_safe::<dyn crate::contract::mailbox::MailboxStore>();
    assert_object_safe::<dyn crate::contract::protocol_replay_log::ProtocolReplayLog>();
}

#[derive(Default)]
struct MemoryConfigStore {
    data: tokio::sync::RwLock<HashMap<String, HashMap<String, Value>>>,
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
async fn scoped_config_store_isolates_same_namespace_and_id() {
    let inner: std::sync::Arc<dyn ConfigStore> = std::sync::Arc::new(MemoryConfigStore::default());
    let scope_a = ScopedConfigStore::new(inner.clone(), ScopeId::new("scope-a").unwrap());
    let scope_b = ScopedConfigStore::new(inner, ScopeId::new("scope-b").unwrap());

    scope_a
        .put("agents", "assistant", &json!({"scope": "a"}))
        .await
        .unwrap();
    scope_b
        .put("agents", "assistant", &json!({"scope": "b"}))
        .await
        .unwrap();

    assert_eq!(
        scope_a.get("agents", "assistant").await.unwrap(),
        Some(json!({"scope": "a"}))
    );
    assert_eq!(
        scope_b.get("agents", "assistant").await.unwrap(),
        Some(json!({"scope": "b"}))
    );
}

mod storage;
mod versioned_registry;
