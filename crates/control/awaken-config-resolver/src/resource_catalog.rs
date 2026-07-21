//! In-memory Resource Catalog adapter used by local mode and conformance tests.

use std::collections::BTreeMap;
use std::sync::Mutex;

use awaken_resource_contract::{
    ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition, RepositoryConfigVersion,
    RepositoryDefinition, ResolvedMemoryStoreConfig, ResolvedRepositoryConfig, ResourceCatalog,
    ResourceCatalogError, ResourceConfigSource, ResourceState,
};

#[derive(Default)]
struct CatalogState {
    memories: BTreeMap<String, MemoryStoreDefinition>,
    memory_configs: BTreeMap<(String, ConfigVersion), MemoryStoreConfigVersion>,
    repositories: BTreeMap<String, RepositoryDefinition>,
    repository_configs: BTreeMap<(String, ConfigVersion), RepositoryConfigVersion>,
}

#[derive(Default)]
pub struct InMemoryResourceCatalog(Mutex<CatalogState>);

impl InMemoryResourceCatalog {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

fn validate_identity(id: &str, workspace_id: &str) -> Result<(), ResourceCatalogError> {
    if id.trim().is_empty() || workspace_id.trim().is_empty() {
        return Err(ResourceCatalogError::Invalid(
            "resource id and workspace id must be non-empty".into(),
        ));
    }
    Ok(())
}

fn check_publish(
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
    let required_next = current.checked_next().ok_or_else(|| {
        ResourceCatalogError::Invalid(format!("resource `{id}` exhausted config versions"))
    })?;
    if next != required_next {
        return Err(ResourceCatalogError::Invalid(format!(
            "resource `{id}` config version must advance from {} to {}",
            current.0, required_next.0
        )));
    }
    Ok(())
}

impl ResourceConfigSource for InMemoryResourceCatalog {
    fn resolve_memory_store(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<ResolvedMemoryStoreConfig, ResourceCatalogError> {
        let state = self.0.lock().expect("resource catalog");
        let definition = state
            .memories
            .get(id)
            .filter(|definition| definition.workspace_id == workspace_id)
            .cloned()
            .ok_or_else(|| ResourceCatalogError::NotFound(id.into()))?;
        if definition.state != ResourceState::Active {
            return Err(ResourceCatalogError::NotActive {
                id: id.into(),
                state: definition.state,
            });
        }
        let config = state
            .memory_configs
            .get(&(id.into(), definition.current_config_version))
            .cloned()
            .ok_or_else(|| {
                ResourceCatalogError::Storage(format!(
                    "MemoryStore `{id}` current config version is missing"
                ))
            })?;
        Ok(ResolvedMemoryStoreConfig { definition, config })
    }

    fn resolve_repository(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<ResolvedRepositoryConfig, ResourceCatalogError> {
        let state = self.0.lock().expect("resource catalog");
        let definition = state
            .repositories
            .get(id)
            .filter(|definition| definition.workspace_id == workspace_id)
            .cloned()
            .ok_or_else(|| ResourceCatalogError::NotFound(id.into()))?;
        if definition.state != ResourceState::Active {
            return Err(ResourceCatalogError::NotActive {
                id: id.into(),
                state: definition.state,
            });
        }
        let config = state
            .repository_configs
            .get(&(id.into(), definition.current_config_version))
            .cloned()
            .ok_or_else(|| {
                ResourceCatalogError::Storage(format!(
                    "Repository `{id}` current config version is missing"
                ))
            })?;
        Ok(ResolvedRepositoryConfig { definition, config })
    }
}

impl ResourceCatalog for InMemoryResourceCatalog {
    fn create_memory_store(
        &self,
        definition: MemoryStoreDefinition,
        initial_config: MemoryStoreConfigVersion,
    ) -> Result<(), ResourceCatalogError> {
        validate_identity(&definition.id, &definition.workspace_id)?;
        if definition.current_config_version != ConfigVersion::INITIAL
            || initial_config.version != ConfigVersion::INITIAL
            || initial_config.memory_store_id != definition.id
        {
            return Err(ResourceCatalogError::Invalid(
                "initial MemoryStore definition/config must agree at version 1".into(),
            ));
        }
        let mut state = self.0.lock().expect("resource catalog");
        if state.memories.contains_key(&definition.id) {
            return Err(ResourceCatalogError::AlreadyExists(definition.id));
        }
        state.memory_configs.insert(
            (definition.id.clone(), ConfigVersion::INITIAL),
            initial_config,
        );
        state.memories.insert(definition.id.clone(), definition);
        Ok(())
    }

    fn memory_store(&self, workspace_id: &str, id: &str) -> Option<MemoryStoreDefinition> {
        self.0
            .lock()
            .expect("resource catalog")
            .memories
            .get(id)
            .filter(|definition| definition.workspace_id == workspace_id)
            .cloned()
    }

    fn memory_config(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Option<MemoryStoreConfigVersion> {
        let state = self.0.lock().expect("resource catalog");
        state
            .memories
            .get(id)
            .filter(|definition| definition.workspace_id == workspace_id)?;
        state.memory_configs.get(&(id.into(), version)).cloned()
    }

    fn publish_memory_config(
        &self,
        workspace_id: &str,
        expected_current: ConfigVersion,
        config: MemoryStoreConfigVersion,
    ) -> Result<(), ResourceCatalogError> {
        let mut state = self.0.lock().expect("resource catalog");
        let definition = state
            .memories
            .get_mut(&config.memory_store_id)
            .filter(|definition| definition.workspace_id == workspace_id)
            .ok_or_else(|| ResourceCatalogError::NotFound(config.memory_store_id.clone()))?;
        check_publish(
            &definition.id,
            definition.current_config_version,
            expected_current,
            config.version,
        )?;
        if definition.state == ResourceState::Deleted {
            return Err(ResourceCatalogError::NotActive {
                id: definition.id.clone(),
                state: definition.state,
            });
        }
        definition.current_config_version = config.version;
        state
            .memory_configs
            .insert((config.memory_store_id.clone(), config.version), config);
        Ok(())
    }

    fn set_memory_state(
        &self,
        workspace_id: &str,
        id: &str,
        state_value: ResourceState,
    ) -> Result<(), ResourceCatalogError> {
        let mut state = self.0.lock().expect("resource catalog");
        let definition = state
            .memories
            .get_mut(id)
            .filter(|definition| definition.workspace_id == workspace_id)
            .ok_or_else(|| ResourceCatalogError::NotFound(id.into()))?;
        definition.state = state_value;
        Ok(())
    }

    fn create_repository(
        &self,
        definition: RepositoryDefinition,
        initial_config: RepositoryConfigVersion,
    ) -> Result<(), ResourceCatalogError> {
        validate_identity(&definition.id, &definition.workspace_id)?;
        if definition.current_config_version != ConfigVersion::INITIAL
            || initial_config.version != ConfigVersion::INITIAL
            || initial_config.repository_id != definition.id
        {
            return Err(ResourceCatalogError::Invalid(
                "initial Repository definition/config must agree at version 1".into(),
            ));
        }
        let mut state = self.0.lock().expect("resource catalog");
        if state.repositories.contains_key(&definition.id) {
            return Err(ResourceCatalogError::AlreadyExists(definition.id));
        }
        state.repository_configs.insert(
            (definition.id.clone(), ConfigVersion::INITIAL),
            initial_config,
        );
        state.repositories.insert(definition.id.clone(), definition);
        Ok(())
    }

    fn repository(&self, workspace_id: &str, id: &str) -> Option<RepositoryDefinition> {
        self.0
            .lock()
            .expect("resource catalog")
            .repositories
            .get(id)
            .filter(|definition| definition.workspace_id == workspace_id)
            .cloned()
    }

    fn repository_config(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Option<RepositoryConfigVersion> {
        let state = self.0.lock().expect("resource catalog");
        state
            .repositories
            .get(id)
            .filter(|definition| definition.workspace_id == workspace_id)?;
        state.repository_configs.get(&(id.into(), version)).cloned()
    }

    fn publish_repository_config(
        &self,
        workspace_id: &str,
        expected_current: ConfigVersion,
        config: RepositoryConfigVersion,
    ) -> Result<(), ResourceCatalogError> {
        let mut state = self.0.lock().expect("resource catalog");
        let definition = state
            .repositories
            .get_mut(&config.repository_id)
            .filter(|definition| definition.workspace_id == workspace_id)
            .ok_or_else(|| ResourceCatalogError::NotFound(config.repository_id.clone()))?;
        check_publish(
            &definition.id,
            definition.current_config_version,
            expected_current,
            config.version,
        )?;
        if definition.state == ResourceState::Deleted {
            return Err(ResourceCatalogError::NotActive {
                id: definition.id.clone(),
                state: definition.state,
            });
        }
        definition.current_config_version = config.version;
        state
            .repository_configs
            .insert((config.repository_id.clone(), config.version), config);
        Ok(())
    }

    fn set_repository_state(
        &self,
        workspace_id: &str,
        id: &str,
        state_value: ResourceState,
    ) -> Result<(), ResourceCatalogError> {
        let mut state = self.0.lock().expect("resource catalog");
        let definition = state
            .repositories
            .get_mut(id)
            .filter(|definition| definition.workspace_id == workspace_id)
            .ok_or_else(|| ResourceCatalogError::NotFound(id.into()))?;
        definition.state = state_value;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_resource_contract::{ClonePolicy, ExtractionPolicy, RecallPolicy, RetentionPolicy};

    fn memory_definition(workspace: &str) -> MemoryStoreDefinition {
        MemoryStoreDefinition {
            id: "memory-1".into(),
            workspace_id: workspace.into(),
            name: "Memory".into(),
            description: String::new(),
            metadata: BTreeMap::new(),
            state: ResourceState::Active,
            current_config_version: ConfigVersion::INITIAL,
        }
    }

    fn memory_config(version: u64) -> MemoryStoreConfigVersion {
        MemoryStoreConfigVersion {
            memory_store_id: "memory-1".into(),
            version: ConfigVersion(version),
            recall_policy: RecallPolicy::default(),
            extraction_policy: ExtractionPolicy::default(),
            retention_policy: RetentionPolicy::default(),
        }
    }

    fn repository_definition(workspace: &str) -> RepositoryDefinition {
        RepositoryDefinition {
            id: "repo-1".into(),
            workspace_id: workspace.into(),
            name: "Repository".into(),
            description: String::new(),
            metadata: BTreeMap::new(),
            state: ResourceState::Active,
            current_config_version: ConfigVersion::INITIAL,
        }
    }

    fn repository_config(version: u64, url: &str) -> RepositoryConfigVersion {
        RepositoryConfigVersion {
            repository_id: "repo-1".into(),
            version: ConfigVersion(version),
            remote_url: url.into(),
            credential_binding: Some("credential-1".into()),
            initial_branch: None,
            clone_policy: ClonePolicy::default(),
        }
    }

    #[test]
    fn memory_resolution_pins_config_not_content_and_preserves_history() {
        let catalog = InMemoryResourceCatalog::new();
        catalog
            .create_memory_store(memory_definition("workspace-a"), memory_config(1))
            .unwrap();
        let first = catalog
            .resolve_memory_store("workspace-a", "memory-1")
            .unwrap();
        catalog
            .publish_memory_config("workspace-a", ConfigVersion(1), memory_config(2))
            .unwrap();
        let second = catalog
            .resolve_memory_store("workspace-a", "memory-1")
            .unwrap();

        assert_eq!(first.config.version, ConfigVersion(1));
        assert_eq!(second.config.version, ConfigVersion(2));
        assert_eq!(
            catalog
                .memory_config("workspace-a", "memory-1", ConfigVersion(1))
                .unwrap()
                .version,
            ConfigVersion(1)
        );
    }

    #[test]
    fn cross_workspace_and_suspended_memory_resolve_fail_closed() {
        let catalog = InMemoryResourceCatalog::new();
        catalog
            .create_memory_store(memory_definition("workspace-a"), memory_config(1))
            .unwrap();
        assert!(matches!(
            catalog.resolve_memory_store("workspace-b", "memory-1"),
            Err(ResourceCatalogError::NotFound(_))
        ));
        catalog
            .set_memory_state("workspace-a", "memory-1", ResourceState::Suspended)
            .unwrap();
        assert!(matches!(
            catalog.resolve_memory_store("workspace-a", "memory-1"),
            Err(ResourceCatalogError::NotActive { .. })
        ));
    }

    #[test]
    fn repository_versions_never_contain_a_commit_pin() {
        let catalog = InMemoryResourceCatalog::new();
        catalog
            .create_repository(
                repository_definition("workspace-a"),
                repository_config(1, "https://example.test/one.git"),
            )
            .unwrap();
        catalog
            .publish_repository_config(
                "workspace-a",
                ConfigVersion(1),
                repository_config(2, "https://example.test/two.git"),
            )
            .unwrap();

        let resolved = catalog.resolve_repository("workspace-a", "repo-1").unwrap();
        assert_eq!(resolved.config.version, ConfigVersion(2));
        assert_eq!(resolved.config.remote_url, "https://example.test/two.git");
    }

    #[test]
    fn config_publication_is_compare_and_swap_and_monotonic() {
        let catalog = InMemoryResourceCatalog::new();
        catalog
            .create_repository(
                repository_definition("workspace-a"),
                repository_config(1, "https://example.test/one.git"),
            )
            .unwrap();
        assert!(matches!(
            catalog.publish_repository_config(
                "workspace-a",
                ConfigVersion(9),
                repository_config(2, "https://example.test/two.git"),
            ),
            Err(ResourceCatalogError::ConfigConflict { .. })
        ));
        assert!(matches!(
            catalog.publish_repository_config(
                "workspace-a",
                ConfigVersion(1),
                repository_config(3, "https://example.test/three.git"),
            ),
            Err(ResourceCatalogError::Invalid(_))
        ));
    }
}
