//! Resources-owned Postgres Resource Catalog adapter. Each aggregate is a single JSON row;
//! mutations lock that row so the current pointer and immutable history commit
//! atomically across processes.

use std::collections::BTreeMap;

use awaken_resource_contract::{
    ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition, RepositoryConfigVersion,
    RepositoryDefinition, ResourceBindingValidator, ResourceCatalog, ResourceCatalogError,
    ResourceCatalogRules, ResourceConfigSource, ResourceState, ResourceTimestamps,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::types::Json;

use crate::postgres::{PostgresResourceStore, block};
use crate::resource_catalog_codec::{MemoryRecord, RepositoryRecord, now_nanos};
use crate::schema::CATALOG_NS;

const MEMORY: &str = "memory_store";
const REPOSITORY: &str = "repository";

/// Upgrade-only shape written by the removed Control-owned MemoryStore registry.
/// It is read during schema migration only; normal reads have one Resource Catalog.
#[derive(Debug, Deserialize)]
struct LegacyMemoryStoreDefinition {
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

fn storage(error: impl ToString) -> ResourceCatalogError {
    ResourceCatalogError::Storage(error.to_string())
}

impl PostgresResourceStore {
    /// Idempotently import owned rows from the removed Control registry when the
    /// legacy and Resources stores share one PostgreSQL database. Deployments
    /// with separate databases have no legacy table here and take the no-op arm.
    pub(crate) async fn migrate_legacy_memory_stores(&self) -> Result<(), ResourceCatalogError> {
        let legacy_table: Option<String> =
            sqlx::query_scalar("SELECT to_regclass('admin_memory_store')::text")
                .fetch_one(&self.pool)
                .await
                .map_err(storage)?;
        if legacy_table.is_none() {
            return Ok(());
        }
        let rows = sqlx::query("SELECT id, data FROM admin_memory_store ORDER BY id")
            .fetch_all(&self.pool)
            .await
            .map_err(storage)?;
        for row in rows {
            let row_id: String = row.try_get("id").map_err(storage)?;
            let Json(legacy): Json<LegacyMemoryStoreDefinition> =
                row.try_get("data").map_err(storage)?;
            if legacy.id != row_id {
                return Err(ResourceCatalogError::Storage(format!(
                    "legacy MemoryStore row `{row_id}` contains id `{}`",
                    legacy.id
                )));
            }
            if legacy.workspace_id.trim().is_empty() {
                continue;
            }
            let state = if legacy.archived {
                ResourceState::Archived
            } else {
                ResourceState::Active
            };
            let at = now_nanos();
            let mut timestamps = ResourceTimestamps::created(at);
            timestamps.transition_to(state, at);
            let id: awaken_resource_contract::MemoryStoreId = legacy.id.into();
            match self.create_memory_store(
                MemoryStoreDefinition {
                    id: id.clone(),
                    workspace_id: legacy.workspace_id,
                    name: legacy.name,
                    description: legacy.description,
                    metadata: legacy.metadata,
                    state,
                    current_config_version: ConfigVersion::INITIAL,
                    timestamps,
                },
                MemoryStoreConfigVersion {
                    memory_store_id: id,
                    version: ConfigVersion::INITIAL,
                    retention_policy: Default::default(),
                },
            ) {
                Ok(()) | Err(ResourceCatalogError::AlreadyExists(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn catalog_record<T>(&self, kind: &str, id: &str) -> Result<Option<T>, ResourceCatalogError>
    where
        T: serde::de::DeserializeOwned + Send + 'static,
    {
        let sql = format!("SELECT data FROM {CATALOG_NS}_entry WHERE kind = $1 AND id = $2");
        let pool = self.pool.clone();
        let kind = kind.to_string();
        let id = id.to_string();
        block(&self.handle, move || async move {
            let row = sqlx::query(&sql)
                .bind(kind)
                .bind(id)
                .fetch_optional(&pool)
                .await
                .map_err(storage)?;
            row.map(|row| {
                let Json(value): Json<T> = row.try_get("data").map_err(storage)?;
                Ok(value)
            })
            .transpose()
        })
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
        let sql = format!("INSERT INTO {CATALOG_NS}_entry (kind, id, data) VALUES ($1, $2, $3)");
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
        let select =
            format!("SELECT data FROM {CATALOG_NS}_entry WHERE kind = $1 AND id = $2 FOR UPDATE");
        let write = format!("UPDATE {CATALOG_NS}_entry SET data = $3 WHERE kind = $1 AND id = $2");
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

impl ResourceConfigSource for PostgresResourceStore {
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

impl ResourceBindingValidator for PostgresResourceStore {
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

impl ResourceCatalog for PostgresResourceStore {
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
        let sql = format!("SELECT id, data FROM {CATALOG_NS}_entry WHERE kind = $1 ORDER BY id");
        let pool = self.pool.clone();
        let workspace_id = workspace_id.to_string();
        block(&self.handle, move || async move {
            let rows = sqlx::query(&sql)
                .bind(MEMORY)
                .fetch_all(&pool)
                .await
                .map_err(storage)?;
            rows.into_iter()
                .map(|row| {
                    let id: String = row.try_get("id").map_err(storage)?;
                    let Json(record): Json<MemoryRecord> = row.try_get("data").map_err(storage)?;
                    ResourceCatalogRules::validate_memory_aggregate(
                        &id,
                        &record.definition,
                        &record.configs,
                    )?;
                    Ok(record)
                })
                .collect::<Result<Vec<_>, ResourceCatalogError>>()
                .map(|records| {
                    records
                        .into_iter()
                        .filter(|record| {
                            record.definition.workspace_id.as_str() == workspace_id
                                && !matches!(
                                    record.definition.state,
                                    ResourceState::Archived | ResourceState::Deleted
                                )
                        })
                        .map(|record| record.definition)
                        .collect()
                })
        })
    }

    fn update_memory_store(
        &self,
        definition: MemoryStoreDefinition,
    ) -> Result<(), ResourceCatalogError> {
        let id = definition.id.clone();
        let update_id = id.clone();
        self.update_catalog_record::<MemoryRecord, _>(MEMORY, id.as_str(), move |record| {
            if record.definition.workspace_id != definition.workspace_id {
                return Err(ResourceCatalogError::NotFound(update_id.to_string()));
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
        let workspace_id = workspace_id.to_string();
        let id = config.memory_store_id.clone();
        let update_id = id.clone();
        self.update_catalog_record::<MemoryRecord, _>(MEMORY, id.as_str(), move |record| {
            if record.definition.workspace_id.as_str() != workspace_id {
                return Err(ResourceCatalogError::NotFound(update_id.to_string()));
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
        let workspace_id = workspace_id.to_string();
        let update_id = id.to_string();
        self.update_catalog_record::<MemoryRecord, _>(MEMORY, id, move |record| {
            if record.definition.workspace_id.as_str() != workspace_id {
                return Err(ResourceCatalogError::NotFound(update_id));
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
        let workspace_id = workspace_id.to_string();
        let id = config.repository_id.clone();
        let update_id = id.clone();
        self.update_catalog_record::<RepositoryRecord, _>(REPOSITORY, id.as_str(), move |record| {
            if record.definition.workspace_id.as_str() != workspace_id {
                return Err(ResourceCatalogError::NotFound(update_id.to_string()));
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
        let workspace_id = workspace_id.to_string();
        let update_id = id.to_string();
        self.update_catalog_record::<RepositoryRecord, _>(REPOSITORY, id, move |record| {
            if record.definition.workspace_id.as_str() != workspace_id {
                return Err(ResourceCatalogError::NotFound(update_id));
            }
            record.definition.state = state;
            record.definition.timestamps.transition_to(state, at);
            Ok(())
        })
    }
}
