//! PostgreSQL implementation of the published versioned registry store.

mod helpers;

use async_trait::async_trait;
use awaken_contract::contract::versioned_registry::{
    ConfigRevisionRef, PublishOutcome, RegistryPublication, RegistryResourcePublish, VersionRef,
    VersionedRecord, VersionedRegistryError, VersionedRegistryStore, VersionedResourceState,
    build_rollback_metadata, registry_content_hash,
};
use serde_json::Value;
use sqlx::Row;

use crate::current_millis;
use crate::postgres::PostgresStore;

use helpers::*;

pub(crate) struct RegistryTables {
    pub(crate) resources: String,
    pub(crate) versions: String,
    pub(crate) publications: String,
    pub(crate) publication_entries: String,
}

struct PreparedRegistryResourcePublish {
    resource: RegistryResourcePublish,
    content_hash: String,
    canonical_json_bytes: Vec<u8>,
    canonical_value_json: String,
}

impl RegistryTables {
    pub(crate) fn from_store(store: &PostgresStore) -> Self {
        let prefix = store
            .threads_table
            .strip_suffix("_threads")
            .unwrap_or(&store.threads_table);
        Self {
            resources: format!("{prefix}_registry_resources"),
            versions: format!("{prefix}_registry_versions"),
            publications: format!("{prefix}_registry_publications"),
            publication_entries: format!("{prefix}_registry_publication_entries"),
        }
    }
}

#[async_trait]
impl VersionedRegistryStore for PostgresStore {
    async fn resource_state(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<Option<VersionedResourceState>, VersionedRegistryError> {
        self.ensure_schema().await.map_err(from_storage_error)?;
        let tables = RegistryTables::from_store(self);
        let sql = format!(
            "SELECT scope_id, kind, id, current_version, archived_at_ms, created_at_ms,
                    updated_at_ms, metadata_json
             FROM {}
             WHERE scope_id = $1 AND kind = $2 AND id = $3",
            tables.resources
        );
        let row = sqlx::query(&sql)
            .bind(scope_id)
            .bind(kind)
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(to_registry_error)?;
        Ok(row.map(resource_from_row))
    }

    async fn current(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<Option<VersionedRecord<Value>>, VersionedRegistryError> {
        self.ensure_schema().await.map_err(from_storage_error)?;
        let tables = RegistryTables::from_store(self);
        let sql = format!(
            "SELECT v.kind, v.id, v.version, v.content_hash, v.value_schema_version,
                    v.value_json, v.canonical_value_json, v.created_at_ms, v.metadata_json
             FROM {} r
             JOIN {} v
               ON v.scope_id = r.scope_id
              AND v.kind = r.kind
              AND v.id = r.id
              AND v.version = r.current_version
             WHERE r.scope_id = $1 AND r.kind = $2 AND r.id = $3",
            tables.resources, tables.versions
        );
        let row = sqlx::query(&sql)
            .bind(scope_id)
            .bind(kind)
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(to_registry_error)?;
        Ok(row.map(record_from_row))
    }

    async fn get(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
        version: u64,
    ) -> Result<Option<VersionedRecord<Value>>, VersionedRegistryError> {
        self.ensure_schema().await.map_err(from_storage_error)?;
        let tables = RegistryTables::from_store(self);
        let sql = format!(
            "SELECT kind, id, version, content_hash, value_schema_version, value_json,
                    canonical_value_json, created_at_ms, metadata_json
             FROM {}
             WHERE scope_id = $1 AND kind = $2 AND id = $3 AND version = $4",
            tables.versions
        );
        let row = sqlx::query(&sql)
            .bind(scope_id)
            .bind(kind)
            .bind(id)
            .bind(version as i64)
            .fetch_optional(&self.pool)
            .await
            .map_err(to_registry_error)?;
        Ok(row.map(record_from_row))
    }

    async fn list_versions(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<Vec<VersionedRecord<Value>>, VersionedRegistryError> {
        self.ensure_schema().await.map_err(from_storage_error)?;
        let tables = RegistryTables::from_store(self);
        let sql = format!(
            "SELECT kind, id, version, content_hash, value_schema_version, value_json,
                    canonical_value_json, created_at_ms, metadata_json
             FROM {}
             WHERE scope_id = $1 AND kind = $2 AND id = $3
             ORDER BY version ASC",
            tables.versions
        );
        let rows = sqlx::query(&sql)
            .bind(scope_id)
            .bind(kind)
            .bind(id)
            .fetch_all(&self.pool)
            .await
            .map_err(to_registry_error)?;
        Ok(rows.into_iter().map(record_from_row).collect())
    }

    async fn publish_resource(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
        value: Value,
        value_schema_version: u32,
        metadata: Value,
    ) -> Result<PublishOutcome<Value>, VersionedRegistryError> {
        self.ensure_schema().await.map_err(from_storage_error)?;
        let tables = RegistryTables::from_store(self);
        let (content_hash, canonical_json_bytes) =
            registry_content_hash(value_schema_version, &value)?;
        let canonical_value_json = canonical_json_string(&canonical_json_bytes)?;
        let now = current_millis();
        let mut tx = self.pool.begin().await.map_err(to_registry_error)?;

        let state = load_resource_state_tx(&mut tx, &tables, scope_id, kind, id, true).await?;
        if let Some(state) = &state {
            state.ensure_not_archived(kind, id)?;
        }
        if let Some(current_version) = state.as_ref().and_then(|state| state.current_version) {
            let current =
                load_version_tx(&mut tx, &tables, scope_id, kind, id, current_version).await?;
            if let Some(current) = current
                && current.content_hash == content_hash
            {
                tx.commit().await.map_err(to_registry_error)?;
                return Ok(PublishOutcome::Noop(current));
            }
        }

        let version = next_version_tx(&mut tx, &tables, scope_id, kind, id).await?;
        let record = VersionedRecord {
            kind: kind.to_string(),
            id: id.to_string(),
            version,
            content_hash,
            value_schema_version,
            value,
            canonical_json_bytes,
            created_at_ms: now,
            metadata,
        };
        insert_version_tx(&mut tx, &tables, scope_id, &record, &canonical_value_json).await?;
        upsert_resource_state_tx(&mut tx, &tables, scope_id, &record, state, now).await?;
        tx.commit().await.map_err(to_registry_error)?;
        Ok(PublishOutcome::Created(record))
    }

    async fn rollback_resource(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
        to_version: u64,
        metadata: Value,
    ) -> Result<VersionedRecord<Value>, VersionedRegistryError> {
        self.ensure_schema().await.map_err(from_storage_error)?;
        let tables = RegistryTables::from_store(self);
        let now = current_millis();
        let mut tx = self.pool.begin().await.map_err(to_registry_error)?;
        let state = load_resource_state_tx(&mut tx, &tables, scope_id, kind, id, true).await?;
        if let Some(state) = &state {
            state.ensure_not_archived(kind, id)?;
        }
        let prior = load_version_tx(&mut tx, &tables, scope_id, kind, id, to_version)
            .await?
            .ok_or_else(|| VersionedRegistryError::NotFound(version_name(kind, id, to_version)))?;
        let metadata = build_rollback_metadata(metadata, to_version)?;
        let (content_hash, canonical_json_bytes) =
            registry_content_hash(prior.value_schema_version, &prior.value)?;
        let canonical_value_json = canonical_json_string(&canonical_json_bytes)?;
        let version = next_version_tx(&mut tx, &tables, scope_id, kind, id).await?;
        let record = VersionedRecord {
            kind: kind.to_string(),
            id: id.to_string(),
            version,
            content_hash,
            value_schema_version: prior.value_schema_version,
            value: prior.value,
            canonical_json_bytes,
            created_at_ms: now,
            metadata,
        };
        insert_version_tx(&mut tx, &tables, scope_id, &record, &canonical_value_json).await?;
        upsert_resource_state_tx(&mut tx, &tables, scope_id, &record, state, now).await?;
        tx.commit().await.map_err(to_registry_error)?;
        Ok(record)
    }

    async fn archive_resource(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<(), VersionedRegistryError> {
        self.ensure_schema().await.map_err(from_storage_error)?;
        let tables = RegistryTables::from_store(self);
        let now = current_millis();
        let sql = format!(
            "UPDATE {}
             SET archived_at_ms = COALESCE(archived_at_ms, $4), updated_at_ms = $4
             WHERE scope_id = $1 AND kind = $2 AND id = $3",
            tables.resources
        );
        let result = sqlx::query(&sql)
            .bind(scope_id)
            .bind(kind)
            .bind(id)
            .bind(now as i64)
            .execute(&self.pool)
            .await
            .map_err(to_registry_error)?;
        if result.rows_affected() == 0 {
            return Err(VersionedRegistryError::NotFound(resource_name(kind, id)));
        }
        Ok(())
    }

    async fn unarchive_resource(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<(), VersionedRegistryError> {
        self.ensure_schema().await.map_err(from_storage_error)?;
        let tables = RegistryTables::from_store(self);
        let now = current_millis();
        let sql = format!(
            "UPDATE {}
             SET archived_at_ms = NULL, updated_at_ms = $4
             WHERE scope_id = $1 AND kind = $2 AND id = $3",
            tables.resources
        );
        let result = sqlx::query(&sql)
            .bind(scope_id)
            .bind(kind)
            .bind(id)
            .bind(now as i64)
            .execute(&self.pool)
            .await
            .map_err(to_registry_error)?;
        if result.rows_affected() == 0 {
            return Err(VersionedRegistryError::NotFound(resource_name(kind, id)));
        }
        Ok(())
    }

    async fn publish_resources_and_create_publication(
        &self,
        scope_id: &str,
        publication_id: &str,
        resources: Vec<RegistryResourcePublish>,
        source_config_revisions: Vec<ConfigRevisionRef>,
        created_by: Option<String>,
        metadata: Value,
    ) -> Result<RegistryPublication, VersionedRegistryError> {
        validate_resource_publication_request(publication_id, &resources)?;
        self.ensure_schema().await.map_err(from_storage_error)?;
        let tables = RegistryTables::from_store(self);
        let prepared = resources
            .into_iter()
            .map(|resource| {
                let (content_hash, canonical_json_bytes) =
                    registry_content_hash(resource.value_schema_version, &resource.value)?;
                let canonical_value_json = canonical_json_string(&canonical_json_bytes)?;
                Ok(PreparedRegistryResourcePublish {
                    resource,
                    content_hash,
                    canonical_json_bytes,
                    canonical_value_json,
                })
            })
            .collect::<Result<Vec<_>, VersionedRegistryError>>()?;
        let now = current_millis();
        let mut tx = self.pool.begin().await.map_err(to_registry_error)?;

        sqlx::query(&format!(
            "LOCK TABLE {}, {}, {} IN EXCLUSIVE MODE",
            tables.resources, tables.versions, tables.publications
        ))
        .execute(&mut *tx)
        .await
        .map_err(to_registry_error)?;

        let duplicate_sql = format!(
            "SELECT 1 FROM {} WHERE scope_id = $1 AND publication_id = $2 LIMIT 1",
            tables.publications
        );
        if sqlx::query(&duplicate_sql)
            .bind(scope_id)
            .bind(publication_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(to_registry_error)?
            .is_some()
        {
            return Err(VersionedRegistryError::AlreadyExists(format!(
                "publication/{publication_id}"
            )));
        }

        let mut entry_hashes = Vec::with_capacity(prepared.len());
        for prepared in prepared {
            let state = load_resource_state_tx(
                &mut tx,
                &tables,
                scope_id,
                &prepared.resource.kind,
                &prepared.resource.id,
                true,
            )
            .await?;
            if let Some(state) = &state
                && state.archived_at_ms.is_some()
            {
                return Err(VersionedRegistryError::Archived {
                    kind: prepared.resource.kind,
                    id: prepared.resource.id,
                });
            }

            let record = if let Some(current_version) =
                state.as_ref().and_then(|state| state.current_version)
            {
                let current = load_version_tx(
                    &mut tx,
                    &tables,
                    scope_id,
                    &prepared.resource.kind,
                    &prepared.resource.id,
                    current_version,
                )
                .await?;
                if let Some(current) = current {
                    if current.content_hash == prepared.content_hash {
                        current
                    } else {
                        publish_new_resource_version_tx(
                            &mut tx, &tables, scope_id, prepared, state, now,
                        )
                        .await?
                    }
                } else {
                    publish_new_resource_version_tx(
                        &mut tx, &tables, scope_id, prepared, state, now,
                    )
                    .await?
                }
            } else {
                publish_new_resource_version_tx(&mut tx, &tables, scope_id, prepared, state, now)
                    .await?
            };
            entry_hashes.push((
                VersionRef {
                    kind: record.kind,
                    id: record.id,
                    version: record.version,
                },
                record.content_hash,
            ));
        }

        let snapshot_sql = format!(
            "SELECT COALESCE(MAX(snapshot_version), 0) + 1 AS snapshot_version
             FROM {} WHERE scope_id = $1",
            tables.publications
        );
        let snapshot_row = sqlx::query(&snapshot_sql)
            .bind(scope_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(to_registry_error)?;
        let snapshot_version = i64_to_u64(snapshot_row.get("snapshot_version"));
        let source_revisions = serde_json::to_value(&source_config_revisions)
            .map_err(|error| VersionedRegistryError::Serialization(error.to_string()))?;
        let insert_pub_sql = format!(
            "INSERT INTO {} (scope_id, snapshot_version, publication_id,
                source_config_revisions_json, created_by, metadata_json, created_at_ms)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            tables.publications
        );
        sqlx::query(&insert_pub_sql)
            .bind(scope_id)
            .bind(snapshot_version as i64)
            .bind(publication_id)
            .bind(&source_revisions)
            .bind(&created_by)
            .bind(&metadata)
            .bind(now as i64)
            .execute(&mut *tx)
            .await
            .map_err(to_registry_error)?;

        let insert_entry_sql = format!(
            "INSERT INTO {} (scope_id, snapshot_version, kind, id, version, content_hash)
             VALUES ($1, $2, $3, $4, $5, $6)",
            tables.publication_entries
        );
        let mut entries = Vec::with_capacity(entry_hashes.len());
        for (entry, content_hash) in &entry_hashes {
            sqlx::query(&insert_entry_sql)
                .bind(scope_id)
                .bind(snapshot_version as i64)
                .bind(&entry.kind)
                .bind(&entry.id)
                .bind(entry.version as i64)
                .bind(content_hash)
                .execute(&mut *tx)
                .await
                .map_err(to_registry_error)?;
            entries.push(entry.clone());
        }
        tx.commit().await.map_err(to_registry_error)?;
        Ok(RegistryPublication {
            publication_id: publication_id.to_string(),
            scope_id: scope_id.to_string(),
            snapshot_version,
            entries,
            source_config_revisions,
            created_by,
            created_at_ms: now,
            metadata,
        })
    }

    async fn create_publication(
        &self,
        scope_id: &str,
        publication_id: &str,
        entries: Vec<VersionRef>,
        source_config_revisions: Vec<ConfigRevisionRef>,
        created_by: Option<String>,
        metadata: Value,
    ) -> Result<RegistryPublication, VersionedRegistryError> {
        validate_publication_request(publication_id, &entries)?;
        self.ensure_schema().await.map_err(from_storage_error)?;
        let tables = RegistryTables::from_store(self);
        let now = current_millis();
        let mut tx = self.pool.begin().await.map_err(to_registry_error)?;

        // ADR-0035 D6/D11: lock resources/versions in addition to
        // publications so a concurrent `publish_resource` cannot advance
        // or archive a resource while we are still reading entry metadata
        // for this publication. SHARE mode lets concurrent readers proceed
        // but blocks the writer locks taken by publish_resource and
        // archive_resource.
        for table in [
            tables.publications.as_str(),
            tables.resources.as_str(),
            tables.versions.as_str(),
            tables.publication_entries.as_str(),
        ] {
            sqlx::query(&format!("LOCK TABLE {table} IN SHARE ROW EXCLUSIVE MODE"))
                .execute(&mut *tx)
                .await
                .map_err(to_registry_error)?;
        }
        let duplicate_sql = format!(
            "SELECT 1 FROM {} WHERE scope_id = $1 AND publication_id = $2 LIMIT 1",
            tables.publications
        );
        if sqlx::query(&duplicate_sql)
            .bind(scope_id)
            .bind(publication_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(to_registry_error)?
            .is_some()
        {
            return Err(VersionedRegistryError::AlreadyExists(format!(
                "publication/{publication_id}"
            )));
        }

        let mut entry_hashes = Vec::with_capacity(entries.len());
        for entry in &entries {
            let state =
                load_resource_state_tx(&mut tx, &tables, scope_id, &entry.kind, &entry.id, true)
                    .await?
                    .ok_or_else(|| {
                        VersionedRegistryError::NotFound(resource_name(&entry.kind, &entry.id))
                    })?;
            if state.archived_at_ms.is_some() {
                return Err(VersionedRegistryError::Archived {
                    kind: entry.kind.clone(),
                    id: entry.id.clone(),
                });
            }
            let record = load_version_tx(
                &mut tx,
                &tables,
                scope_id,
                &entry.kind,
                &entry.id,
                entry.version,
            )
            .await?
            .ok_or_else(|| {
                VersionedRegistryError::NotFound(version_name(
                    &entry.kind,
                    &entry.id,
                    entry.version,
                ))
            })?;
            entry_hashes.push((entry.clone(), record.content_hash));
        }

        let snapshot_sql = format!(
            "SELECT COALESCE(MAX(snapshot_version), 0) + 1 AS snapshot_version
             FROM {} WHERE scope_id = $1",
            tables.publications
        );
        let snapshot_row = sqlx::query(&snapshot_sql)
            .bind(scope_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(to_registry_error)?;
        let snapshot_version = i64_to_u64(snapshot_row.get("snapshot_version"));
        let source_revisions = serde_json::to_value(&source_config_revisions)
            .map_err(|error| VersionedRegistryError::Serialization(error.to_string()))?;
        let insert_pub_sql = format!(
            "INSERT INTO {} (scope_id, snapshot_version, publication_id,
                source_config_revisions_json, created_by, metadata_json, created_at_ms)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            tables.publications
        );
        sqlx::query(&insert_pub_sql)
            .bind(scope_id)
            .bind(snapshot_version as i64)
            .bind(publication_id)
            .bind(&source_revisions)
            .bind(&created_by)
            .bind(&metadata)
            .bind(now as i64)
            .execute(&mut *tx)
            .await
            .map_err(to_registry_error)?;

        let insert_entry_sql = format!(
            "INSERT INTO {} (scope_id, snapshot_version, kind, id, version, content_hash)
             VALUES ($1, $2, $3, $4, $5, $6)",
            tables.publication_entries
        );
        for (entry, content_hash) in &entry_hashes {
            sqlx::query(&insert_entry_sql)
                .bind(scope_id)
                .bind(snapshot_version as i64)
                .bind(&entry.kind)
                .bind(&entry.id)
                .bind(entry.version as i64)
                .bind(content_hash)
                .execute(&mut *tx)
                .await
                .map_err(to_registry_error)?;
        }
        tx.commit().await.map_err(to_registry_error)?;
        Ok(RegistryPublication {
            publication_id: publication_id.to_string(),
            scope_id: scope_id.to_string(),
            snapshot_version,
            entries,
            source_config_revisions,
            created_by,
            created_at_ms: now,
            metadata,
        })
    }

    async fn latest_publication(
        &self,
        scope_id: &str,
    ) -> Result<Option<RegistryPublication>, VersionedRegistryError> {
        self.ensure_schema().await.map_err(from_storage_error)?;
        let tables = RegistryTables::from_store(self);
        let sql = format!(
            "SELECT scope_id, snapshot_version, publication_id,
                    source_config_revisions_json, created_by, metadata_json, created_at_ms
             FROM {}
             WHERE scope_id = $1
             ORDER BY snapshot_version DESC
             LIMIT 1",
            tables.publications
        );
        let row = sqlx::query(&sql)
            .bind(scope_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(to_registry_error)?;
        load_publication_with_entries(self, &tables, row).await
    }

    async fn get_publication(
        &self,
        scope_id: &str,
        snapshot_version: u64,
    ) -> Result<Option<RegistryPublication>, VersionedRegistryError> {
        self.ensure_schema().await.map_err(from_storage_error)?;
        let tables = RegistryTables::from_store(self);
        let sql = format!(
            "SELECT scope_id, snapshot_version, publication_id,
                    source_config_revisions_json, created_by, metadata_json, created_at_ms
             FROM {}
             WHERE scope_id = $1 AND snapshot_version = $2",
            tables.publications
        );
        let row = sqlx::query(&sql)
            .bind(scope_id)
            .bind(snapshot_version as i64)
            .fetch_optional(&self.pool)
            .await
            .map_err(to_registry_error)?;
        load_publication_with_entries(self, &tables, row).await
    }
}
