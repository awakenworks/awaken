//! Resource Catalog domain vocabulary and port.
//!
//! This module owns resource identity, immutable configuration versions, and live
//! lifecycle state. It intentionally contains no principal, API key, role, policy,
//! Org, Project, or WorkUnit type: a PEP authorizes first and then invokes this port
//! with a trusted Workspace.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{MemoryStoreId, RepositoryId};

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

/// Durable lifecycle metadata owned by the resource aggregate. Nanoseconds keep
/// storage/backend representations neutral; wire adapters choose their timestamp format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceTimestamps {
    #[serde(default)]
    pub created_unix_nanos: u64,
    #[serde(default)]
    pub updated_unix_nanos: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_unix_nanos: Option<u64>,
}

impl ResourceTimestamps {
    #[must_use]
    pub const fn created(at_unix_nanos: u64) -> Self {
        Self {
            created_unix_nanos: at_unix_nanos,
            updated_unix_nanos: at_unix_nanos,
            archived_unix_nanos: None,
        }
    }

    pub fn touch(&mut self, at_unix_nanos: u64) {
        self.updated_unix_nanos = at_unix_nanos;
    }

    pub fn transition_to(&mut self, state: ResourceState, at_unix_nanos: u64) {
        self.touch(at_unix_nanos);
        self.archived_unix_nanos =
            matches!(state, ResourceState::Archived | ResourceState::Deleted)
                .then_some(at_unix_nanos);
    }
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
    pub id: MemoryStoreId,
    pub workspace_id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub state: ResourceState,
    pub current_config_version: ConfigVersion,
    #[serde(default)]
    pub timestamps: ResourceTimestamps,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryStoreConfigVersion {
    pub memory_store_id: MemoryStoreId,
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
    pub id: RepositoryId,
    pub workspace_id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub state: ResourceState,
    pub current_config_version: ConfigVersion,
    #[serde(default)]
    pub timestamps: ResourceTimestamps,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ClonePolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryConfigVersion {
    pub repository_id: RepositoryId,
    pub version: ConfigVersion,
    pub remote_url: String,
    /// A Vault binding/reference, never credential material.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_binding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_branch: Option<String>,
    /// Exact commit selected for the initial working tree. Mutually exclusive
    /// with `initial_branch`; unlike a branch preference this is immutable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_commit: Option<String>,
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
    #[error("resource `{id}` config version {version:?} was not found")]
    ConfigNotFound { id: String, version: ConfigVersion },
    #[error("invalid resource catalog write: {0}")]
    Invalid(String),
    #[error("resource catalog storage failure: {0}")]
    Storage(String),
}

/// Backend-neutral invariants for the Resource Catalog aggregate.
///
/// Adapters persist records differently, but creation, publication, lifecycle,
/// and immutable configuration identity must have one executable definition.
/// These rules contain no authentication or authorization concepts.
pub struct ResourceCatalogRules;

impl ResourceCatalogRules {
    fn validate_definition_identity(
        row_id: &str,
        definition_id: &str,
        workspace_id: &str,
        current: ConfigVersion,
    ) -> Result<(), ResourceCatalogError> {
        if row_id.trim().is_empty()
            || definition_id != row_id
            || workspace_id.trim().is_empty()
            || current.0 == 0
        {
            return Err(ResourceCatalogError::Storage(format!(
                "resource catalog aggregate `{row_id}` has an invalid definition identity"
            )));
        }
        Ok(())
    }

    pub fn validate_memory_aggregate<'a>(
        row_id: &str,
        definition: &MemoryStoreDefinition,
        configs: impl IntoIterator<Item = (&'a ConfigVersion, &'a MemoryStoreConfigVersion)>,
    ) -> Result<(), ResourceCatalogError> {
        Self::validate_definition_identity(
            row_id,
            definition.id.as_str(),
            definition.workspace_id.as_str(),
            definition.current_config_version,
        )?;
        let mut current_exists = false;
        let mut count = 0_usize;
        for (version, config) in configs {
            count += 1;
            Self::validate_memory_config(row_id, *version, config)?;
            current_exists |= *version == definition.current_config_version;
        }
        if count == 0 || !current_exists {
            return Err(ResourceCatalogError::Storage(format!(
                "MemoryStore `{row_id}` current config version is missing"
            )));
        }
        Ok(())
    }

    pub fn validate_repository_aggregate<'a>(
        row_id: &str,
        definition: &RepositoryDefinition,
        configs: impl IntoIterator<Item = (&'a ConfigVersion, &'a RepositoryConfigVersion)>,
    ) -> Result<(), ResourceCatalogError> {
        Self::validate_definition_identity(
            row_id,
            definition.id.as_str(),
            definition.workspace_id.as_str(),
            definition.current_config_version,
        )?;
        let mut current_exists = false;
        let mut count = 0_usize;
        for (version, config) in configs {
            count += 1;
            Self::validate_repository_config(row_id, *version, config)?;
            current_exists |= *version == definition.current_config_version;
        }
        if count == 0 || !current_exists {
            return Err(ResourceCatalogError::Storage(format!(
                "Repository `{row_id}` current config version is missing"
            )));
        }
        Ok(())
    }

    pub fn validate_live_definition(
        id: &str,
        state: ResourceState,
    ) -> Result<(), ResourceCatalogError> {
        if state == ResourceState::Active {
            Ok(())
        } else {
            Err(ResourceCatalogError::NotActive {
                id: id.into(),
                state,
            })
        }
    }

    pub fn validate_memory_config(
        id: &str,
        version: ConfigVersion,
        config: &MemoryStoreConfigVersion,
    ) -> Result<(), ResourceCatalogError> {
        if config.memory_store_id.as_str() == id && config.version == version {
            Ok(())
        } else {
            Err(ResourceCatalogError::Storage(format!(
                "MemoryStore `{id}` config version {} is corrupt",
                version.0
            )))
        }
    }

    pub fn validate_repository_config(
        id: &str,
        version: ConfigVersion,
        config: &RepositoryConfigVersion,
    ) -> Result<(), ResourceCatalogError> {
        if config.repository_id.as_str() != id || config.version != version {
            Err(ResourceCatalogError::Storage(format!(
                "Repository `{id}` config version {} is corrupt",
                version.0
            )))
        } else if config.initial_branch.is_some() && config.initial_commit.is_some() {
            Err(ResourceCatalogError::Invalid(
                "Repository checkout cannot select both branch and commit".into(),
            ))
        } else if config
            .initial_branch
            .iter()
            .chain(config.initial_commit.iter())
            .any(|value| value.trim().is_empty())
        {
            Err(ResourceCatalogError::Invalid(
                "Repository checkout value must be non-empty".into(),
            ))
        } else {
            Ok(())
        }
    }

    pub fn validate_initial(
        id: &str,
        workspace_id: &str,
        current: ConfigVersion,
        config_id: &str,
        config_version: ConfigVersion,
    ) -> Result<(), ResourceCatalogError> {
        if id.trim().is_empty() || workspace_id.trim().is_empty() {
            return Err(ResourceCatalogError::Invalid(
                "resource id and workspace id must be non-empty".into(),
            ));
        }
        if id != config_id
            || current != ConfigVersion::INITIAL
            || config_version != ConfigVersion::INITIAL
        {
            return Err(ResourceCatalogError::Invalid(
                "initial resource definition/config must agree at version 1".into(),
            ));
        }
        Ok(())
    }

    pub fn validate_publish(
        id: &str,
        current: ConfigVersion,
        expected: ConfigVersion,
        next: ConfigVersion,
    ) -> Result<(), ResourceCatalogError> {
        if current != expected {
            return Err(ResourceCatalogError::ConfigConflict {
                id: id.into(),
                expected,
                current,
            });
        }
        let required = current.checked_next().ok_or_else(|| {
            ResourceCatalogError::Invalid(format!("resource `{id}` exhausted config versions"))
        })?;
        if next != required {
            return Err(ResourceCatalogError::Invalid(format!(
                "resource `{id}` config version must advance from {} to {}",
                current.0, required.0
            )));
        }
        Ok(())
    }
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

/// Live resource-invariant check consumed at binding activation and use. This is
/// intentionally separate from [`ResourceConfigSource`]: a Session selects a
/// configuration exactly once, while the Runtime only verifies that the trusted
/// Workspace still owns an active resource and that the frozen version remains
/// intact. Authorization principals, policies, and decisions stay outside this
/// port.
pub trait ResourceBindingValidator: Send + Sync {
    fn validate_memory_binding(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Result<(), ResourceCatalogError>;

    fn validate_repository_binding(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Result<(), ResourceCatalogError>;
}

/// Secret-free Resource Catalog application port. Authorization decisions are made
/// outside this boundary; Workspace ownership and lifecycle are intrinsic invariants.
pub trait ResourceCatalog: ResourceConfigSource + ResourceBindingValidator {
    fn create_memory_store(
        &self,
        definition: MemoryStoreDefinition,
        initial_config: MemoryStoreConfigVersion,
    ) -> Result<(), ResourceCatalogError>;
    fn memory_store(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Option<MemoryStoreDefinition>, ResourceCatalogError>;
    /// Definitions owned by one Workspace, sorted by id. Archived/deleted rows
    /// remain available by id but are excluded from this ordinary inventory.
    fn list_memory_stores(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<MemoryStoreDefinition>, ResourceCatalogError>;
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
    ) -> Result<Option<MemoryStoreConfigVersion>, ResourceCatalogError>;
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
    fn repository(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Option<RepositoryDefinition>, ResourceCatalogError>;
    fn repository_config(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Result<Option<RepositoryConfigVersion>, ResourceCatalogError>;
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
