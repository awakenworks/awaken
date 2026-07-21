//! Postgres Resource Catalog adapter. Each aggregate is a single JSON row;
//! mutations lock that row so the current pointer and immutable history commit
//! atomically across processes.

use std::collections::BTreeMap;

use awaken_resource_contract::{
    ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition, RepositoryConfigVersion,
    RepositoryDefinition, ResourceBindingValidator, ResourceCatalog, ResourceCatalogError,
    ResourceCatalogRules, ResourceConfigSource, ResourceState,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::types::Json;

use crate::postgres::{NS, PostgresAdminStore, block};

const MEMORY: &str = "memory_store";
const REPOSITORY: &str = "repository";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MemoryRecord {
    definition: MemoryStoreDefinition,
    configs: BTreeMap<ConfigVersion, MemoryStoreConfigVersion>,
}

/// Upgrade-only shape written by the removed `MemoryStoreRegistry`. Empty-owner
/// rows remain quarantined because assigning ownership implicitly is unsafe.
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

impl PostgresAdminStore {
    /// Idempotently import owned legacy rows into the Resource Catalog. The old
    /// table is never consulted by normal reads after this startup migration.
    pub(crate) fn migrate_legacy_memory_stores(&self) -> Result<(), ResourceCatalogError> {
        let sql = format!("SELECT data FROM {NS}_memory_store ORDER BY id");
        let pool = self.pool.clone();
        let rows = block(&self.handle, move || async move {
            sqlx::query(&sql)
                .fetch_all(&pool)
                .await
                .map_err(storage)?
                .into_iter()
                .map(|row| {
                    let Json(value): Json<LegacyMemoryStoreDef> =
                        row.try_get("data").map_err(storage)?;
                    Ok(value)
                })
                .collect::<Result<Vec<_>, ResourceCatalogError>>()
        })?;
        for legacy in rows {
            if legacy.workspace_id.trim().is_empty()
                || self
                    .catalog_record::<MemoryRecord>(MEMORY, &legacy.id)
                    .is_some()
            {
                continue;
            }
            let id = legacy.id;
            match self.create_memory_store(
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
            ) {
                Ok(()) | Err(ResourceCatalogError::AlreadyExists(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn catalog_record<T>(&self, kind: &str, id: &str) -> Option<T>
    where
        T: serde::de::DeserializeOwned + Send + 'static,
    {
        let sql = format!("SELECT data FROM {NS}_resource_catalog WHERE kind = $1 AND id = $2");
        let pool = self.pool.clone();
        let kind = kind.to_string();
        let id = id.to_string();
        block(&self.handle, move || async move {
            let row = sqlx::query(&sql)
                .bind(kind)
                .bind(id)
                .fetch_optional(&pool)
                .await
                .expect("read resource catalog")?;
            let Json(value): Json<T> = row.try_get("data").expect("decode resource catalog");
            Some(value)
        })
    }

    fn insert_catalog_record<T: Serialize>(
        &self,
        kind: &str,
        id: &str,
        value: &T,
    ) -> Result<(), ResourceCatalogError> {
        let sql = format!("INSERT INTO {NS}_resource_catalog (kind, id, data) VALUES ($1, $2, $3)");
        let data = serde_json::to_value(value).map_err(storage)?;
        let pool = self.pool.clone();
        let kind = kind.to_string();
        let id = id.to_string();
        let duplicate_id = id.clone();
        let result = block(&self.handle, move || async move {
            sqlx::query(&sql)
                .bind(kind)
                .bind(id)
                .bind(Json(data))
                .execute(&pool)
                .await
        });
        match result {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some("23505") => {
                Err(ResourceCatalogError::AlreadyExists(duplicate_id))
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
        T: serde::de::DeserializeOwned + Serialize + Send + 'static,
        F: FnOnce(&mut T) -> Result<(), ResourceCatalogError> + Send + 'static,
    {
        let select = format!(
            "SELECT data FROM {NS}_resource_catalog WHERE kind = $1 AND id = $2 FOR UPDATE"
        );
        let write =
            format!("UPDATE {NS}_resource_catalog SET data = $3 WHERE kind = $1 AND id = $2");
        let pool = self.pool.clone();
        let kind = kind.to_string();
        let id = id.to_string();
        block(&self.handle, move || async move {
            let mut tx = pool.begin().await.map_err(storage)?;
            let row = sqlx::query(&select)
                .bind(&kind)
                .bind(&id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(storage)?
                .ok_or_else(|| ResourceCatalogError::NotFound(id.clone()))?;
            let Json(mut record): Json<T> = row.try_get("data").map_err(storage)?;
            update(&mut record)?;
            let data = serde_json::to_value(record).map_err(storage)?;
            sqlx::query(&write)
                .bind(kind)
                .bind(id)
                .bind(Json(data))
                .execute(&mut *tx)
                .await
                .map_err(storage)?;
            tx.commit().await.map_err(storage)
        })
    }
}

impl ResourceConfigSource for PostgresAdminStore {
    fn resolve_memory_store(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<MemoryStoreConfigVersion, ResourceCatalogError> {
        let record = self
            .catalog_record::<MemoryRecord>(MEMORY, id)
            .filter(|record| record.definition.workspace_id == workspace_id)
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
            .catalog_record::<RepositoryRecord>(REPOSITORY, id)
            .filter(|record| record.definition.workspace_id == workspace_id)
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

impl ResourceBindingValidator for PostgresAdminStore {
    fn validate_memory_binding(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Result<(), ResourceCatalogError> {
        let record = self
            .catalog_record::<MemoryRecord>(MEMORY, id)
            .filter(|record| record.definition.workspace_id == workspace_id)
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
            .catalog_record::<RepositoryRecord>(REPOSITORY, id)
            .filter(|record| record.definition.workspace_id == workspace_id)
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

impl ResourceCatalog for PostgresAdminStore {
    fn create_memory_store(
        &self,
        definition: MemoryStoreDefinition,
        initial_config: MemoryStoreConfigVersion,
    ) -> Result<(), ResourceCatalogError> {
        ResourceCatalogRules::validate_initial(
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
        let sql = format!("SELECT data FROM {NS}_resource_catalog WHERE kind = $1 ORDER BY id");
        let pool = self.pool.clone();
        let workspace_id = workspace_id.to_string();
        block(&self.handle, move || async move {
            sqlx::query(&sql)
                .bind(MEMORY)
                .fetch_all(&pool)
                .await
                .expect("list resource catalog")
                .into_iter()
                .map(|row| {
                    let Json(record): Json<MemoryRecord> =
                        row.try_get("data").expect("decode resource catalog");
                    record
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
        })
    }

    fn update_memory_store(
        &self,
        definition: MemoryStoreDefinition,
    ) -> Result<(), ResourceCatalogError> {
        let id = definition.id.clone();
        let update_id = id.clone();
        self.update_catalog_record::<MemoryRecord, _>(MEMORY, &id, move |record| {
            if record.definition.workspace_id != definition.workspace_id {
                return Err(ResourceCatalogError::NotFound(update_id));
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
        let workspace_id = workspace_id.to_string();
        let id = config.memory_store_id.clone();
        let update_id = id.clone();
        self.update_catalog_record::<MemoryRecord, _>(MEMORY, &id, move |record| {
            if record.definition.workspace_id != workspace_id {
                return Err(ResourceCatalogError::NotFound(update_id));
            }
            ResourceCatalogRules::validate_publish(
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
        let workspace_id = workspace_id.to_string();
        let update_id = id.to_string();
        self.update_catalog_record::<MemoryRecord, _>(MEMORY, id, move |record| {
            if record.definition.workspace_id != workspace_id {
                return Err(ResourceCatalogError::NotFound(update_id));
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
        ResourceCatalogRules::validate_initial(
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
        let workspace_id = workspace_id.to_string();
        let id = config.repository_id.clone();
        let update_id = id.clone();
        self.update_catalog_record::<RepositoryRecord, _>(REPOSITORY, &id, move |record| {
            if record.definition.workspace_id != workspace_id {
                return Err(ResourceCatalogError::NotFound(update_id));
            }
            ResourceCatalogRules::validate_publish(
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
        let workspace_id = workspace_id.to_string();
        let update_id = id.to_string();
        self.update_catalog_record::<RepositoryRecord, _>(REPOSITORY, id, move |record| {
            if record.definition.workspace_id != workspace_id {
                return Err(ResourceCatalogError::NotFound(update_id));
            }
            record.definition.state = state;
            Ok(())
        })
    }
}
