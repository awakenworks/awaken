//! Canonical Resource Registry use cases.
//!
//! Database adapters store aggregates; this application is the sole production
//! implementation of registration, inventory, execution resolution, binding
//! verification, publication, and lifecycle semantics.

use std::sync::Arc;

use awaken_resource_contract::{
    ChangeMemoryStoreState, ChangeRepositoryState, ConfigVersion, ExecutionResourceResolver,
    InsertOutcome, LiveResourceBindingVerifier, MemoryStoreAggregate, MemoryStoreConfigVersion,
    MemoryStoreDefinition, PublishMemoryStoreConfig, PublishRepositoryConfig, RegisterMemoryStore,
    RegisterRepository, RegistryRepositoryError, ReplaceOutcome, RepositoryAggregate,
    RepositoryConfigVersion, RepositoryDefinition, ResourceAdministration, ResourceInventory,
    ResourceRegistryError, ResourceRegistryRepository, ResourceState, Stored,
    UpdateMemoryStoreProfile,
};

pub trait RegistryClock: Send + Sync {
    fn now_unix_nanos(&self) -> u64;
}

#[derive(Debug, Default)]
pub struct SystemRegistryClock;

impl RegistryClock for SystemRegistryClock {
    fn now_unix_nanos(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64)
            .unwrap_or_default()
    }
}

pub struct RegistryApplication {
    repository: Arc<dyn ResourceRegistryRepository>,
    clock: Arc<dyn RegistryClock>,
}

impl RegistryApplication {
    #[must_use]
    pub fn new(repository: Arc<dyn ResourceRegistryRepository>) -> Self {
        Self::with_clock(repository, Arc::new(SystemRegistryClock))
    }

    #[must_use]
    pub fn with_clock(
        repository: Arc<dyn ResourceRegistryRepository>,
        clock: Arc<dyn RegistryClock>,
    ) -> Self {
        Self { repository, clock }
    }

    fn memory(
        &self,
        id: &str,
    ) -> Result<Option<Stored<MemoryStoreAggregate>>, ResourceRegistryError> {
        self.repository
            .load_memory_store(id)
            .map_err(registry_error)
    }

    fn repository(
        &self,
        id: &str,
    ) -> Result<Option<Stored<RepositoryAggregate>>, ResourceRegistryError> {
        self.repository.load_repository(id).map_err(registry_error)
    }

    fn replace_memory(
        &self,
        id: &str,
        stored: Stored<MemoryStoreAggregate>,
    ) -> Result<(), ResourceRegistryError> {
        match self
            .repository
            .replace_memory_store(stored.revision, &stored.aggregate)
            .map_err(registry_error)?
        {
            ReplaceOutcome::Replaced { .. } => Ok(()),
            ReplaceOutcome::ConcurrentModification => {
                Err(ResourceRegistryError::ConcurrentModification(id.into()))
            }
        }
    }

    fn replace_repository(
        &self,
        id: &str,
        stored: Stored<RepositoryAggregate>,
    ) -> Result<(), ResourceRegistryError> {
        match self
            .repository
            .replace_repository(stored.revision, &stored.aggregate)
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
        RegistryRepositoryError::Unavailable(message) => {
            ResourceRegistryError::Unavailable(message)
        }
    }
}

impl ResourceInventory for RegistryApplication {
    fn find_memory_store(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Option<MemoryStoreDefinition>, ResourceRegistryError> {
        Ok(self.memory(id)?.and_then(|stored| {
            (stored.aggregate.definition().workspace_id == workspace_id)
                .then(|| stored.aggregate.into_definition())
        }))
    }

    fn list_memory_stores(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<MemoryStoreDefinition>, ResourceRegistryError> {
        let mut definitions = self
            .repository
            .list_memory_stores(workspace_id)
            .map_err(registry_error)?
            .into_iter()
            .filter_map(|stored| {
                (stored.aggregate.definition().workspace_id == workspace_id
                    && !matches!(
                        stored.aggregate.definition().state,
                        ResourceState::Archived | ResourceState::Deleted
                    ))
                .then(|| stored.aggregate.into_definition())
            })
            .collect::<Vec<_>>();
        definitions.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(definitions)
    }

    fn find_memory_store_config(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Result<Option<MemoryStoreConfigVersion>, ResourceRegistryError> {
        Ok(self.memory(id)?.and_then(|stored| {
            (stored.aggregate.definition().workspace_id == workspace_id)
                .then(|| stored.aggregate.into_config(version))
                .flatten()
        }))
    }

    fn find_repository(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Option<RepositoryDefinition>, ResourceRegistryError> {
        Ok(self.repository(id)?.and_then(|stored| {
            (stored.aggregate.definition().workspace_id == workspace_id)
                .then(|| stored.aggregate.into_definition())
        }))
    }

    fn find_repository_config(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Result<Option<RepositoryConfigVersion>, ResourceRegistryError> {
        Ok(self.repository(id)?.and_then(|stored| {
            (stored.aggregate.definition().workspace_id == workspace_id)
                .then(|| stored.aggregate.into_config(version))
                .flatten()
        }))
    }
}

impl ResourceAdministration for RegistryApplication {
    fn register_memory_store(
        &self,
        command: RegisterMemoryStore,
    ) -> Result<(), ResourceRegistryError> {
        let aggregate = MemoryStoreAggregate::register(command.definition, command.initial_config)?;
        match self
            .repository
            .insert_memory_store(&aggregate)
            .map_err(registry_error)?
        {
            InsertOutcome::Inserted => Ok(()),
            InsertOutcome::AlreadyRegistered => Err(ResourceRegistryError::AlreadyRegistered(
                aggregate.definition().id.to_string(),
            )),
        }
    }

    fn update_memory_store_profile(
        &self,
        command: UpdateMemoryStoreProfile,
    ) -> Result<(), ResourceRegistryError> {
        let id = command.id.to_string();
        let Some(mut stored) = self
            .memory(&id)?
            .filter(|stored| stored.aggregate.definition().workspace_id == command.workspace_id)
        else {
            return Err(ResourceRegistryError::NotFound(id));
        };
        stored.aggregate.update_profile(
            command.name,
            command.description,
            command.metadata,
            self.clock.now_unix_nanos(),
        );
        self.replace_memory(&id, stored)
    }

    fn publish_memory_store_config(
        &self,
        command: PublishMemoryStoreConfig,
    ) -> Result<(), ResourceRegistryError> {
        let id = command.config.memory_store_id.to_string();
        let Some(mut stored) = self.memory(&id)? else {
            return Err(ResourceRegistryError::NotFound(id));
        };
        stored.aggregate.publish_config(
            &command.workspace_id,
            command.expected_current,
            command.config,
            self.clock.now_unix_nanos(),
        )?;
        self.replace_memory(&id, stored)
    }

    fn change_memory_store_state(
        &self,
        command: ChangeMemoryStoreState,
    ) -> Result<(), ResourceRegistryError> {
        let id = command.id.to_string();
        let Some(mut stored) = self.memory(&id)? else {
            return Err(ResourceRegistryError::NotFound(id));
        };
        stored.aggregate.change_state(
            &command.workspace_id,
            command.state,
            self.clock.now_unix_nanos(),
        )?;
        self.replace_memory(&id, stored)
    }

    fn register_repository(
        &self,
        command: RegisterRepository,
    ) -> Result<(), ResourceRegistryError> {
        let aggregate = RepositoryAggregate::register(command.definition, command.initial_config)?;
        match self
            .repository
            .insert_repository(&aggregate)
            .map_err(registry_error)?
        {
            InsertOutcome::Inserted => Ok(()),
            InsertOutcome::AlreadyRegistered => Err(ResourceRegistryError::AlreadyRegistered(
                aggregate.definition().id.to_string(),
            )),
        }
    }

    fn publish_repository_config(
        &self,
        command: PublishRepositoryConfig,
    ) -> Result<(), ResourceRegistryError> {
        let id = command.config.repository_id.to_string();
        let Some(mut stored) = self.repository(&id)? else {
            return Err(ResourceRegistryError::NotFound(id));
        };
        stored.aggregate.publish_config(
            &command.workspace_id,
            command.expected_current,
            command.config,
            self.clock.now_unix_nanos(),
        )?;
        self.replace_repository(&id, stored)
    }

    fn change_repository_state(
        &self,
        command: ChangeRepositoryState,
    ) -> Result<(), ResourceRegistryError> {
        let id = command.id.to_string();
        let Some(mut stored) = self.repository(&id)? else {
            return Err(ResourceRegistryError::NotFound(id));
        };
        stored.aggregate.change_state(
            &command.workspace_id,
            command.state,
            self.clock.now_unix_nanos(),
        )?;
        self.replace_repository(&id, stored)
    }
}

impl ExecutionResourceResolver for RegistryApplication {
    fn resolve_memory_store(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<MemoryStoreConfigVersion, ResourceRegistryError> {
        self.memory(id)?
            .ok_or_else(|| ResourceRegistryError::NotFound(id.into()))?
            .aggregate
            .resolve_for_execution(workspace_id)
    }

    fn resolve_repository(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<RepositoryConfigVersion, ResourceRegistryError> {
        self.repository(id)?
            .ok_or_else(|| ResourceRegistryError::NotFound(id.into()))?
            .aggregate
            .resolve_for_execution(workspace_id)
    }
}

impl LiveResourceBindingVerifier for RegistryApplication {
    fn verify_memory_binding(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Result<(), ResourceRegistryError> {
        self.memory(id)?
            .ok_or_else(|| ResourceRegistryError::NotFound(id.into()))?
            .aggregate
            .verify_live_binding(workspace_id, version)
    }

    fn verify_repository_binding(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Result<(), ResourceRegistryError> {
        self.repository(id)?
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

    struct FixedClock(u64);

    impl RegistryClock for FixedClock {
        fn now_unix_nanos(&self) -> u64 {
            self.0
        }
    }

    #[test]
    fn application_owns_profile_timestamps_with_an_injected_clock() {
        // Responsibility test: driving adapters supply intent but never time.
        // A profile mutation commits the application clock exactly once; the
        // repository only preserves that already-decided aggregate state.
        let repository = Arc::new(
            awaken_resource_store::SqliteResourceStore::in_memory()
                .expect("open test Registry repository"),
        );
        let registry = RegistryApplication::with_clock(repository, Arc::new(FixedClock(42)));
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
            .expect("register clock fixture");
        registry
            .update_memory_store_profile(UpdateMemoryStoreProfile {
                workspace_id: "workspace".into(),
                id: "memory-clock".into(),
                name: "Renamed".into(),
                description: String::new(),
                metadata: Default::default(),
            })
            .expect("update profile");
        let definition = registry
            .find_memory_store("workspace", "memory-clock")
            .expect("read profile")
            .expect("profile exists");
        assert_eq!(definition.name, "Renamed");
        assert_eq!(definition.timestamps.updated_unix_nanos, 42);
    }
}
