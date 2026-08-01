//! Resources-owned SQLite Resource Catalog adapter. One JSON record per aggregate keeps the
//! definition, current pointer, and immutable config history in one atomic row.

use std::collections::BTreeMap;

use awaken_resource_contract::{
    ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition, RepositoryConfigVersion,
    RepositoryDefinition, ResourceBindingValidator, ResourceCatalog, ResourceCatalogError,
    ResourceCatalogRules, ResourceConfigSource, ResourceState,
};
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

use crate::SqliteResourceStore;
use crate::resource_catalog_codec::{MemoryRecord, RepositoryRecord, now_nanos};
use crate::schema::CATALOG_NS;

const MEMORY: &str = "memory_store";
const REPOSITORY: &str = "repository";

fn storage(error: impl ToString) -> ResourceCatalogError {
    ResourceCatalogError::Storage(error.to_string())
}

impl SqliteResourceStore {
    fn catalog_record<T: serde::de::DeserializeOwned>(
        &self,
        kind: &str,
        id: &str,
    ) -> Result<Option<T>, ResourceCatalogError> {
        let data: Option<String> = self
            .connection()
            .query_row(
                &format!("SELECT data FROM {CATALOG_NS}_entry WHERE kind = ?1 AND id = ?2"),
                params![kind, id],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage)?;
        data.map(|data| serde_json::from_str(&data).map_err(storage))
            .transpose()
    }

    fn memory_record(&self, id: &str) -> Result<Option<MemoryRecord>, ResourceCatalogError> {
        let record = self.catalog_record::<MemoryRecord>(MEMORY, id)?;
        if let Some(record) = &record {
            ResourceCatalogRules::validate_memory_aggregate(
                id,
                &record.definition,
                &record.configs,
            )?;
        }
        Ok(record)
    }

    fn repository_record(
        &self,
        id: &str,
    ) -> Result<Option<RepositoryRecord>, ResourceCatalogError> {
        let record = self.catalog_record::<RepositoryRecord>(REPOSITORY, id)?;
        if let Some(record) = &record {
            ResourceCatalogRules::validate_repository_aggregate(
                id,
                &record.definition,
                &record.configs,
            )?;
        }
        Ok(record)
    }

    fn insert_catalog_record<T: Serialize>(
        &self,
        kind: &str,
        id: &str,
        value: &T,
    ) -> Result<(), ResourceCatalogError> {
        let data = serde_json::to_string(value).map_err(storage)?;
        let result = self.connection().execute(
            &format!("INSERT INTO {CATALOG_NS}_entry (kind, id, data) VALUES (?1, ?2, ?3)"),
            params![kind, id, data],
        );
        match result {
            Ok(_) => Ok(()),
            Err(error)
                if error.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) =>
            {
                Err(ResourceCatalogError::AlreadyExists(id.into()))
            }
            Err(error) => Err(storage(error)),
        }
    }

    fn update_catalog_record<T, F>(
        &self,
        kind: &str,
        id: &str,
        update: F,
    ) -> Result<(), ResourceCatalogError>
    where
        T: serde::de::DeserializeOwned + Serialize,
        F: FnOnce(&mut T) -> Result<(), ResourceCatalogError>,
    {
        let mut conn = self.connection();
        let tx = conn.transaction().map_err(storage)?;
        let data: Option<String> = tx
            .query_row(
                &format!("SELECT data FROM {CATALOG_NS}_entry WHERE kind = ?1 AND id = ?2"),
                params![kind, id],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage)?;
        let mut record: T = data
            .map(|data| serde_json::from_str(&data).map_err(storage))
            .transpose()?
            .ok_or_else(|| ResourceCatalogError::NotFound(id.into()))?;
        update(&mut record)?;
        let data = serde_json::to_string(&record).map_err(storage)?;
        tx.execute(
            &format!("UPDATE {CATALOG_NS}_entry SET data = ?3 WHERE kind = ?1 AND id = ?2"),
            params![kind, id, data],
        )
        .map_err(storage)?;
        tx.commit().map_err(storage)
    }
}

impl ResourceConfigSource for SqliteResourceStore {
    fn resolve_memory_store(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<MemoryStoreConfigVersion, ResourceCatalogError> {
        let record = self
            .memory_record(id)?
            .filter(|record| record.definition.workspace_id.as_str() == workspace_id)
            .ok_or_else(|| ResourceCatalogError::NotFound(id.into()))?;
        ResourceCatalogRules::validate_live_definition(id, record.definition.state)?;
        let version = record.definition.current_config_version;
        let config = record.configs.get(&version).cloned().ok_or_else(|| {
            ResourceCatalogError::Storage(format!(
                "MemoryStore `{id}` current config version is missing"
            ))
        })?;
        ResourceCatalogRules::validate_memory_config(id, version, &config)?;
        Ok(config)
    }

    fn resolve_repository(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<RepositoryConfigVersion, ResourceCatalogError> {
        let record = self
            .repository_record(id)?
            .filter(|record| record.definition.workspace_id.as_str() == workspace_id)
            .ok_or_else(|| ResourceCatalogError::NotFound(id.into()))?;
        ResourceCatalogRules::validate_live_definition(id, record.definition.state)?;
        let version = record.definition.current_config_version;
        let config = record.configs.get(&version).cloned().ok_or_else(|| {
            ResourceCatalogError::Storage(format!(
                "Repository `{id}` current config version is missing"
            ))
        })?;
        ResourceCatalogRules::validate_repository_config(id, version, &config)?;
        Ok(config)
    }
}

impl ResourceBindingValidator for SqliteResourceStore {
    fn validate_memory_binding(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Result<(), ResourceCatalogError> {
        let record = self
            .memory_record(id)?
            .filter(|record| record.definition.workspace_id.as_str() == workspace_id)
            .ok_or_else(|| ResourceCatalogError::NotFound(id.into()))?;
        ResourceCatalogRules::validate_live_definition(id, record.definition.state)?;
        let config =
            record
                .configs
                .get(&version)
                .ok_or_else(|| ResourceCatalogError::ConfigNotFound {
                    id: id.into(),
                    version,
                })?;
        ResourceCatalogRules::validate_memory_config(id, version, config)
    }

    fn validate_repository_binding(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Result<(), ResourceCatalogError> {
        let record = self
            .repository_record(id)?
            .filter(|record| record.definition.workspace_id.as_str() == workspace_id)
            .ok_or_else(|| ResourceCatalogError::NotFound(id.into()))?;
        ResourceCatalogRules::validate_live_definition(id, record.definition.state)?;
        let config =
            record
                .configs
                .get(&version)
                .ok_or_else(|| ResourceCatalogError::ConfigNotFound {
                    id: id.into(),
                    version,
                })?;
        ResourceCatalogRules::validate_repository_config(id, version, config)
    }
}

impl ResourceCatalog for SqliteResourceStore {
    fn create_memory_store(
        &self,
        definition: MemoryStoreDefinition,
        initial_config: MemoryStoreConfigVersion,
    ) -> Result<(), ResourceCatalogError> {
        ResourceCatalogRules::validate_initial(
            definition.id.as_str(),
            definition.workspace_id.as_str(),
            definition.current_config_version,
            initial_config.memory_store_id.as_str(),
            initial_config.version,
        )?;
        let id = definition.id.clone();
        self.insert_catalog_record(
            MEMORY,
            id.as_str(),
            &MemoryRecord {
                definition,
                configs: BTreeMap::from([(ConfigVersion::INITIAL, initial_config)]),
            },
        )
    }

    fn memory_store(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Option<MemoryStoreDefinition>, ResourceCatalogError> {
        Ok(self
            .memory_record(id)?
            .filter(|record| record.definition.workspace_id.as_str() == workspace_id)
            .map(|record| record.definition))
    }

    fn list_memory_stores(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<MemoryStoreDefinition>, ResourceCatalogError> {
        let conn = self.connection();
        let mut statement = conn
            .prepare(&format!(
                "SELECT id, data FROM {CATALOG_NS}_entry WHERE kind = ?1 ORDER BY id"
            ))
            .map_err(storage)?;
        let rows = statement
            .query_map(params![MEMORY], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(storage)?;
        let records = rows
            .map(|row| {
                let (id, data) = row.map_err(storage)?;
                let record: MemoryRecord = serde_json::from_str(&data).map_err(storage)?;
                ResourceCatalogRules::validate_memory_aggregate(
                    &id,
                    &record.definition,
                    &record.configs,
                )?;
                Ok(record)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(records
            .into_iter()
            .filter(|record| {
                record.definition.workspace_id.as_str() == workspace_id
                    && !matches!(
                        record.definition.state,
                        ResourceState::Archived | ResourceState::Deleted
                    )
            })
            .map(|record| record.definition)
            .collect())
    }

    fn update_memory_store(
        &self,
        definition: MemoryStoreDefinition,
    ) -> Result<(), ResourceCatalogError> {
        let id = definition.id.clone();
        self.update_catalog_record::<MemoryRecord, _>(MEMORY, id.as_str(), |record| {
            if record.definition.workspace_id != definition.workspace_id {
                return Err(ResourceCatalogError::NotFound(id.to_string()));
            }
            if record.definition.state != definition.state
                || record.definition.current_config_version != definition.current_config_version
            {
                return Err(ResourceCatalogError::Invalid(
                    "definition update cannot change lifecycle or config version".into(),
                ));
            }
            record.definition.name = definition.name;
            record.definition.description = definition.description;
            record.definition.metadata = definition.metadata;
            Ok(())
        })
    }

    fn memory_config(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Result<Option<MemoryStoreConfigVersion>, ResourceCatalogError> {
        Ok(self
            .memory_record(id)?
            .filter(|record| record.definition.workspace_id.as_str() == workspace_id)
            .and_then(|record| record.configs.get(&version).cloned()))
    }

    fn publish_memory_config(
        &self,
        workspace_id: &str,
        expected_current: ConfigVersion,
        config: MemoryStoreConfigVersion,
    ) -> Result<(), ResourceCatalogError> {
        let id = config.memory_store_id.clone();
        self.update_catalog_record::<MemoryRecord, _>(MEMORY, id.as_str(), |record| {
            if record.definition.workspace_id.as_str() != workspace_id {
                return Err(ResourceCatalogError::NotFound(id.to_string()));
            }
            ResourceCatalogRules::validate_publish(
                record.definition.id.as_str(),
                record.definition.current_config_version,
                expected_current,
                config.version,
            )?;
            if record.definition.state == ResourceState::Deleted {
                return Err(ResourceCatalogError::NotActive {
                    id: record.definition.id.to_string(),
                    state: ResourceState::Deleted,
                });
            }
            record.definition.current_config_version = config.version;
            record.configs.insert(config.version, config);
            Ok(())
        })
    }

    fn set_memory_state(
        &self,
        workspace_id: &str,
        id: &str,
        state: ResourceState,
    ) -> Result<(), ResourceCatalogError> {
        let at = now_nanos();
        self.update_catalog_record::<MemoryRecord, _>(MEMORY, id, |record| {
            if record.definition.workspace_id.as_str() != workspace_id {
                return Err(ResourceCatalogError::NotFound(id.into()));
            }
            record.definition.state = state;
            record.definition.timestamps.transition_to(state, at);
            Ok(())
        })
    }

    fn create_repository(
        &self,
        definition: RepositoryDefinition,
        initial_config: RepositoryConfigVersion,
    ) -> Result<(), ResourceCatalogError> {
        ResourceCatalogRules::validate_initial(
            definition.id.as_str(),
            definition.workspace_id.as_str(),
            definition.current_config_version,
            initial_config.repository_id.as_str(),
            initial_config.version,
        )?;
        let id = definition.id.clone();
        self.insert_catalog_record(
            REPOSITORY,
            id.as_str(),
            &RepositoryRecord {
                definition,
                configs: BTreeMap::from([(ConfigVersion::INITIAL, initial_config)]),
            },
        )
    }

    fn repository(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Option<RepositoryDefinition>, ResourceCatalogError> {
        Ok(self
            .repository_record(id)?
            .filter(|record| record.definition.workspace_id.as_str() == workspace_id)
            .map(|record| record.definition))
    }

    fn repository_config(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Result<Option<RepositoryConfigVersion>, ResourceCatalogError> {
        Ok(self
            .repository_record(id)?
            .filter(|record| record.definition.workspace_id.as_str() == workspace_id)
            .and_then(|record| record.configs.get(&version).cloned()))
    }

    fn publish_repository_config(
        &self,
        workspace_id: &str,
        expected_current: ConfigVersion,
        config: RepositoryConfigVersion,
    ) -> Result<(), ResourceCatalogError> {
        let id = config.repository_id.clone();
        self.update_catalog_record::<RepositoryRecord, _>(REPOSITORY, id.as_str(), |record| {
            if record.definition.workspace_id.as_str() != workspace_id {
                return Err(ResourceCatalogError::NotFound(id.to_string()));
            }
            ResourceCatalogRules::validate_publish(
                record.definition.id.as_str(),
                record.definition.current_config_version,
                expected_current,
                config.version,
            )?;
            if record.definition.state == ResourceState::Deleted {
                return Err(ResourceCatalogError::NotActive {
                    id: record.definition.id.to_string(),
                    state: ResourceState::Deleted,
                });
            }
            record.definition.current_config_version = config.version;
            record.configs.insert(config.version, config);
            Ok(())
        })
    }

    fn set_repository_state(
        &self,
        workspace_id: &str,
        id: &str,
        state: ResourceState,
    ) -> Result<(), ResourceCatalogError> {
        let at = now_nanos();
        self.update_catalog_record::<RepositoryRecord, _>(REPOSITORY, id, |record| {
            if record.definition.workspace_id.as_str() != workspace_id {
                return Err(ResourceCatalogError::NotFound(id.into()));
            }
            record.definition.state = state;
            record.definition.timestamps.transition_to(state, at);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_resource_contract::{ClonePolicy, RetentionPolicy};

    fn memory() -> (MemoryStoreDefinition, MemoryStoreConfigVersion) {
        (
            MemoryStoreDefinition {
                id: "memory-1".into(),
                workspace_id: "workspace-a".into(),
                name: "Memory".into(),
                description: String::new(),
                metadata: BTreeMap::new(),
                state: ResourceState::Active,
                current_config_version: ConfigVersion::INITIAL,
                timestamps: Default::default(),
            },
            MemoryStoreConfigVersion {
                memory_store_id: "memory-1".into(),
                version: ConfigVersion::INITIAL,
                retention_policy: RetentionPolicy::default(),
            },
        )
    }

    fn repository() -> (RepositoryDefinition, RepositoryConfigVersion) {
        (
            RepositoryDefinition {
                id: "repo-1".into(),
                workspace_id: "workspace-a".into(),
                name: "Repo".into(),
                description: String::new(),
                metadata: BTreeMap::new(),
                state: ResourceState::Active,
                current_config_version: ConfigVersion::INITIAL,
                timestamps: Default::default(),
            },
            RepositoryConfigVersion {
                repository_id: "repo-1".into(),
                version: ConfigVersion::INITIAL,
                remote_url: "https://example.test/repo.git".into(),
                credential_binding: Some("credential-1".into()),
                initial_branch: None,
                initial_commit: None,
                clone_policy: ClonePolicy::default(),
            },
        )
    }

    #[test]
    fn catalog_versions_and_state_survive_reopen() {
        // Cause/effect rules: a fresh Resources database applies the catalog
        // bundle and accepts valid aggregate transitions; a reopen validates the
        // same ledger and returns the persisted current/history/state. Foreign
        // Workspace and stale-version inputs still fail without side effects.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resources.db");
        {
            let store = SqliteResourceStore::open(path.to_str().unwrap()).unwrap();
            let (definition, initial) = memory();
            store.create_memory_store(definition, initial).unwrap();
            let mut second = store
                .memory_config("workspace-a", "memory-1", ConfigVersion(1))
                .unwrap()
                .unwrap();
            second.version = ConfigVersion(2);
            second.retention_policy.retention_days = Some(25);
            assert!(matches!(
                store.publish_memory_config("workspace-b", ConfigVersion(1), second.clone()),
                Err(ResourceCatalogError::NotFound(_))
            ));
            store
                .publish_memory_config("workspace-a", ConfigVersion(1), second.clone())
                .unwrap();
            let mut third = second;
            third.version = ConfigVersion(3);
            assert!(matches!(
                store.publish_memory_config("workspace-a", ConfigVersion(1), third),
                Err(ResourceCatalogError::ConfigConflict { .. })
            ));
            let (definition, initial) = repository();
            store.create_repository(definition, initial).unwrap();
            store
                .set_repository_state("workspace-a", "repo-1", ResourceState::Suspended)
                .unwrap();
        }

        let reopened = SqliteResourceStore::open(path.to_str().unwrap()).unwrap();
        assert_eq!(
            reopened
                .resolve_memory_store("workspace-a", "memory-1")
                .unwrap()
                .version,
            ConfigVersion(2)
        );
        assert!(
            reopened
                .memory_config("workspace-a", "memory-1", ConfigVersion(1))
                .unwrap()
                .is_some()
        );
        assert!(
            reopened
                .memory_config("workspace-a", "memory-1", ConfigVersion(3))
                .unwrap()
                .is_none()
        );
        reopened
            .validate_memory_binding("workspace-a", "memory-1", ConfigVersion(1))
            .unwrap();
        reopened
            .validate_memory_binding("workspace-a", "memory-1", ConfigVersion(2))
            .unwrap();
        assert!(matches!(
            reopened.validate_memory_binding("workspace-a", "memory-1", ConfigVersion(3)),
            Err(ResourceCatalogError::ConfigNotFound { .. })
        ));
        assert!(matches!(
            reopened.resolve_repository("workspace-a", "repo-1"),
            Err(ResourceCatalogError::NotActive { .. })
        ));
        assert!(matches!(
            reopened.validate_repository_binding("workspace-a", "repo-1", ConfigVersion::INITIAL),
            Err(ResourceCatalogError::NotActive { .. })
        ));
        assert!(
            reopened
                .repository("workspace-b", "repo-1")
                .unwrap()
                .is_none()
        );
    }
}
