//! Resources application port for the MemoryStore identity/lifecycle aggregate.
//!
//! Memory content remains owned by `MemoryRepository`; this port prevents HTTP,
//! Dream, and other driving adapters from independently coordinating Catalog
//! writes, retention, and purge scheduling.

use std::collections::BTreeMap;

use async_trait::async_trait;

use crate::{
    MemoryStoreDefinition, MemoryStoreId, ResourceCatalogError, ResourcePurgeError, ResourceState,
    RetentionPolicy,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateMemoryStoreCommand {
    pub workspace_id: String,
    /// `None` asks the application to mint the public opaque identity. Internal
    /// workflows such as Dream use a deterministic idempotency identity.
    pub id: Option<MemoryStoreId>,
    pub name: String,
    pub description: String,
    pub metadata: BTreeMap<String, String>,
    pub initial_state: ResourceState,
    pub retention_policy: RetentionPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateMemoryStoreCommand {
    pub workspace_id: String,
    pub id: MemoryStoreId,
    /// `None` preserves; `Some(value)` replaces the human-readable name.
    pub name: Option<String>,
    /// `None` preserves; `Some("")` clears.
    pub description: Option<String>,
    /// String values upsert and `None` values delete.
    pub metadata_patch: BTreeMap<String, Option<String>>,
}

#[derive(Debug, thiserror::Error)]
pub enum MemoryStoreApplicationError {
    #[error(transparent)]
    Catalog(#[from] ResourceCatalogError),
    #[error(transparent)]
    Purge(#[from] ResourcePurgeError),
}

#[async_trait]
pub trait MemoryStoreApplicationService: Send + Sync {
    async fn create(
        &self,
        command: CreateMemoryStoreCommand,
    ) -> Result<MemoryStoreDefinition, MemoryStoreApplicationError>;

    async fn get(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Option<MemoryStoreDefinition>, MemoryStoreApplicationError>;

    async fn list(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<MemoryStoreDefinition>, MemoryStoreApplicationError>;

    async fn update(
        &self,
        command: UpdateMemoryStoreCommand,
    ) -> Result<MemoryStoreDefinition, MemoryStoreApplicationError>;

    async fn set_state(
        &self,
        workspace_id: &str,
        id: &str,
        state: ResourceState,
    ) -> Result<MemoryStoreDefinition, MemoryStoreApplicationError>;

    async fn delete(
        &self,
        workspace_id: &str,
        id: &str,
        requested_at_unix_ms: u64,
    ) -> Result<MemoryStoreDefinition, MemoryStoreApplicationError>;
}
