// @formal entrypoint resource_catalog.aggregate_revision_is_strictly_monotonic_and_never_wraps
// @formal entrypoint resource_catalog.config_publication_is_exact_and_never_wraps
// @formal entrypoint resource_catalog.lifecycle_timestamp_projection_is_total_and_exact
// @formal entrypoint session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin
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

/// The Registry policy required by a registration's existing consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositoryRegistrationMode {
    Execution,
    Preparation,
}

/// Exact replay is decided once by the Registry, never reconstructed by a caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryRegistration {
    pub definition: RepositoryDefinition,
    pub outcome: InsertOutcome,
    pub preparation: Option<RepositoryPreparation>,
}

/// Opaque Registry observation carried by its staged registration result.
/// An administrative mutation invalidates it; exact completion remains replayable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryPreparation {
    revision: AggregateRevision,
    request_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RepositoryPreparationPhase {
    Pending,
    Completed { prepared_revision: u64 },
    Closed,
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
    #[error("resource identity was rejected: {detail}")]
    IdentityRejected { detail: String },
    #[error("resource configuration was rejected: {detail}")]
    ConfigurationRejected { detail: String },
    #[error("resource admission was rejected: {detail}")]
    AdmissionRejected { detail: String },
    #[error("resource config version space is exhausted for `{id}`")]
    VersionExhausted { id: String },
    #[error("resource registry request encoding failed: {detail}")]
    EncodingFailure { detail: String },
    #[error("resource registry data is corrupt: {0}")]
    CorruptData(String),
    #[error("resource registry authority unavailable: {detail}")]
    AuthorityUnavailable { detail: String },
    #[error("resource registry outcome is indeterminate: {detail}")]
    Indeterminate { detail: String },
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
    /// The repository supplied no definitive receipt. Mutation callers must
    /// reuse their existing identity/CAS, not infer that no write occurred.
    #[error("resource registry repository outcome is indeterminate: {detail}")]
    Indeterminate { detail: String },
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

fn ensure_resource_workspace(
    id: &str,
    resource_workspace_id: &str,
    requested_workspace_id: &str,
) -> Result<(), ResourceRegistryError> {
    if resource_workspace_id == requested_workspace_id {
        Ok(())
    } else {
        Err(ResourceRegistryError::NotFound(id.to_string()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LiveBindingAdmission {
    Accept,
    WorkspaceMismatch,
    NotActive,
    ConfigMissing,
}

const fn live_binding_admission(
    workspace_matches: bool,
    state: ResourceState,
    config_exists: bool,
) -> LiveBindingAdmission {
    if !workspace_matches {
        LiveBindingAdmission::WorkspaceMismatch
    } else if !matches!(state, ResourceState::Active) {
        LiveBindingAdmission::NotActive
    } else if !config_exists {
        LiveBindingAdmission::ConfigMissing
    } else {
        LiveBindingAdmission::Accept
    }
}

fn verify_live_binding_facts(
    id: &str,
    resource_workspace_id: &str,
    requested_workspace_id: &str,
    state: ResourceState,
    version: ConfigVersion,
    config_exists: bool,
) -> Result<(), ResourceRegistryError> {
    match live_binding_admission(
        resource_workspace_id == requested_workspace_id,
        state,
        config_exists,
    ) {
        LiveBindingAdmission::Accept => Ok(()),
        LiveBindingAdmission::WorkspaceMismatch => {
            Err(ResourceRegistryError::NotFound(id.to_string()))
        }
        LiveBindingAdmission::NotActive => Err(ResourceRegistryError::NotActive {
            id: id.to_string(),
            state,
        }),
        LiveBindingAdmission::ConfigMissing => Err(ResourceRegistryError::ConfigNotFound {
            id: id.to_string(),
            version,
        }),
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
        ConfigPublicationDecision::VersionExhausted => {
            Err(ResourceRegistryError::VersionExhausted { id: id.into() })
        }
        ConfigPublicationDecision::NonSuccessor => {
            Err(ResourceRegistryError::ConfigurationRejected {
                detail: format!(
                    "resource `{id}` config version must advance from {} to {}",
                    current.0,
                    current.0 + 1
                ),
            })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryStoreAggregate {
    definition: MemoryStoreDefinition,
    configs: BTreeMap<ConfigVersion, MemoryStoreConfigVersion>,
    /// Immutable creation receipt, committed in the same row as the identity.
    /// It is not user-editable metadata and survives profile/config changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    creation_request_sha256: Option<String>,
}

impl MemoryStoreAggregate {
    pub fn register(
        definition: MemoryStoreDefinition,
        initial_config: MemoryStoreConfigVersion,
    ) -> Result<Self, ResourceRegistryError> {
        if definition.id.as_str().trim().is_empty() || definition.workspace_id.trim().is_empty() {
            return Err(ResourceRegistryError::IdentityRejected {
                detail: "resource id and workspace id must be non-empty".into(),
            });
        }
        if definition.id != initial_config.memory_store_id
            || definition.current_config_version != ConfigVersion::INITIAL
            || initial_config.version != ConfigVersion::INITIAL
        {
            return Err(ResourceRegistryError::ConfigurationRejected {
                detail: "initial MemoryStore definition/config must agree at version 1".into(),
            });
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
            creation_request_sha256: None,
        })
    }

    pub fn validate_integrity(&self) -> Result<(), ResourceRegistryError> {
        if self.creation_request_sha256.as_ref().is_some_and(|hash| {
            hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            return Err(ResourceRegistryError::CorruptData(
                "MemoryStore creation receipt is invalid".into(),
            ));
        }
        validate_memory_store_integrity(&self.definition, &self.configs)
    }

    pub fn register_idempotent(
        mut definition: MemoryStoreDefinition,
        initial_config: MemoryStoreConfigVersion,
    ) -> Result<Self, ResourceRegistryError> {
        let timestamps = definition.timestamps;
        definition.timestamps = ResourceTimestamps::default();
        let request = serde_json::to_string(&(&definition, &initial_config)).map_err(|error| {
            ResourceRegistryError::EncodingFailure {
                detail: error.to_string(),
            }
        })?;
        definition.timestamps = timestamps;
        let mut aggregate = Self::register(definition, initial_config)?;
        aggregate.creation_request_sha256 = Some(crate::memory_sha256_hex(&request));
        Ok(aggregate)
    }

    #[must_use]
    pub fn is_creation_replay_of(&self, candidate: &Self) -> bool {
        self.creation_request_sha256.is_some()
            && self.creation_request_sha256 == candidate.creation_request_sha256
            && self.definition.id == candidate.definition.id
            && self.definition.workspace_id == candidate.definition.workspace_id
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
        ensure_resource_workspace(
            self.definition.id.as_str(),
            &self.definition.workspace_id,
            workspace_id,
        )?;
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
        verify_live_binding_facts(
            self.definition.id.as_str(),
            &self.definition.workspace_id,
            workspace_id,
            self.definition.state,
            version,
            self.config(version).is_some(),
        )
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
        ensure_resource_workspace(
            self.definition.id.as_str(),
            &self.definition.workspace_id,
            workspace_id,
        )?;
        if config.memory_store_id != self.definition.id {
            return Err(ResourceRegistryError::ConfigurationRejected {
                detail: "MemoryStore config identity does not match its aggregate".into(),
            });
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
        ensure_resource_workspace(
            self.definition.id.as_str(),
            &self.definition.workspace_id,
            workspace_id,
        )?;
        self.definition.state = state;
        self.definition.timestamps.transition_to(state, updated_at);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryAggregate {
    definition: RepositoryDefinition,
    configs: BTreeMap<ConfigVersion, RepositoryConfigVersion>,
    /// Atomic, immutable creation receipt; never exposed as editable metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    creation_request_sha256: Option<String>,
    /// Only an untouched staged registration may resume while suspended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    creation_preparation: Option<RepositoryPreparationPhase>,
}

impl RepositoryAggregate {
    pub fn register(
        definition: RepositoryDefinition,
        initial_config: RepositoryConfigVersion,
    ) -> Result<Self, ResourceRegistryError> {
        if definition.id.as_str().trim().is_empty() || definition.workspace_id.trim().is_empty() {
            return Err(ResourceRegistryError::IdentityRejected {
                detail: "resource id and workspace id must be non-empty".into(),
            });
        }
        if definition.id != initial_config.repository_id
            || definition.current_config_version != ConfigVersion::INITIAL
            || initial_config.version != ConfigVersion::INITIAL
        {
            return Err(ResourceRegistryError::ConfigurationRejected {
                detail: "initial Repository definition/config must agree at version 1".into(),
            });
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
            creation_request_sha256: None,
            creation_preparation: None,
        })
    }

    pub fn validate_integrity(&self) -> Result<(), ResourceRegistryError> {
        if self.creation_preparation.is_some() && self.creation_request_sha256.is_none() {
            return Err(ResourceRegistryError::CorruptData(
                "Repository preparation has no creation receipt".into(),
            ));
        }
        if self.creation_request_sha256.as_ref().is_some_and(|hash| {
            hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            return Err(ResourceRegistryError::CorruptData(
                "Repository creation receipt is invalid".into(),
            ));
        }
        validate_repository_integrity(&self.definition, &self.configs)
    }

    pub fn register_idempotent(
        mut definition: RepositoryDefinition,
        initial_config: RepositoryConfigVersion,
        mode: RepositoryRegistrationMode,
    ) -> Result<Self, ResourceRegistryError> {
        let timestamps = definition.timestamps;
        definition.timestamps = ResourceTimestamps::default();
        let request = serde_json::to_string(&(&definition, &initial_config)).map_err(|error| {
            ResourceRegistryError::EncodingFailure {
                detail: error.to_string(),
            }
        })?;
        definition.timestamps = timestamps;
        let mut aggregate = Self::register(definition, initial_config)?;
        aggregate.creation_request_sha256 = Some(crate::memory_sha256_hex(&request));
        if mode == RepositoryRegistrationMode::Preparation {
            aggregate.creation_preparation = Some(RepositoryPreparationPhase::Pending);
        }
        aggregate.validate_registration_replay(mode)?;
        Ok(aggregate)
    }

    #[must_use]
    pub fn is_creation_replay_of(&self, candidate: &Self) -> bool {
        self.creation_request_sha256.is_some()
            && self.creation_request_sha256 == candidate.creation_request_sha256
            && self.definition.id == candidate.definition.id
            && self.definition.workspace_id == candidate.definition.workspace_id
            && self.creation_preparation.is_some() == candidate.creation_preparation.is_some()
    }

    /// Lifecycle policy is separate from the immutable creation fingerprint.
    pub fn validate_registration_replay(
        &self,
        mode: RepositoryRegistrationMode,
    ) -> Result<(), ResourceRegistryError> {
        if mode == RepositoryRegistrationMode::Execution {
            return self
                .resolve_for_execution(&self.definition.workspace_id)
                .map(|_| ());
        }
        if self.creation_preparation.is_none()
            || self.definition.current_config_version != ConfigVersion::INITIAL
        {
            return Err(ResourceRegistryError::AlreadyRegistered(
                self.definition.id.to_string(),
            ));
        }
        if self.definition.state == ResourceState::Active
            || (self.definition.state == ResourceState::Suspended
                && self.creation_preparation == Some(RepositoryPreparationPhase::Pending))
        {
            Ok(())
        } else {
            Err(ResourceRegistryError::NotActive {
                id: self.definition.id.to_string(),
                state: self.definition.state,
            })
        }
    }

    #[must_use]
    pub fn preparation(&self, revision: AggregateRevision) -> Option<RepositoryPreparation> {
        if self.creation_preparation != Some(RepositoryPreparationPhase::Pending) {
            return None;
        }
        Some(RepositoryPreparation {
            revision,
            request_sha256: self.creation_request_sha256.clone()?,
        })
    }

    /// True applies one completion; false acknowledges that exact completion.
    pub fn complete_preparation(
        &mut self,
        workspace_id: &str,
        revision: AggregateRevision,
        proof: &RepositoryPreparation,
        target: ResourceState,
        updated_at: u64,
    ) -> Result<bool, ResourceRegistryError> {
        ensure_resource_workspace(
            self.definition.id.as_str(),
            &self.definition.workspace_id,
            workspace_id,
        )?;
        if target != ResourceState::Active
            || self.creation_request_sha256.as_ref() != Some(&proof.request_sha256)
        {
            return Err(ResourceRegistryError::ConcurrentModification(
                self.definition.id.to_string(),
            ));
        }
        if self.definition.state == ResourceState::Active
            && revision.get() > proof.revision.get()
            && self.creation_preparation
                == Some(RepositoryPreparationPhase::Completed {
                    prepared_revision: proof.revision.get(),
                })
        {
            return Ok(false);
        }
        if self.creation_preparation != Some(RepositoryPreparationPhase::Pending)
            || proof.revision != revision
        {
            return Err(ResourceRegistryError::ConcurrentModification(
                self.definition.id.to_string(),
            ));
        }
        self.definition.state = target;
        self.definition.timestamps.transition_to(target, updated_at);
        self.creation_preparation = Some(RepositoryPreparationPhase::Completed {
            prepared_revision: proof.revision.get(),
        });
        Ok(true)
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
        ensure_resource_workspace(
            self.definition.id.as_str(),
            &self.definition.workspace_id,
            workspace_id,
        )?;
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
        verify_live_binding_facts(
            self.definition.id.as_str(),
            &self.definition.workspace_id,
            workspace_id,
            self.definition.state,
            version,
            self.config(version).is_some(),
        )
    }

    pub fn publish_config(
        &mut self,
        workspace_id: &str,
        expected_current: ConfigVersion,
        config: RepositoryConfigVersion,
        updated_at: u64,
    ) -> Result<(), ResourceRegistryError> {
        ensure_resource_workspace(
            self.definition.id.as_str(),
            &self.definition.workspace_id,
            workspace_id,
        )?;
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
        if self.creation_preparation.is_some() {
            self.creation_preparation = Some(RepositoryPreparationPhase::Closed);
        }
        self.definition.timestamps.touch(updated_at);
        Ok(())
    }

    pub fn change_state(
        &mut self,
        workspace_id: &str,
        state: ResourceState,
        updated_at: u64,
    ) -> Result<(), ResourceRegistryError> {
        ensure_resource_workspace(
            self.definition.id.as_str(),
            &self.definition.workspace_id,
            workspace_id,
        )?;
        self.definition.state = state;
        if self.creation_preparation.is_some() {
            self.creation_preparation = Some(RepositoryPreparationPhase::Closed);
        }
        self.definition.timestamps.transition_to(state, updated_at);
        Ok(())
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
        Err(ResourceRegistryError::ConfigurationRejected {
            detail: "Repository checkout cannot select both branch and commit".into(),
        })
    } else if config
        .initial_branch
        .iter()
        .chain(config.initial_commit.iter())
        .any(|value| value.trim().is_empty())
    {
        Err(ResourceRegistryError::ConfigurationRejected {
            detail: "Repository checkout value must be non-empty".into(),
        })
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
    /// None is an explicit administrative transition. A staged completion
    /// carries the Registry's exact observation instead of reviving by state.
    pub preparation: Option<RepositoryPreparation>,
}

/// Query definitions and immutable configuration history.
#[async_trait::async_trait]
pub trait ResourceInventory: Send + Sync {
    async fn find_memory_store(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Option<MemoryStoreDefinition>, ResourceRegistryError>;
    async fn list_memory_stores(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<MemoryStoreDefinition>, ResourceRegistryError>;
    async fn find_memory_store_config(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Result<Option<MemoryStoreConfigVersion>, ResourceRegistryError>;
    async fn find_repository(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Option<RepositoryDefinition>, ResourceRegistryError>;
    async fn list_repositories(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<RepositoryDefinition>, ResourceRegistryError>;
    async fn find_repository_config(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Result<Option<RepositoryConfigVersion>, ResourceRegistryError>;
}

/// Register resources, publish configuration, and change lifecycle state.
#[async_trait::async_trait]
pub trait ResourceAdministration: Send + Sync {
    async fn register_memory_store(
        &self,
        command: RegisterMemoryStore,
    ) -> Result<(), ResourceRegistryError>;
    /// Atomic registration with a retained request receipt. Exact retries return
    /// the current active definition; changed payloads and retired stores fail.
    async fn register_memory_store_idempotent(
        &self,
        command: RegisterMemoryStore,
    ) -> Result<MemoryStoreDefinition, ResourceRegistryError>;
    async fn update_memory_store_profile(
        &self,
        command: UpdateMemoryStoreProfile,
    ) -> Result<(), ResourceRegistryError>;
    async fn publish_memory_store_config(
        &self,
        command: PublishMemoryStoreConfig,
    ) -> Result<(), ResourceRegistryError>;
    async fn change_memory_store_state(
        &self,
        command: ChangeMemoryStoreState,
    ) -> Result<(), ResourceRegistryError>;
    async fn register_repository(
        &self,
        command: RegisterRepository,
    ) -> Result<(), ResourceRegistryError>;
    /// Return the Registry's exact creation outcome under the explicit
    /// execution/preparation policy. Neither lane overwrites configuration or
    /// revives a resource after an administrative lifecycle transition.
    async fn register_repository_idempotent(
        &self,
        command: RegisterRepository,
        mode: RepositoryRegistrationMode,
    ) -> Result<RepositoryRegistration, ResourceRegistryError>;
    async fn publish_repository_config(
        &self,
        command: PublishRepositoryConfig,
    ) -> Result<(), ResourceRegistryError>;
    async fn change_repository_state(
        &self,
        command: ChangeRepositoryState,
    ) -> Result<(), ResourceRegistryError>;
}

/// Resolve the current active configuration selected for execution.
#[async_trait::async_trait]
pub trait ExecutionResourceResolver: Send + Sync {
    async fn resolve_memory_store(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<MemoryStoreConfigVersion, ResourceRegistryError>;
    async fn resolve_repository(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<RepositoryConfigVersion, ResourceRegistryError>;
}

/// Verify that a frozen binding remains owned, active, and intact.
#[async_trait::async_trait]
pub trait LiveResourceBindingVerifier: Send + Sync {
    async fn verify_memory_binding(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Result<(), ResourceRegistryError>;
    async fn verify_repository_binding(
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
#[async_trait::async_trait]
pub trait ResourceRegistryRepository: Send + Sync {
    async fn load_memory_store(
        &self,
        id: &str,
    ) -> Result<Option<Stored<MemoryStoreAggregate>>, RegistryRepositoryError>;
    async fn insert_memory_store(
        &self,
        aggregate: &MemoryStoreAggregate,
    ) -> Result<InsertOutcome, RegistryRepositoryError>;
    async fn replace_memory_store(
        &self,
        expected_revision: AggregateRevision,
        aggregate: &MemoryStoreAggregate,
    ) -> Result<ReplaceOutcome, RegistryRepositoryError>;
    async fn list_memory_stores(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<Stored<MemoryStoreAggregate>>, RegistryRepositoryError>;
    async fn load_repository(
        &self,
        id: &str,
    ) -> Result<Option<Stored<RepositoryAggregate>>, RegistryRepositoryError>;
    async fn list_repositories(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<Stored<RepositoryAggregate>>, RegistryRepositoryError>;
    async fn insert_repository(
        &self,
        aggregate: &RepositoryAggregate,
    ) -> Result<InsertOutcome, RegistryRepositoryError>;
    async fn replace_repository(
        &self,
        expected_revision: AggregateRevision,
        aggregate: &RepositoryAggregate,
    ) -> Result<ReplaceOutcome, RegistryRepositoryError>;
}

#[cfg(test)]
mod registry_tests {
    use super::*;

    #[test]
    fn repository_creation_receipts_preserve_legacy_compatibility_and_validate_integrity() {
        let definition = RepositoryDefinition {
            id: "repository".into(),
            workspace_id: "workspace".into(),
            name: "Repository".into(),
            description: String::new(),
            metadata: Default::default(),
            state: ResourceState::Active,
            current_config_version: ConfigVersion::INITIAL,
            timestamps: Default::default(),
        };
        let config = RepositoryConfigVersion {
            repository_id: definition.id.clone(),
            version: ConfigVersion::INITIAL,
            remote_url: "https://github.com/example/repository.git".into(),
            credential_binding: None,
            initial_branch: None,
            initial_commit: None,
            clone_policy: ClonePolicy::default(),
        };
        let ordinary = RepositoryAggregate::register(definition.clone(), config.clone()).unwrap();
        let wire = serde_json::to_value(&ordinary).unwrap();
        assert!(wire.get("creation_request_sha256").is_none());
        let legacy: RepositoryAggregate = serde_json::from_value(wire).unwrap();
        legacy.validate_integrity().unwrap();
        let keyed = RepositoryAggregate::register_idempotent(
            definition,
            config,
            RepositoryRegistrationMode::Execution,
        )
        .unwrap();
        assert!(!keyed.is_creation_replay_of(&legacy));
        let mut wire = serde_json::to_value(&keyed).unwrap();
        let restored: RepositoryAggregate = serde_json::from_value(wire.clone()).unwrap();
        assert!(keyed.is_creation_replay_of(&restored));
        wire["creation_request_sha256"] = serde_json::json!("not-a-hash");
        let corrupt: RepositoryAggregate = serde_json::from_value(wire).unwrap();
        assert!(matches!(
            corrupt.validate_integrity(),
            Err(ResourceRegistryError::CorruptData(_))
        ));
    }

    #[test]
    fn memory_creation_receipt_is_optional_for_legacy_rows_but_validated_when_present() {
        let definition = MemoryStoreDefinition {
            id: "memory-a".into(),
            workspace_id: "workspace-a".into(),
            name: "Memory".into(),
            description: String::new(),
            metadata: Default::default(),
            state: ResourceState::Active,
            current_config_version: ConfigVersion::INITIAL,
            timestamps: Default::default(),
        };
        let config = MemoryStoreConfigVersion {
            memory_store_id: definition.id.clone(),
            version: ConfigVersion::INITIAL,
            retention_policy: Default::default(),
        };
        let ordinary = MemoryStoreAggregate::register(definition.clone(), config.clone()).unwrap();
        let wire = serde_json::to_value(&ordinary).unwrap();
        assert!(wire.get("creation_request_sha256").is_none());
        let legacy: MemoryStoreAggregate = serde_json::from_value(wire).unwrap();
        legacy.validate_integrity().unwrap();
        let keyed = MemoryStoreAggregate::register_idempotent(definition, config).unwrap();
        assert!(
            !legacy.is_creation_replay_of(&keyed),
            "a request cannot adopt an unrelated legacy row"
        );
        let mut wire = serde_json::to_value(&keyed).unwrap();
        wire["creation_request_sha256"] = serde_json::json!("invalid");
        let corrupt: MemoryStoreAggregate = serde_json::from_value(wire).unwrap();
        assert!(matches!(
            corrupt.validate_integrity(),
            Err(ResourceRegistryError::CorruptData(_))
        ));
    }

    #[test]
    fn live_binding_decision_table_is_total_exact_and_fail_closed() {
        // Cause/effect graph: C1 the requested Workspace matches the resource;
        // C2 lifecycle is Active; C3 the frozen config exists. E1 accepts only
        // C1+C2+C3; E2 a C1 failure is NotFound; E3 after C1, a C2 failure is
        // NotActive; E4 after C1+C2, a C3 failure is ConfigNotFound.
        // Constraint: error precedence hides cross-Workspace existence before
        // exposing lifecycle or config facts. The 2*4*2 table below exercises
        // every rule for both shared MemoryStore and Repository admission.
        let states = [
            ResourceState::Active,
            ResourceState::Suspended,
            ResourceState::Archived,
            ResourceState::Deleted,
        ];
        for workspace_matches in [false, true] {
            for state in states {
                for config_exists in [false, true] {
                    let expected = if !workspace_matches {
                        LiveBindingAdmission::WorkspaceMismatch
                    } else if state != ResourceState::Active {
                        LiveBindingAdmission::NotActive
                    } else if !config_exists {
                        LiveBindingAdmission::ConfigMissing
                    } else {
                        LiveBindingAdmission::Accept
                    };
                    assert_eq!(
                        live_binding_admission(workspace_matches, state, config_exists),
                        expected
                    );

                    let requested_workspace = if workspace_matches { "ws-a" } else { "ws-b" };
                    assert_eq!(
                        ensure_resource_workspace("resource-a", "ws-a", requested_workspace)
                            .is_ok(),
                        workspace_matches
                    );
                    let result = verify_live_binding_facts(
                        "resource-a",
                        "ws-a",
                        requested_workspace,
                        state,
                        ConfigVersion(7),
                        config_exists,
                    );
                    match expected {
                        LiveBindingAdmission::Accept => assert!(result.is_ok()),
                        LiveBindingAdmission::WorkspaceMismatch => assert!(matches!(
                            result,
                            Err(ResourceRegistryError::NotFound(id)) if id == "resource-a"
                        )),
                        LiveBindingAdmission::NotActive => assert!(matches!(
                            result,
                            Err(ResourceRegistryError::NotActive { id, state: actual })
                                if id == "resource-a" && actual == state
                        )),
                        LiveBindingAdmission::ConfigMissing => assert!(matches!(
                            result,
                            Err(ResourceRegistryError::ConfigNotFound { id, version })
                                if id == "resource-a" && version == ConfigVersion(7)
                        )),
                    }
                }
            }
        }
    }
}

#[cfg(kani)]
mod registry_proofs {
    use super::*;

    /// @obligation resource_catalog.config_publication_is_exact_and_never_wraps
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

    /// @obligation resource_catalog.config_publication_is_exact_and_never_wraps
    #[kani::proof]
    fn exhausted_resource_config_versions_fail_closed() {
        let next = ConfigVersion(kani::any());
        assert_eq!(
            config_publication_decision(ConfigVersion(u64::MAX), ConfigVersion(u64::MAX), next,),
            ConfigPublicationDecision::VersionExhausted
        );
    }

    /// @obligation resource_catalog.aggregate_revision_is_strictly_monotonic_and_never_wraps
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

    /// @obligation resource_catalog.lifecycle_timestamp_projection_is_total_and_exact
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

#[cfg(test)]
mod exhaustive {
    use super::*;

    const EVERY_RESOURCE_STATE: [ResourceState; 4] = [
        ResourceState::Active,
        ResourceState::Suspended,
        ResourceState::Archived,
        ResourceState::Deleted,
    ];

    const fn listed_resource_state(state: ResourceState) -> usize {
        match state {
            ResourceState::Active => 0,
            ResourceState::Suspended => 1,
            ResourceState::Archived => 2,
            ResourceState::Deleted => 3,
        }
    }

    #[test]
    fn every_resource_state_is_listed_once() {
        for (index, state) in EVERY_RESOURCE_STATE.into_iter().enumerate() {
            assert_eq!(listed_resource_state(state), index);
        }
    }

    /// @exhaustive bool(2) x ResourceState(4) x bool(2) = 16
    /// @obligation session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin
    #[test]
    fn live_resource_binding_requires_exact_scope_active_state_and_existing_pin() {
        for workspace_matches in [false, true] {
            for state in EVERY_RESOURCE_STATE {
                for config_exists in [false, true] {
                    let decision = live_binding_admission(workspace_matches, state, config_exists);

                    assert_eq!(
                        decision == LiveBindingAdmission::Accept,
                        workspace_matches
                            && matches!(state, ResourceState::Active)
                            && config_exists
                    );
                    if !workspace_matches {
                        assert_eq!(decision, LiveBindingAdmission::WorkspaceMismatch);
                    } else if !matches!(state, ResourceState::Active) {
                        assert_eq!(decision, LiveBindingAdmission::NotActive);
                    } else if !config_exists {
                        assert_eq!(decision, LiveBindingAdmission::ConfigMissing);
                    }
                }
            }
        }
    }
}
