//! Resource Catalog domain vocabulary and port.
//!
//! This module owns resource identity, immutable configuration versions, and live
//! lifecycle state. It intentionally contains no principal, API key, role, policy,
//! Org, Project, or WorkUnit type: a PEP authorizes first and then invokes this port
//! with a trusted Workspace.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConfigVersion(pub u64);

impl ConfigVersion {
    pub const INITIAL: Self = Self(1);

    #[must_use]
    pub fn checked_next(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResourceState {
    #[default]
    Active,
    Suspended,
    Archived,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecallPolicy {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_recall_results")]
    pub max_results: u32,
}

impl Default for RecallPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            max_results: default_recall_results(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractionPolicy {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for ExtractionPolicy {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RetentionPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u32>,
}

const fn default_true() -> bool {
    true
}

const fn default_recall_results() -> u32 {
    10
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryStoreDefinition {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub state: ResourceState,
    pub current_config_version: ConfigVersion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryStoreConfigVersion {
    pub memory_store_id: String,
    pub version: ConfigVersion,
    #[serde(default)]
    pub recall_policy: RecallPolicy,
    #[serde(default)]
    pub extraction_policy: ExtractionPolicy,
    #[serde(default)]
    pub retention_policy: RetentionPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryDefinition {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub state: ResourceState,
    pub current_config_version: ConfigVersion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ClonePolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryConfigVersion {
    pub repository_id: String,
    pub version: ConfigVersion,
    pub remote_url: String,
    /// A Vault binding/reference, never credential material.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_binding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_branch: Option<String>,
    #[serde(default)]
    pub clone_policy: ClonePolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResourceCatalogError {
    #[error("resource `{0}` already exists")]
    AlreadyExists(String),
    #[error("resource `{0}` was not found in this Workspace")]
    NotFound(String),
    #[error("resource `{id}` is not active ({state:?})")]
    NotActive { id: String, state: ResourceState },
    #[error("resource `{id}` config changed (expected {expected:?}, current {current:?})")]
    ConfigConflict {
        id: String,
        expected: ConfigVersion,
        current: ConfigVersion,
    },
    #[error("invalid resource catalog write: {0}")]
    Invalid(String),
    #[error("resource catalog storage failure: {0}")]
    Storage(String),
}

/// Narrow read port consumed by Session resolution. It deliberately exposes no
/// authoring or lifecycle mutation, following interface segregation: resolving
/// an execution manifest cannot publish, suspend, archive, or delete resources.
pub trait ResourceConfigSource: Send + Sync {
    fn resolve_memory_store(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<MemoryStoreConfigVersion, ResourceCatalogError>;

    fn resolve_repository(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<RepositoryConfigVersion, ResourceCatalogError>;
}

/// Secret-free Resource Catalog application port. Authorization decisions are made
/// outside this boundary; Workspace ownership and lifecycle are intrinsic invariants.
pub trait ResourceCatalog: ResourceConfigSource {
    fn create_memory_store(
        &self,
        definition: MemoryStoreDefinition,
        initial_config: MemoryStoreConfigVersion,
    ) -> Result<(), ResourceCatalogError>;
    fn memory_store(&self, workspace_id: &str, id: &str) -> Option<MemoryStoreDefinition>;
    /// Definitions owned by one Workspace, sorted by id. Archived/deleted rows
    /// remain available by id but are excluded from this ordinary inventory.
    fn list_memory_stores(&self, workspace_id: &str) -> Vec<MemoryStoreDefinition>;
    /// Update descriptive fields of an existing definition without changing its
    /// owner, lifecycle state, or current config pointer.
    fn update_memory_store(
        &self,
        definition: MemoryStoreDefinition,
    ) -> Result<(), ResourceCatalogError>;
    fn memory_config(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Option<MemoryStoreConfigVersion>;
    fn publish_memory_config(
        &self,
        workspace_id: &str,
        expected_current: ConfigVersion,
        config: MemoryStoreConfigVersion,
    ) -> Result<(), ResourceCatalogError>;
    fn set_memory_state(
        &self,
        workspace_id: &str,
        id: &str,
        state: ResourceState,
    ) -> Result<(), ResourceCatalogError>;

    fn create_repository(
        &self,
        definition: RepositoryDefinition,
        initial_config: RepositoryConfigVersion,
    ) -> Result<(), ResourceCatalogError>;
    fn repository(&self, workspace_id: &str, id: &str) -> Option<RepositoryDefinition>;
    fn repository_config(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Option<RepositoryConfigVersion>;
    fn publish_repository_config(
        &self,
        workspace_id: &str,
        expected_current: ConfigVersion,
        config: RepositoryConfigVersion,
    ) -> Result<(), ResourceCatalogError>;
    fn set_repository_state(
        &self,
        workspace_id: &str,
        id: &str,
        state: ResourceState,
    ) -> Result<(), ResourceCatalogError>;
}
