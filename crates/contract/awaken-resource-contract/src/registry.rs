//! Resource Registry domain vocabulary and ports.
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RetentionPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u32>,
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

/// Monotonic persistence revision for optimistic concurrency. This is
/// deliberately independent from [`ConfigVersion`]: profile and lifecycle
/// changes modify an aggregate without publishing a new configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct AggregateRevision(u64);

impl AggregateRevision {
    pub const INITIAL: Self = Self(1);
    pub const MAX: u64 = i64::MAX as u64;

    pub fn new(value: u64) -> Result<Self, RegistryRepositoryError> {
        if value == 0 || value > Self::MAX {
            Err(RegistryRepositoryError::CorruptData(format!(
                "aggregate revision {value} is outside the portable database range"
            )))
        } else {
            Ok(Self(value))
        }
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    #[must_use]
    pub fn checked_next(self) -> Option<Self> {
        (self.0 < Self::MAX).then(|| Self(self.0 + 1))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored<T> {
    pub revision: AggregateRevision,
    pub aggregate: T,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    Inserted,
    AlreadyRegistered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceOutcome {
    Replaced { revision: AggregateRevision },
    ConcurrentModification,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResourceRegistryError {
    #[error("resource `{0}` is already registered")]
    AlreadyRegistered(String),
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
    #[error("resource `{0}` was concurrently modified")]
    ConcurrentModification(String),
    #[error("invalid resource registry command: {0}")]
    Invalid(String),
    #[error("resource registry data is corrupt: {0}")]
    CorruptData(String),
    #[error("resource registry is unavailable: {0}")]
    Unavailable(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryRepositoryError {
    #[error("resource `{0}` already exists")]
    AlreadyExists(String),
    #[error("resource `{0}` was not found")]
    NotFound(String),
    #[error("resource `{0}` was concurrently modified")]
    ConcurrentModification(String),
    #[error("stored resource registry data is corrupt: {0}")]
    CorruptData(String),
    #[error("resource registry repository is unavailable: {0}")]
    Unavailable(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigPublicationDecision {
    Accept,
    StaleExpected,
    VersionExhausted,
    NonSuccessor,
}

const fn config_publication_decision(
    current: ConfigVersion,
    expected: ConfigVersion,
    next: ConfigVersion,
) -> ConfigPublicationDecision {
    if current.0 != expected.0 {
        ConfigPublicationDecision::StaleExpected
    } else if current.0 == u64::MAX {
        ConfigPublicationDecision::VersionExhausted
    } else if next.0 != current.0 + 1 {
        ConfigPublicationDecision::NonSuccessor
    } else {
        ConfigPublicationDecision::Accept
    }
}

fn validate_definition(
    id: &str,
    workspace_id: &str,
    current: ConfigVersion,
) -> Result<(), ResourceRegistryError> {
    if id.trim().is_empty() || workspace_id.trim().is_empty() || current.0 == 0 {
        return Err(ResourceRegistryError::CorruptData(format!(
            "resource registry aggregate `{id}` has an invalid definition"
        )));
    }
    Ok(())
}

fn validate_live_definition(id: &str, state: ResourceState) -> Result<(), ResourceRegistryError> {
    if state == ResourceState::Active {
        Ok(())
    } else {
        Err(ResourceRegistryError::NotActive {
            id: id.into(),
            state,
        })
    }
}

fn validate_publish(
    id: &str,
    current: ConfigVersion,
    expected: ConfigVersion,
    next: ConfigVersion,
) -> Result<(), ResourceRegistryError> {
    match config_publication_decision(current, expected, next) {
        ConfigPublicationDecision::Accept => Ok(()),
        ConfigPublicationDecision::StaleExpected => Err(ResourceRegistryError::ConfigConflict {
            id: id.into(),
            expected,
            current,
        }),
        ConfigPublicationDecision::VersionExhausted => Err(ResourceRegistryError::Invalid(
            format!("resource `{id}` exhausted config versions"),
        )),
        ConfigPublicationDecision::NonSuccessor => Err(ResourceRegistryError::Invalid(format!(
            "resource `{id}` config version must advance from {} to {}",
            current.0,
            current.0 + 1
        ))),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryStoreAggregate {
    definition: MemoryStoreDefinition,
    configs: BTreeMap<ConfigVersion, MemoryStoreConfigVersion>,
}

impl MemoryStoreAggregate {
    pub fn register(
        definition: MemoryStoreDefinition,
        initial_config: MemoryStoreConfigVersion,
    ) -> Result<Self, ResourceRegistryError> {
        if definition.id.as_str().trim().is_empty() || definition.workspace_id.trim().is_empty() {
            return Err(ResourceRegistryError::Invalid(
                "resource id and workspace id must be non-empty".into(),
            ));
        }
        if definition.id != initial_config.memory_store_id
            || definition.current_config_version != ConfigVersion::INITIAL
            || initial_config.version != ConfigVersion::INITIAL
        {
            return Err(ResourceRegistryError::Invalid(
                "initial MemoryStore definition/config must agree at version 1".into(),
            ));
        }
        Self::rehydrate(
            definition,
            BTreeMap::from([(ConfigVersion::INITIAL, initial_config)]),
        )
    }

    pub fn rehydrate(
        definition: MemoryStoreDefinition,
        configs: BTreeMap<ConfigVersion, MemoryStoreConfigVersion>,
    ) -> Result<Self, ResourceRegistryError> {
        validate_memory_store_integrity(&definition, &configs)?;
        Ok(Self {
            definition,
            configs,
        })
    }

    pub fn validate_integrity(&self) -> Result<(), ResourceRegistryError> {
        validate_memory_store_integrity(&self.definition, &self.configs)
    }

    #[must_use]
    pub fn definition(&self) -> &MemoryStoreDefinition {
        &self.definition
    }

    #[must_use]
    pub fn into_definition(self) -> MemoryStoreDefinition {
        self.definition
    }

    #[must_use]
    pub fn config(&self, version: ConfigVersion) -> Option<&MemoryStoreConfigVersion> {
        self.configs.get(&version)
    }

    #[must_use]
    pub fn into_config(mut self, version: ConfigVersion) -> Option<MemoryStoreConfigVersion> {
        self.configs.remove(&version)
    }

    pub fn resolve_for_execution(
        &self,
        workspace_id: &str,
    ) -> Result<MemoryStoreConfigVersion, ResourceRegistryError> {
        self.ensure_workspace(workspace_id)?;
        validate_live_definition(self.definition.id.as_str(), self.definition.state)?;
        self.config(self.definition.current_config_version)
            .cloned()
            .ok_or_else(|| {
                ResourceRegistryError::CorruptData(format!(
                    "MemoryStore `{}` current config version is missing",
                    self.definition.id
                ))
            })
    }

    pub fn verify_live_binding(
        &self,
        workspace_id: &str,
        version: ConfigVersion,
    ) -> Result<(), ResourceRegistryError> {
        self.ensure_workspace(workspace_id)?;
        validate_live_definition(self.definition.id.as_str(), self.definition.state)?;
        self.config(version)
            .map(|_| ())
            .ok_or_else(|| ResourceRegistryError::ConfigNotFound {
                id: self.definition.id.to_string(),
                version,
            })
    }

    pub fn update_profile(
        &mut self,
        name: String,
        description: String,
        metadata: BTreeMap<String, String>,
        updated_at: u64,
    ) {
        self.definition.name = name;
        self.definition.description = description;
        self.definition.metadata = metadata;
        self.definition.timestamps.touch(updated_at);
    }

    pub fn publish_config(
        &mut self,
        workspace_id: &str,
        expected_current: ConfigVersion,
        config: MemoryStoreConfigVersion,
        updated_at: u64,
    ) -> Result<(), ResourceRegistryError> {
        self.ensure_workspace(workspace_id)?;
        if config.memory_store_id != self.definition.id {
            return Err(ResourceRegistryError::Invalid(
                "MemoryStore config identity does not match its aggregate".into(),
            ));
        }
        validate_publish(
            self.definition.id.as_str(),
            self.definition.current_config_version,
            expected_current,
            config.version,
        )?;
        if self.definition.state == ResourceState::Deleted {
            return Err(ResourceRegistryError::NotActive {
                id: self.definition.id.to_string(),
                state: ResourceState::Deleted,
            });
        }
        self.definition.current_config_version = config.version;
        self.configs.insert(config.version, config);
        self.definition.timestamps.touch(updated_at);
        Ok(())
    }

    pub fn change_state(
        &mut self,
        workspace_id: &str,
        state: ResourceState,
        updated_at: u64,
    ) -> Result<(), ResourceRegistryError> {
        self.ensure_workspace(workspace_id)?;
        self.definition.state = state;
        self.definition.timestamps.transition_to(state, updated_at);
        Ok(())
    }

    fn ensure_workspace(&self, workspace_id: &str) -> Result<(), ResourceRegistryError> {
        if self.definition.workspace_id == workspace_id {
            Ok(())
        } else {
            Err(ResourceRegistryError::NotFound(
                self.definition.id.to_string(),
            ))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryAggregate {
    definition: RepositoryDefinition,
    configs: BTreeMap<ConfigVersion, RepositoryConfigVersion>,
}

impl RepositoryAggregate {
    pub fn register(
        definition: RepositoryDefinition,
        initial_config: RepositoryConfigVersion,
    ) -> Result<Self, ResourceRegistryError> {
        if definition.id.as_str().trim().is_empty() || definition.workspace_id.trim().is_empty() {
            return Err(ResourceRegistryError::Invalid(
                "resource id and workspace id must be non-empty".into(),
            ));
        }
        if definition.id != initial_config.repository_id
            || definition.current_config_version != ConfigVersion::INITIAL
            || initial_config.version != ConfigVersion::INITIAL
        {
            return Err(ResourceRegistryError::Invalid(
                "initial Repository definition/config must agree at version 1".into(),
            ));
        }
        Self::rehydrate(
            definition,
            BTreeMap::from([(ConfigVersion::INITIAL, initial_config)]),
        )
    }

    pub fn rehydrate(
        definition: RepositoryDefinition,
        configs: BTreeMap<ConfigVersion, RepositoryConfigVersion>,
    ) -> Result<Self, ResourceRegistryError> {
        validate_repository_integrity(&definition, &configs)?;
        Ok(Self {
            definition,
            configs,
        })
    }

    pub fn validate_integrity(&self) -> Result<(), ResourceRegistryError> {
        validate_repository_integrity(&self.definition, &self.configs)
    }

    #[must_use]
    pub fn definition(&self) -> &RepositoryDefinition {
        &self.definition
    }

    #[must_use]
    pub fn into_definition(self) -> RepositoryDefinition {
        self.definition
    }

    #[must_use]
    pub fn config(&self, version: ConfigVersion) -> Option<&RepositoryConfigVersion> {
        self.configs.get(&version)
    }

    #[must_use]
    pub fn into_config(mut self, version: ConfigVersion) -> Option<RepositoryConfigVersion> {
        self.configs.remove(&version)
    }

    pub fn resolve_for_execution(
        &self,
        workspace_id: &str,
    ) -> Result<RepositoryConfigVersion, ResourceRegistryError> {
        self.ensure_workspace(workspace_id)?;
        validate_live_definition(self.definition.id.as_str(), self.definition.state)?;
        self.config(self.definition.current_config_version)
            .cloned()
            .ok_or_else(|| {
                ResourceRegistryError::CorruptData(format!(
                    "Repository `{}` current config version is missing",
                    self.definition.id
                ))
            })
    }

    pub fn verify_live_binding(
        &self,
        workspace_id: &str,
        version: ConfigVersion,
    ) -> Result<(), ResourceRegistryError> {
        self.ensure_workspace(workspace_id)?;
        validate_live_definition(self.definition.id.as_str(), self.definition.state)?;
        self.config(version)
            .map(|_| ())
            .ok_or_else(|| ResourceRegistryError::ConfigNotFound {
                id: self.definition.id.to_string(),
                version,
            })
    }

    pub fn publish_config(
        &mut self,
        workspace_id: &str,
        expected_current: ConfigVersion,
        config: RepositoryConfigVersion,
        updated_at: u64,
    ) -> Result<(), ResourceRegistryError> {
        self.ensure_workspace(workspace_id)?;
        validate_repository_config(self.definition.id.as_str(), config.version, &config)?;
        validate_publish(
            self.definition.id.as_str(),
            self.definition.current_config_version,
            expected_current,
            config.version,
        )?;
        if self.definition.state == ResourceState::Deleted {
            return Err(ResourceRegistryError::NotActive {
                id: self.definition.id.to_string(),
                state: ResourceState::Deleted,
            });
        }
        self.definition.current_config_version = config.version;
        self.configs.insert(config.version, config);
        self.definition.timestamps.touch(updated_at);
        Ok(())
    }

    pub fn change_state(
        &mut self,
        workspace_id: &str,
        state: ResourceState,
        updated_at: u64,
    ) -> Result<(), ResourceRegistryError> {
        self.ensure_workspace(workspace_id)?;
        self.definition.state = state;
        self.definition.timestamps.transition_to(state, updated_at);
        Ok(())
    }

    fn ensure_workspace(&self, workspace_id: &str) -> Result<(), ResourceRegistryError> {
        if self.definition.workspace_id == workspace_id {
            Ok(())
        } else {
            Err(ResourceRegistryError::NotFound(
                self.definition.id.to_string(),
            ))
        }
    }
}

fn validate_memory_store_integrity(
    definition: &MemoryStoreDefinition,
    configs: &BTreeMap<ConfigVersion, MemoryStoreConfigVersion>,
) -> Result<(), ResourceRegistryError> {
    validate_definition(
        definition.id.as_str(),
        &definition.workspace_id,
        definition.current_config_version,
    )?;
    for (version, config) in configs {
        if config.memory_store_id != definition.id || config.version != *version {
            return Err(ResourceRegistryError::CorruptData(format!(
                "MemoryStore `{}` config version {} is corrupt",
                definition.id, version.0
            )));
        }
    }
    if configs.is_empty() || !configs.contains_key(&definition.current_config_version) {
        return Err(ResourceRegistryError::CorruptData(format!(
            "MemoryStore `{}` current config version is missing",
            definition.id
        )));
    }
    Ok(())
}

fn validate_repository_integrity(
    definition: &RepositoryDefinition,
    configs: &BTreeMap<ConfigVersion, RepositoryConfigVersion>,
) -> Result<(), ResourceRegistryError> {
    validate_definition(
        definition.id.as_str(),
        &definition.workspace_id,
        definition.current_config_version,
    )?;
    for (version, config) in configs {
        validate_repository_config(definition.id.as_str(), *version, config)?;
    }
    if configs.is_empty() || !configs.contains_key(&definition.current_config_version) {
        return Err(ResourceRegistryError::CorruptData(format!(
            "Repository `{}` current config version is missing",
            definition.id
        )));
    }
    Ok(())
}

fn validate_repository_config(
    id: &str,
    version: ConfigVersion,
    config: &RepositoryConfigVersion,
) -> Result<(), ResourceRegistryError> {
    if config.repository_id.as_str() != id || config.version != version {
        Err(ResourceRegistryError::CorruptData(format!(
            "Repository `{id}` config version {} is corrupt",
            version.0
        )))
    } else if config.initial_branch.is_some() && config.initial_commit.is_some() {
        Err(ResourceRegistryError::Invalid(
            "Repository checkout cannot select both branch and commit".into(),
        ))
    } else if config
        .initial_branch
        .iter()
        .chain(config.initial_commit.iter())
        .any(|value| value.trim().is_empty())
    {
        Err(ResourceRegistryError::Invalid(
            "Repository checkout value must be non-empty".into(),
        ))
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterMemoryStore {
    pub definition: MemoryStoreDefinition,
    pub initial_config: MemoryStoreConfigVersion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateMemoryStoreProfile {
    pub workspace_id: String,
    pub id: MemoryStoreId,
    pub name: String,
    pub description: String,
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishMemoryStoreConfig {
    pub workspace_id: String,
    pub expected_current: ConfigVersion,
    pub config: MemoryStoreConfigVersion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeMemoryStoreState {
    pub workspace_id: String,
    pub id: MemoryStoreId,
    pub state: ResourceState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterRepository {
    pub definition: RepositoryDefinition,
    pub initial_config: RepositoryConfigVersion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishRepositoryConfig {
    pub workspace_id: String,
    pub expected_current: ConfigVersion,
    pub config: RepositoryConfigVersion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeRepositoryState {
    pub workspace_id: String,
    pub id: RepositoryId,
    pub state: ResourceState,
}

/// Query definitions and immutable configuration history.
pub trait ResourceInventory: Send + Sync {
    fn find_memory_store(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Option<MemoryStoreDefinition>, ResourceRegistryError>;
    fn list_memory_stores(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<MemoryStoreDefinition>, ResourceRegistryError>;
    fn find_memory_store_config(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Result<Option<MemoryStoreConfigVersion>, ResourceRegistryError>;
    fn find_repository(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Option<RepositoryDefinition>, ResourceRegistryError>;
    fn find_repository_config(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Result<Option<RepositoryConfigVersion>, ResourceRegistryError>;
}

/// Register resources, publish configuration, and change lifecycle state.
pub trait ResourceAdministration: Send + Sync {
    fn register_memory_store(
        &self,
        command: RegisterMemoryStore,
    ) -> Result<(), ResourceRegistryError>;
    fn update_memory_store_profile(
        &self,
        command: UpdateMemoryStoreProfile,
    ) -> Result<(), ResourceRegistryError>;
    fn publish_memory_store_config(
        &self,
        command: PublishMemoryStoreConfig,
    ) -> Result<(), ResourceRegistryError>;
    fn change_memory_store_state(
        &self,
        command: ChangeMemoryStoreState,
    ) -> Result<(), ResourceRegistryError>;
    fn register_repository(&self, command: RegisterRepository)
    -> Result<(), ResourceRegistryError>;
    fn publish_repository_config(
        &self,
        command: PublishRepositoryConfig,
    ) -> Result<(), ResourceRegistryError>;
    fn change_repository_state(
        &self,
        command: ChangeRepositoryState,
    ) -> Result<(), ResourceRegistryError>;
}

/// Resolve the current active configuration selected for execution.
pub trait ExecutionResourceResolver: Send + Sync {
    fn resolve_memory_store(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<MemoryStoreConfigVersion, ResourceRegistryError>;
    fn resolve_repository(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<RepositoryConfigVersion, ResourceRegistryError>;
}

/// Verify that a frozen binding remains owned, active, and intact.
pub trait LiveResourceBindingVerifier: Send + Sync {
    fn verify_memory_binding(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Result<(), ResourceRegistryError>;
    fn verify_repository_binding(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Result<(), ResourceRegistryError>;
}

pub trait ResourceRegistry:
    ResourceInventory + ResourceAdministration + ExecutionResourceResolver + LiveResourceBindingVerifier
{
}

impl<T> ResourceRegistry for T where
    T: ResourceInventory
        + ResourceAdministration
        + ExecutionResourceResolver
        + LiveResourceBindingVerifier
{
}

/// Backend-neutral aggregate repository. SQL, transaction APIs, JSON types,
/// and vendor error codes must not cross this boundary.
pub trait ResourceRegistryRepository: Send + Sync {
    fn load_memory_store(
        &self,
        id: &str,
    ) -> Result<Option<Stored<MemoryStoreAggregate>>, RegistryRepositoryError>;
    fn insert_memory_store(
        &self,
        aggregate: &MemoryStoreAggregate,
    ) -> Result<InsertOutcome, RegistryRepositoryError>;
    fn replace_memory_store(
        &self,
        expected_revision: AggregateRevision,
        aggregate: &MemoryStoreAggregate,
    ) -> Result<ReplaceOutcome, RegistryRepositoryError>;
    fn list_memory_stores(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<Stored<MemoryStoreAggregate>>, RegistryRepositoryError>;
    fn load_repository(
        &self,
        id: &str,
    ) -> Result<Option<Stored<RepositoryAggregate>>, RegistryRepositoryError>;
    fn insert_repository(
        &self,
        aggregate: &RepositoryAggregate,
    ) -> Result<InsertOutcome, RegistryRepositoryError>;
    fn replace_repository(
        &self,
        expected_revision: AggregateRevision,
        aggregate: &RepositoryAggregate,
    ) -> Result<ReplaceOutcome, RegistryRepositoryError>;
}

#[cfg(kani)]
mod registry_proofs {
    use super::*;

    #[kani::proof]
    fn resource_config_publication_is_exact_and_never_wraps() {
        let current = ConfigVersion(kani::any());
        let expected = ConfigVersion(kani::any());
        let next = ConfigVersion(kani::any());
        let decision = config_publication_decision(current, expected, next);

        assert_eq!(
            decision == ConfigPublicationDecision::Accept,
            current == expected && current.0 < u64::MAX && next.0 == current.0 + 1
        );
        if decision == ConfigPublicationDecision::Accept {
            assert!(next.0 > current.0);
        }
    }

    #[kani::proof]
    fn exhausted_resource_config_versions_fail_closed() {
        let next = ConfigVersion(kani::any());
        assert_eq!(
            config_publication_decision(ConfigVersion(u64::MAX), ConfigVersion(u64::MAX), next,),
            ConfigPublicationDecision::VersionExhausted
        );
    }

    #[kani::proof]
    fn portable_aggregate_revision_is_strictly_monotonic_and_never_wraps() {
        let value: u64 = kani::any();
        kani::assume(value > 0 && value <= AggregateRevision::MAX);
        let revision = AggregateRevision(value);
        match revision.checked_next() {
            Some(next) => {
                assert!(value < AggregateRevision::MAX);
                assert_eq!(next.get(), value + 1);
                assert!(next > revision);
            }
            None => assert_eq!(value, AggregateRevision::MAX),
        }
    }

    #[kani::proof]
    fn resource_lifecycle_timestamps_project_the_exact_transition() {
        let state_code: u8 = kani::any();
        let at: u64 = kani::any();
        let mut timestamps = ResourceTimestamps {
            created_unix_nanos: kani::any(),
            updated_unix_nanos: kani::any(),
            archived_unix_nanos: if kani::any() { Some(kani::any()) } else { None },
        };
        kani::assume(state_code <= 3);
        let state = match state_code {
            0 => ResourceState::Active,
            1 => ResourceState::Suspended,
            2 => ResourceState::Archived,
            _ => ResourceState::Deleted,
        };

        timestamps.transition_to(state, at);

        assert_eq!(timestamps.updated_unix_nanos, at);
        assert_eq!(
            timestamps.archived_unix_nanos,
            matches!(state, ResourceState::Archived | ResourceState::Deleted).then_some(at)
        );
    }
}
