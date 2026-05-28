pub use awaken_runtime_contract::contract::config_store::*;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::contract::scope::ScopeId;
use crate::contract::storage::StorageError;

#[derive(Clone)]
pub struct ScopedConfigStore {
    inner: Arc<dyn ConfigStore>,
    scope_id: ScopeId,
}

impl ScopedConfigStore {
    pub fn new(inner: Arc<dyn ConfigStore>, scope_id: ScopeId) -> Self {
        Self { inner, scope_id }
    }

    pub fn scope_id(&self) -> &ScopeId {
        &self.scope_id
    }

    pub fn inner(&self) -> &dyn ConfigStore {
        self.inner.as_ref()
    }

    fn scoped_namespace(&self, namespace: &str) -> String {
        let scope = self.scope_id.as_str();
        format!("scope:{}:{}:{}", scope.len(), scope, namespace)
    }
}

#[async_trait]
impl ConfigStore for ScopedConfigStore {
    async fn get(&self, namespace: &str, id: &str) -> Result<Option<Value>, StorageError> {
        self.inner.get(&self.scoped_namespace(namespace), id).await
    }

    async fn list(
        &self,
        namespace: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<(String, Value)>, StorageError> {
        self.inner
            .list(&self.scoped_namespace(namespace), offset, limit)
            .await
    }

    async fn put(&self, namespace: &str, id: &str, value: &Value) -> Result<(), StorageError> {
        self.inner
            .put(&self.scoped_namespace(namespace), id, value)
            .await
    }

    async fn delete(&self, namespace: &str, id: &str) -> Result<(), StorageError> {
        self.inner
            .delete(&self.scoped_namespace(namespace), id)
            .await
    }

    async fn put_if_absent(
        &self,
        namespace: &str,
        id: &str,
        value: &Value,
    ) -> Result<(), StorageError> {
        self.inner
            .put_if_absent(&self.scoped_namespace(namespace), id, value)
            .await
    }

    async fn exists(&self, namespace: &str, id: &str) -> Result<bool, StorageError> {
        self.inner
            .exists(&self.scoped_namespace(namespace), id)
            .await
    }

    async fn put_if_revision(
        &self,
        namespace: &str,
        id: &str,
        value: &Value,
        expected_revision: u64,
    ) -> Result<(), StorageError> {
        self.inner
            .put_if_revision(
                &self.scoped_namespace(namespace),
                id,
                value,
                expected_revision,
            )
            .await
    }

    async fn delete_if_revision(
        &self,
        namespace: &str,
        id: &str,
        expected_revision: u64,
    ) -> Result<(), StorageError> {
        self.inner
            .delete_if_revision(&self.scoped_namespace(namespace), id, expected_revision)
            .await
    }
}
