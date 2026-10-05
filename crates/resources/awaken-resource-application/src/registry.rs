// @formal entrypoint session_resources.dispatch_activation_requires_capability_scope_live_state_and_exact_pin
//! Canonical Resource Registry use cases.
//!
//! Database adapters store aggregates; this application is the sole production
//! implementation of registration, inventory, execution resolution, binding
//! verification, publication, and lifecycle semantics.

use std::sync::Arc;

use awaken_time::WallClock;

use awaken_resource_contract::{
    ChangeMemoryStoreState, ChangeRepositoryState, ConfigVersion, ExecutionResourceResolver,
    InsertOutcome, LiveResourceBindingVerifier, MemoryStoreAggregate, MemoryStoreConfigVersion,
    MemoryStoreDefinition, PublishMemoryStoreConfig, PublishRepositoryConfig, RegisterMemoryStore,
    RegisterRepository, RegistryRepositoryError, ReplaceOutcome, RepositoryAggregate,
    RepositoryConfigVersion, RepositoryDefinition, ResourceAdministration, ResourceInventory,
    ResourceRegistryError, ResourceRegistryRepository, ResourceState, Stored,
    UpdateMemoryStoreProfile,
};

pub struct RegistryApplication {
    repository: Arc<dyn ResourceRegistryRepository>,
    /// The Resources context's one clock: every profile, publication and state
    /// timestamp this application commits is its reading, the same clock the
    /// registration commands of that context stamp creation with.
    clock: Arc<dyn WallClock>,
}

impl RegistryApplication {
    #[must_use]
    pub fn new(repository: Arc<dyn ResourceRegistryRepository>, clock: Arc<dyn WallClock>) -> Self {
        Self { repository, clock }
    }

    async fn memory(
        &self,
        id: &str,
    ) -> Result<Option<Stored<MemoryStoreAggregate>>, ResourceRegistryError> {
        self.repository
            .load_memory_store(id)
            .await
            .map_err(registry_error)
    }

    async fn repository(
        &self,
        id: &str,
    ) -> Result<Option<Stored<RepositoryAggregate>>, ResourceRegistryError> {
        self.repository
            .load_repository(id)
            .await
            .map_err(registry_error)
    }

    async fn replace_memory(
        &self,
        id: &str,
        stored: Stored<MemoryStoreAggregate>,
    ) -> Result<(), ResourceRegistryError> {
        match self
            .repository
            .replace_memory_store(stored.revision, &stored.aggregate)
            .await
            .map_err(registry_error)?
        {
            ReplaceOutcome::Replaced { .. } => Ok(()),
            ReplaceOutcome::ConcurrentModification => {
                Err(ResourceRegistryError::ConcurrentModification(id.into()))
            }
        }
    }

    async fn replace_repository(
        &self,
        id: &str,
        stored: Stored<RepositoryAggregate>,
    ) -> Result<(), ResourceRegistryError> {
        match self
            .repository
            .replace_repository(stored.revision, &stored.aggregate)
            .await
            .map_err(registry_error)?
        {
            ReplaceOutcome::Replaced { .. } => Ok(()),
            ReplaceOutcome::ConcurrentModification => {
                Err(ResourceRegistryError::ConcurrentModification(id.into()))
            }
        }
    }
}

fn registry_error(error: RegistryRepositoryError) -> ResourceRegistryError {
    match error {
        RegistryRepositoryError::AlreadyExists(id) => ResourceRegistryError::AlreadyRegistered(id),
        RegistryRepositoryError::NotFound(id) => ResourceRegistryError::NotFound(id),
        RegistryRepositoryError::ConcurrentModification(id) => {
            ResourceRegistryError::ConcurrentModification(id)
        }
        RegistryRepositoryError::CorruptData(message) => {
            ResourceRegistryError::CorruptData(message)
        }
        RegistryRepositoryError::Indeterminate { detail: message } => {
            ResourceRegistryError::Indeterminate { detail: message }
        }
    }
}

#[async_trait::async_trait]
impl ResourceInventory for RegistryApplication {
    async fn find_memory_store(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Option<MemoryStoreDefinition>, ResourceRegistryError> {
        Ok(self.memory(id).await?.and_then(|stored| {
            (stored.aggregate.definition().workspace_id == workspace_id)
                .then(|| stored.aggregate.into_definition())
        }))
    }

    async fn list_memory_stores(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<MemoryStoreDefinition>, ResourceRegistryError> {
        let mut definitions = self
            .repository
            .list_memory_stores(workspace_id)
            .await
            .map_err(registry_error)?
            .into_iter()
            .filter_map(|stored| {
                (stored.aggregate.definition().workspace_id == workspace_id
                    && stored.aggregate.definition().state != ResourceState::Deleted)
                    .then(|| stored.aggregate.into_definition())
            })
            .collect::<Vec<_>>();
        definitions.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(definitions)
    }

    async fn find_memory_store_config(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Result<Option<MemoryStoreConfigVersion>, ResourceRegistryError> {
        Ok(self.memory(id).await?.and_then(|stored| {
            (stored.aggregate.definition().workspace_id == workspace_id)
                .then(|| stored.aggregate.into_config(version))
                .flatten()
        }))
    }

    async fn find_repository(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Option<RepositoryDefinition>, ResourceRegistryError> {
        Ok(self.repository(id).await?.and_then(|stored| {
            (stored.aggregate.definition().workspace_id == workspace_id)
                .then(|| stored.aggregate.into_definition())
        }))
    }

    async fn find_repository_config(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Result<Option<RepositoryConfigVersion>, ResourceRegistryError> {
        Ok(self.repository(id).await?.and_then(|stored| {
            (stored.aggregate.definition().workspace_id == workspace_id)
                .then(|| stored.aggregate.into_config(version))
                .flatten()
        }))
    }

    async fn list_repositories(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<RepositoryDefinition>, ResourceRegistryError> {
        let mut definitions = self
            .repository
            .list_repositories(workspace_id)
            .await
            .map_err(registry_error)?
            .into_iter()
            .filter_map(|stored| {
                (stored.aggregate.definition().workspace_id == workspace_id
                    && stored.aggregate.definition().state != ResourceState::Deleted)
                    .then(|| stored.aggregate.into_definition())
            })
            .collect::<Vec<_>>();
        definitions.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(definitions)
    }
}

#[async_trait::async_trait]
impl ResourceAdministration for RegistryApplication {
    async fn register_memory_store_idempotent(
        &self,
        command: RegisterMemoryStore,
    ) -> Result<MemoryStoreDefinition, ResourceRegistryError> {
        let candidate =
            MemoryStoreAggregate::register_idempotent(command.definition, command.initial_config)?;
        match self
            .repository
            .insert_memory_store(&candidate)
            .await
            .map_err(registry_error)?
        {
            InsertOutcome::Inserted => Ok(candidate.into_definition()),
            InsertOutcome::AlreadyRegistered => {
                let id = candidate.definition().id.as_str();
                let stored = self
                    .memory(id)
                    .await?
                    .ok_or_else(|| ResourceRegistryError::NotFound(id.into()))?;
                if !stored.aggregate.is_creation_replay_of(&candidate) {
                    return Err(ResourceRegistryError::AlreadyRegistered(id.into()));
                }
                stored
                    .aggregate
                    .resolve_for_execution(&candidate.definition().workspace_id)?;
                Ok(stored.aggregate.into_definition())
            }
        }
    }

    async fn register_memory_store(
        &self,
        command: RegisterMemoryStore,
    ) -> Result<(), ResourceRegistryError> {
        let aggregate = MemoryStoreAggregate::register(command.definition, command.initial_config)?;
        match self
            .repository
            .insert_memory_store(&aggregate)
            .await
            .map_err(registry_error)?
        {
            InsertOutcome::Inserted => Ok(()),
            InsertOutcome::AlreadyRegistered => Err(ResourceRegistryError::AlreadyRegistered(
                aggregate.definition().id.to_string(),
            )),
        }
    }

    async fn update_memory_store_profile(
        &self,
        command: UpdateMemoryStoreProfile,
    ) -> Result<(), ResourceRegistryError> {
        let id = command.id.to_string();
        let Some(mut stored) = self
            .memory(&id)
            .await?
            .filter(|stored| stored.aggregate.definition().workspace_id == command.workspace_id)
        else {
            return Err(ResourceRegistryError::NotFound(id));
        };
        stored.aggregate.update_profile(
            command.name,
            command.description,
            command.metadata,
            self.clock.now().as_unix_nanos_u64(),
        );
        self.replace_memory(&id, stored).await
    }

    async fn publish_memory_store_config(
        &self,
        command: PublishMemoryStoreConfig,
    ) -> Result<(), ResourceRegistryError> {
        let id = command.config.memory_store_id.to_string();
        let Some(mut stored) = self.memory(&id).await? else {
            return Err(ResourceRegistryError::NotFound(id));
        };
        stored.aggregate.publish_config(
            &command.workspace_id,
            command.expected_current,
            command.config,
            self.clock.now().as_unix_nanos_u64(),
        )?;
        self.replace_memory(&id, stored).await
    }

    async fn change_memory_store_state(
        &self,
        command: ChangeMemoryStoreState,
    ) -> Result<(), ResourceRegistryError> {
        let id = command.id.to_string();
        let Some(mut stored) = self.memory(&id).await? else {
            return Err(ResourceRegistryError::NotFound(id));
        };
        stored.aggregate.change_state(
            &command.workspace_id,
            command.state,
            self.clock.now().as_unix_nanos_u64(),
        )?;
        self.replace_memory(&id, stored).await
    }

    async fn register_repository(
        &self,
        command: RegisterRepository,
    ) -> Result<(), ResourceRegistryError> {
        let aggregate = RepositoryAggregate::register(command.definition, command.initial_config)?;
        match self
            .repository
            .insert_repository(&aggregate)
            .await
            .map_err(registry_error)?
        {
            InsertOutcome::Inserted => Ok(()),
            InsertOutcome::AlreadyRegistered => Err(ResourceRegistryError::AlreadyRegistered(
                aggregate.definition().id.to_string(),
            )),
        }
    }

    async fn register_repository_idempotent(
        &self,
        command: RegisterRepository,
    ) -> Result<RepositoryDefinition, ResourceRegistryError> {
        let candidate =
            RepositoryAggregate::register_idempotent(command.definition, command.initial_config)?;
        match self
            .repository
            .insert_repository(&candidate)
            .await
            .map_err(registry_error)?
        {
            InsertOutcome::Inserted => Ok(candidate.into_definition()),
            InsertOutcome::AlreadyRegistered => {
                let id = candidate.definition().id.as_str();
                let stored = self
                    .repository(id)
                    .await?
                    .ok_or_else(|| ResourceRegistryError::NotFound(id.into()))?;
                if !stored.aggregate.is_creation_replay_of(&candidate) {
                    return Err(ResourceRegistryError::AlreadyRegistered(id.into()));
                }
                stored
                    .aggregate
                    .resolve_for_execution(&candidate.definition().workspace_id)?;
                Ok(stored.aggregate.into_definition())
            }
        }
    }

    async fn publish_repository_config(
        &self,
        command: PublishRepositoryConfig,
    ) -> Result<(), ResourceRegistryError> {
        let id = command.config.repository_id.to_string();
        let Some(mut stored) = self.repository(&id).await? else {
            return Err(ResourceRegistryError::NotFound(id));
        };
        stored.aggregate.publish_config(
            &command.workspace_id,
            command.expected_current,
            command.config,
            self.clock.now().as_unix_nanos_u64(),
        )?;
        self.replace_repository(&id, stored).await
    }

    async fn change_repository_state(
        &self,
        command: ChangeRepositoryState,
    ) -> Result<(), ResourceRegistryError> {
        let id = command.id.to_string();
        let Some(mut stored) = self.repository(&id).await? else {
            return Err(ResourceRegistryError::NotFound(id));
        };
        stored.aggregate.change_state(
            &command.workspace_id,
            command.state,
            self.clock.now().as_unix_nanos_u64(),
        )?;
        self.replace_repository(&id, stored).await
    }
}

#[async_trait::async_trait]
impl ExecutionResourceResolver for RegistryApplication {
    async fn resolve_memory_store(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<MemoryStoreConfigVersion, ResourceRegistryError> {
        self.memory(id)
            .await?
            .ok_or_else(|| ResourceRegistryError::NotFound(id.into()))?
            .aggregate
            .resolve_for_execution(workspace_id)
    }

    async fn resolve_repository(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<RepositoryConfigVersion, ResourceRegistryError> {
        self.repository(id)
            .await?
            .ok_or_else(|| ResourceRegistryError::NotFound(id.into()))?
            .aggregate
            .resolve_for_execution(workspace_id)
    }
}

#[async_trait::async_trait]
impl LiveResourceBindingVerifier for RegistryApplication {
    async fn verify_memory_binding(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Result<(), ResourceRegistryError> {
        self.memory(id)
            .await?
            .ok_or_else(|| ResourceRegistryError::NotFound(id.into()))?
            .aggregate
            .verify_live_binding(workspace_id, version)
    }

    async fn verify_repository_binding(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Result<(), ResourceRegistryError> {
        self.repository(id)
            .await?
            .ok_or_else(|| ResourceRegistryError::NotFound(id.into()))?
            .aggregate
            .verify_live_binding(workspace_id, version)
    }
}

#[cfg(test)]
mod tests {
    use awaken_resource_contract::{
        ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition, RegisterMemoryStore,
        ResourceAdministration as _, ResourceInventory as _, ResourceState, RetentionPolicy,
        UpdateMemoryStoreProfile,
    };

    use super::*;

    #[test]
    fn repository_outcomes_keep_their_cause_at_the_application_boundary() {
        // C1 no definitive repository receipt => indeterminate, never rejected;
        // C2 corrupt data => corruption; C3 confirmed absence => NotFound.
        // Equal diagnostics prove this projection does not inspect message text.
        assert!(matches!(
            registry_error(RegistryRepositoryError::Indeterminate {
                detail: "same".into()
            }),
            ResourceRegistryError::Indeterminate { .. }
        ));
        assert!(matches!(
            registry_error(RegistryRepositoryError::CorruptData("same".into())),
            ResourceRegistryError::CorruptData(_)
        ));
        assert!(matches!(
            registry_error(RegistryRepositoryError::NotFound("same".into())),
            ResourceRegistryError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn application_owns_profile_timestamps_with_an_injected_clock() {
        // Responsibility test: driving adapters supply intent but never time.
        // A profile mutation commits the application clock exactly once; the
        // repository only preserves that already-decided aggregate state.
        let repository = Arc::new(
            awaken_resource_store::SqliteResourceStore::in_memory()
                .expect("open test Registry repository"),
        );
        let registry = RegistryApplication::new(
            repository,
            Arc::new(awaken_lease_testkit::FixedClock::at(
                awaken_time::UtcTime::from_unix_nanos(42),
            )),
        );
        registry
            .register_memory_store(RegisterMemoryStore {
                definition: MemoryStoreDefinition {
                    id: "memory-clock".into(),
                    workspace_id: "workspace".into(),
                    name: "Memory".into(),
                    description: String::new(),
                    metadata: Default::default(),
                    state: ResourceState::Active,
                    current_config_version: ConfigVersion::INITIAL,
                    timestamps: Default::default(),
                },
                initial_config: MemoryStoreConfigVersion {
                    memory_store_id: "memory-clock".into(),
                    version: ConfigVersion::INITIAL,
                    retention_policy: RetentionPolicy::default(),
                },
            })
            .await
            .expect("register clock fixture");
        registry
            .update_memory_store_profile(UpdateMemoryStoreProfile {
                workspace_id: "workspace".into(),
                id: "memory-clock".into(),
                name: "Renamed".into(),
                description: String::new(),
                metadata: Default::default(),
            })
            .await
            .expect("update profile");
        let definition = registry
            .find_memory_store("workspace", "memory-clock")
            .await
            .expect("read profile")
            .expect("profile exists");
        assert_eq!(definition.name, "Renamed");
        assert_eq!(definition.timestamps.updated_unix_nanos, 42);
    }

    #[tokio::test]
    async fn inventory_retains_archived_definitions_but_never_deleted_tombstones() {
        // Cause/effect graph: lifecycle is independent from inventory identity.
        // Archived definitions remain queryable by management callers, while
        // Deleted is a terminal tombstone and cannot re-enter a public list.
        // Execution safety is proved separately by resolve_for_execution.
        let repository = Arc::new(
            awaken_resource_store::SqliteResourceStore::in_memory()
                .expect("open test Registry repository"),
        );
        let registry = RegistryApplication::new(repository, awaken_time::system_clock());
        for (id, state) in [
            ("active", ResourceState::Active),
            ("archived", ResourceState::Archived),
            ("deleted", ResourceState::Deleted),
        ] {
            registry
                .register_memory_store(RegisterMemoryStore {
                    definition: MemoryStoreDefinition {
                        id: id.into(),
                        workspace_id: "workspace".into(),
                        name: id.into(),
                        description: String::new(),
                        metadata: Default::default(),
                        state,
                        current_config_version: ConfigVersion::INITIAL,
                        timestamps: Default::default(),
                    },
                    initial_config: MemoryStoreConfigVersion {
                        memory_store_id: id.into(),
                        version: ConfigVersion::INITIAL,
                        retention_policy: RetentionPolicy::default(),
                    },
                })
                .await
                .expect("register lifecycle fixture");
        }

        let definitions = registry
            .list_memory_stores("workspace")
            .await
            .expect("list inventory");
        assert_eq!(
            definitions
                .iter()
                .map(|definition| definition.id.as_str())
                .collect::<Vec<_>>(),
            ["active", "archived"]
        );
        assert!(matches!(
            registry.resolve_memory_store("workspace", "archived").await,
            Err(awaken_resource_contract::ResourceRegistryError::NotActive { .. })
        ));
    }
}
