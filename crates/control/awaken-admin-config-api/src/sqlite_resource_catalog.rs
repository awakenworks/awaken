//! SQLite Resource Catalog adapter. One JSON record per aggregate keeps the
//! definition, current pointer, and immutable config history in one atomic row.

use std::collections::BTreeMap;

use awaken_resource_contract::{
    ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition, RepositoryConfigVersion,
    RepositoryDefinition, ResourceCatalog, ResourceCatalogError, ResourceConfigSource,
    ResourceState,
};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::sqlite::{NS, SqliteAdminStore};

const MEMORY: &str = "memory_store";
const REPOSITORY: &str = "repository";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MemoryRecord {
    definition: MemoryStoreDefinition,
    configs: BTreeMap<ConfigVersion, MemoryStoreConfigVersion>,
}

/// Upgrade-only shape written by the removed `MemoryStoreRegistry`. Empty-owner
/// rows are deliberately quarantined: ownership cannot be inferred safely.
#[derive(Debug, Deserialize)]
struct LegacyMemoryStoreDef {
    id: String,
    #[serde(default)]
    workspace_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
    #[serde(default)]
    archived: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RepositoryRecord {
    definition: RepositoryDefinition,
    configs: BTreeMap<ConfigVersion, RepositoryConfigVersion>,
}

fn storage(error: impl ToString) -> ResourceCatalogError {
    ResourceCatalogError::Storage(error.to_string())
}

fn validate_initial(
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

fn validate_publish(
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

impl SqliteAdminStore {
    /// Idempotently import owned rows from the retired identity table into the
    /// Resource Catalog. This is an upgrade adapter, never a live read fallback.
    pub(crate) fn migrate_legacy_memory_stores(&self) -> Result<(), ResourceCatalogError> {
        let rows = {
            let conn = self.conn.lock().expect("resource catalog");
            let mut statement = conn
                .prepare(&format!("SELECT data FROM {NS}_memory_store ORDER BY id"))
                .map_err(storage)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(storage)?;
            rows.map(|row| {
                let data = row.map_err(storage)?;
                serde_json::from_str::<LegacyMemoryStoreDef>(&data).map_err(storage)
            })
            .collect::<Result<Vec<_>, _>>()?
        };
        for legacy in rows {
            if legacy.workspace_id.trim().is_empty()
                || self
                    .catalog_record::<MemoryRecord>(MEMORY, &legacy.id)
                    .is_some()
            {
                continue;
            }
            let id = legacy.id;
            let result = self.create_memory_store(
                MemoryStoreDefinition {
                    id: id.clone(),
                    workspace_id: legacy.workspace_id,
                    name: legacy.name,
                    description: legacy.description,
                    metadata: legacy.metadata,
                    state: if legacy.archived {
                        ResourceState::Archived
                    } else {
                        ResourceState::Active
                    },
                    current_config_version: ConfigVersion::INITIAL,
                },
                MemoryStoreConfigVersion {
                    memory_store_id: id,
                    version: ConfigVersion::INITIAL,
                    recall_policy: Default::default(),
                    extraction_policy: Default::default(),
                    retention_policy: Default::default(),
                },
            );
            match result {
                Ok(()) | Err(ResourceCatalogError::AlreadyExists(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn catalog_record<T: serde::de::DeserializeOwned>(&self, kind: &str, id: &str) -> Option<T> {
        let data: Option<String> = self
            .conn
            .lock()
            .expect("resource catalog")
            .query_row(
                &format!("SELECT data FROM {NS}_resource_catalog WHERE kind = ?1 AND id = ?2"),
                params![kind, id],
                |row| row.get(0),
            )
            .optional()
            .expect("read resource catalog");
        data.map(|data| serde_json::from_str(&data).expect("decode resource catalog"))
    }

    fn insert_catalog_record<T: Serialize>(
        &self,
        kind: &str,
        id: &str,
        value: &T,
    ) -> Result<(), ResourceCatalogError> {
        let data = serde_json::to_string(value).map_err(storage)?;
        let result = self.conn.lock().expect("resource catalog").execute(
            &format!("INSERT INTO {NS}_resource_catalog (kind, id, data) VALUES (?1, ?2, ?3)"),
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
        let mut conn = self.conn.lock().expect("resource catalog");
        let tx = conn.transaction().map_err(storage)?;
        let data: Option<String> = tx
            .query_row(
                &format!("SELECT data FROM {NS}_resource_catalog WHERE kind = ?1 AND id = ?2"),
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
            &format!("UPDATE {NS}_resource_catalog SET data = ?3 WHERE kind = ?1 AND id = ?2"),
            params![kind, id, data],
        )
        .map_err(storage)?;
        tx.commit().map_err(storage)
    }
}

impl ResourceConfigSource for SqliteAdminStore {
    fn resolve_memory_store(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<MemoryStoreConfigVersion, ResourceCatalogError> {
        let record = self
            .catalog_record::<MemoryRecord>(MEMORY, id)
            .filter(|record| record.definition.workspace_id == workspace_id)
            .ok_or_else(|| ResourceCatalogError::NotFound(id.into()))?;
        if record.definition.state != ResourceState::Active {
            return Err(ResourceCatalogError::NotActive {
                id: id.into(),
                state: record.definition.state,
            });
        }
        let config = record
            .configs
            .get(&record.definition.current_config_version)
            .cloned()
            .ok_or_else(|| {
                ResourceCatalogError::Storage(format!(
                    "MemoryStore `{id}` current config version is missing"
                ))
            })?;
        Ok(config)
    }

    fn resolve_repository(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<RepositoryConfigVersion, ResourceCatalogError> {
        let record = self
            .catalog_record::<RepositoryRecord>(REPOSITORY, id)
            .filter(|record| record.definition.workspace_id == workspace_id)
            .ok_or_else(|| ResourceCatalogError::NotFound(id.into()))?;
        if record.definition.state != ResourceState::Active {
            return Err(ResourceCatalogError::NotActive {
                id: id.into(),
                state: record.definition.state,
            });
        }
        let config = record
            .configs
            .get(&record.definition.current_config_version)
            .cloned()
            .ok_or_else(|| {
                ResourceCatalogError::Storage(format!(
                    "Repository `{id}` current config version is missing"
                ))
            })?;
        Ok(config)
    }
}

impl ResourceCatalog for SqliteAdminStore {
    fn create_memory_store(
        &self,
        definition: MemoryStoreDefinition,
        initial_config: MemoryStoreConfigVersion,
    ) -> Result<(), ResourceCatalogError> {
        validate_initial(
            &definition.id,
            &definition.workspace_id,
            definition.current_config_version,
            &initial_config.memory_store_id,
            initial_config.version,
        )?;
        let id = definition.id.clone();
        self.insert_catalog_record(
            MEMORY,
            &id,
            &MemoryRecord {
                definition,
                configs: BTreeMap::from([(ConfigVersion::INITIAL, initial_config)]),
            },
        )
    }

    fn memory_store(&self, workspace_id: &str, id: &str) -> Option<MemoryStoreDefinition> {
        self.catalog_record::<MemoryRecord>(MEMORY, id)
            .filter(|record| record.definition.workspace_id == workspace_id)
            .map(|record| record.definition)
    }

    fn list_memory_stores(&self, workspace_id: &str) -> Vec<MemoryStoreDefinition> {
        let conn = self.conn.lock().expect("resource catalog");
        let mut statement = conn
            .prepare(&format!(
                "SELECT data FROM {NS}_resource_catalog WHERE kind = ?1 ORDER BY id"
            ))
            .expect("prepare resource catalog list");
        statement
            .query_map(params![MEMORY], |row| row.get::<_, String>(0))
            .expect("list resource catalog")
            .map(|data| {
                serde_json::from_str::<MemoryRecord>(&data.expect("read resource catalog"))
                    .expect("decode resource catalog")
            })
            .filter(|record| {
                record.definition.workspace_id == workspace_id
                    && !matches!(
                        record.definition.state,
                        ResourceState::Archived | ResourceState::Deleted
                    )
            })
            .map(|record| record.definition)
            .collect()
    }

    fn update_memory_store(
        &self,
        definition: MemoryStoreDefinition,
    ) -> Result<(), ResourceCatalogError> {
        let id = definition.id.clone();
        self.update_catalog_record::<MemoryRecord, _>(MEMORY, &id, |record| {
            if record.definition.workspace_id != definition.workspace_id {
                return Err(ResourceCatalogError::NotFound(id.clone()));
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
    ) -> Option<MemoryStoreConfigVersion> {
        self.catalog_record::<MemoryRecord>(MEMORY, id)
            .filter(|record| record.definition.workspace_id == workspace_id)
            .and_then(|record| record.configs.get(&version).cloned())
    }

    fn publish_memory_config(
        &self,
        workspace_id: &str,
        expected_current: ConfigVersion,
        config: MemoryStoreConfigVersion,
    ) -> Result<(), ResourceCatalogError> {
        let id = config.memory_store_id.clone();
        self.update_catalog_record::<MemoryRecord, _>(MEMORY, &id, |record| {
            if record.definition.workspace_id != workspace_id {
                return Err(ResourceCatalogError::NotFound(id.clone()));
            }
            validate_publish(
                &record.definition.id,
                record.definition.current_config_version,
                expected_current,
                config.version,
            )?;
            if record.definition.state == ResourceState::Deleted {
                return Err(ResourceCatalogError::NotActive {
                    id: record.definition.id.clone(),
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
        self.update_catalog_record::<MemoryRecord, _>(MEMORY, id, |record| {
            if record.definition.workspace_id != workspace_id {
                return Err(ResourceCatalogError::NotFound(id.into()));
            }
            record.definition.state = state;
            Ok(())
        })
    }

    fn create_repository(
        &self,
        definition: RepositoryDefinition,
        initial_config: RepositoryConfigVersion,
    ) -> Result<(), ResourceCatalogError> {
        validate_initial(
            &definition.id,
            &definition.workspace_id,
            definition.current_config_version,
            &initial_config.repository_id,
            initial_config.version,
        )?;
        let id = definition.id.clone();
        self.insert_catalog_record(
            REPOSITORY,
            &id,
            &RepositoryRecord {
                definition,
                configs: BTreeMap::from([(ConfigVersion::INITIAL, initial_config)]),
            },
        )
    }

    fn repository(&self, workspace_id: &str, id: &str) -> Option<RepositoryDefinition> {
        self.catalog_record::<RepositoryRecord>(REPOSITORY, id)
            .filter(|record| record.definition.workspace_id == workspace_id)
            .map(|record| record.definition)
    }

    fn repository_config(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Option<RepositoryConfigVersion> {
        self.catalog_record::<RepositoryRecord>(REPOSITORY, id)
            .filter(|record| record.definition.workspace_id == workspace_id)
            .and_then(|record| record.configs.get(&version).cloned())
    }

    fn publish_repository_config(
        &self,
        workspace_id: &str,
        expected_current: ConfigVersion,
        config: RepositoryConfigVersion,
    ) -> Result<(), ResourceCatalogError> {
        let id = config.repository_id.clone();
        self.update_catalog_record::<RepositoryRecord, _>(REPOSITORY, &id, |record| {
            if record.definition.workspace_id != workspace_id {
                return Err(ResourceCatalogError::NotFound(id.clone()));
            }
            validate_publish(
                &record.definition.id,
                record.definition.current_config_version,
                expected_current,
                config.version,
            )?;
            if record.definition.state == ResourceState::Deleted {
                return Err(ResourceCatalogError::NotActive {
                    id: record.definition.id.clone(),
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
        self.update_catalog_record::<RepositoryRecord, _>(REPOSITORY, id, |record| {
            if record.definition.workspace_id != workspace_id {
                return Err(ResourceCatalogError::NotFound(id.into()));
            }
            record.definition.state = state;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_resource_contract::{ClonePolicy, ExtractionPolicy, RecallPolicy, RetentionPolicy};

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
            },
            MemoryStoreConfigVersion {
                memory_store_id: "memory-1".into(),
                version: ConfigVersion::INITIAL,
                recall_policy: RecallPolicy::default(),
                extraction_policy: ExtractionPolicy::default(),
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
            },
            RepositoryConfigVersion {
                repository_id: "repo-1".into(),
                version: ConfigVersion::INITIAL,
                remote_url: "https://example.test/repo.git".into(),
                credential_binding: Some("credential-1".into()),
                initial_branch: None,
                clone_policy: ClonePolicy::default(),
            },
        )
    }

    #[test]
    fn catalog_versions_and_state_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin.db");
        {
            let store = SqliteAdminStore::open(path.to_str().unwrap()).unwrap();
            let (definition, initial) = memory();
            store.create_memory_store(definition, initial).unwrap();
            let mut second = store
                .memory_config("workspace-a", "memory-1", ConfigVersion(1))
                .unwrap();
            second.version = ConfigVersion(2);
            second.recall_policy.max_results = 25;
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

        let reopened = SqliteAdminStore::open(path.to_str().unwrap()).unwrap();
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
                .is_some()
        );
        assert!(
            reopened
                .memory_config("workspace-a", "memory-1", ConfigVersion(3))
                .is_none()
        );
        assert!(matches!(
            reopened.resolve_repository("workspace-a", "repo-1"),
            Err(ResourceCatalogError::NotActive { .. })
        ));
        assert!(reopened.repository("workspace-b", "repo-1").is_none());
    }

    #[test]
    fn inventory_update_and_legacy_upgrade_are_scoped_and_durable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin.db");
        {
            let store = SqliteAdminStore::open(path.to_str().unwrap()).unwrap();
            store
                .conn
                .lock()
                .unwrap()
                .execute(
                    &format!("INSERT INTO {NS}_memory_store (id, data) VALUES (?1, ?2), (?3, ?4)"),
                    rusqlite::params![
                        "legacy-owned",
                        serde_json::json!({
                            "id": "legacy-owned",
                            "workspace_id": "workspace-a",
                            "name": "Legacy",
                            "description": "old row",
                            "metadata": {"source": "v6"},
                            "archived": false
                        })
                        .to_string(),
                        "legacy-unowned",
                        serde_json::json!({
                            "id": "legacy-unowned",
                            "name": "Quarantined",
                            "archived": false
                        })
                        .to_string(),
                    ],
                )
                .unwrap();
        }

        let store = SqliteAdminStore::open(path.to_str().unwrap()).unwrap();
        let mut migrated = store
            .memory_store("workspace-a", "legacy-owned")
            .expect("owned legacy row migrated");
        assert_eq!(migrated.current_config_version, ConfigVersion::INITIAL);
        assert!(
            store
                .memory_store("workspace-a", "legacy-unowned")
                .is_none()
        );
        assert_eq!(
            store.list_memory_stores("workspace-a")[0].id,
            "legacy-owned"
        );

        migrated.name = "Renamed".into();
        store.update_memory_store(migrated).unwrap();
        drop(store);
        let reopened = SqliteAdminStore::open(path.to_str().unwrap()).unwrap();
        assert_eq!(
            reopened
                .memory_store("workspace-a", "legacy-owned")
                .unwrap()
                .name,
            "Renamed"
        );
        assert!(reopened.list_memory_stores("workspace-b").is_empty());
    }
}
