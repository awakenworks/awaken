use std::collections::HashSet;

use awaken_server_contract::contract::storage::StorageError;
use awaken_server_contract::contract::versioned_registry::{
    ConfigRevisionRef, RegistryPublication, RegistryResourcePublish, VersionRef, VersionedRecord,
    VersionedRegistryError, VersionedResourceState,
};
use serde_json::Value;
use sqlx::postgres::PgRow;
use sqlx::{Postgres, Row, Transaction};

use crate::postgres::PostgresStore;

use super::{PreparedRegistryResourcePublish, RegistryTables};

pub(super) async fn load_resource_state_tx(
    tx: &mut Transaction<'_, Postgres>,
    tables: &RegistryTables,
    scope_id: &str,
    kind: &str,
    id: &str,
    for_update: bool,
) -> Result<Option<VersionedResourceState>, VersionedRegistryError> {
    let suffix = if for_update { " FOR UPDATE" } else { "" };
    let sql = format!(
        "SELECT scope_id, kind, id, current_version, archived_at_ms, created_at_ms,
                updated_at_ms, metadata_json
         FROM {}
         WHERE scope_id = $1 AND kind = $2 AND id = $3{}",
        tables.resources, suffix
    );
    let row = sqlx::query(&sql)
        .bind(scope_id)
        .bind(kind)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(to_registry_error)?;
    Ok(row.map(resource_from_row))
}

pub(super) async fn load_version_tx(
    tx: &mut Transaction<'_, Postgres>,
    tables: &RegistryTables,
    scope_id: &str,
    kind: &str,
    id: &str,
    version: u64,
) -> Result<Option<VersionedRecord<Value>>, VersionedRegistryError> {
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
        .fetch_optional(&mut **tx)
        .await
        .map_err(to_registry_error)?;
    Ok(row.map(record_from_row))
}

pub(super) async fn next_version_tx(
    tx: &mut Transaction<'_, Postgres>,
    tables: &RegistryTables,
    scope_id: &str,
    kind: &str,
    id: &str,
) -> Result<u64, VersionedRegistryError> {
    let sql = format!(
        "SELECT COALESCE(MAX(version), 0) + 1 AS next_version
         FROM {}
         WHERE scope_id = $1 AND kind = $2 AND id = $3",
        tables.versions
    );
    let row = sqlx::query(&sql)
        .bind(scope_id)
        .bind(kind)
        .bind(id)
        .fetch_one(&mut **tx)
        .await
        .map_err(to_registry_error)?;
    let next_version: i64 = row.get("next_version");
    Ok(next_version as u64)
}

pub(super) async fn insert_version_tx(
    tx: &mut Transaction<'_, Postgres>,
    tables: &RegistryTables,
    scope_id: &str,
    record: &VersionedRecord<Value>,
    canonical_value_json: &str,
) -> Result<(), VersionedRegistryError> {
    let sql = format!(
        "INSERT INTO {} (
            scope_id, kind, id, version, content_hash, value_schema_version,
            canonical_value_json, value_json, metadata_json, created_at_ms
         )
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        tables.versions
    );
    sqlx::query(&sql)
        .bind(scope_id)
        .bind(&record.kind)
        .bind(&record.id)
        .bind(record.version as i64)
        .bind(&record.content_hash)
        .bind(record.value_schema_version as i32)
        .bind(canonical_value_json)
        .bind(&record.value)
        .bind(&record.metadata)
        .bind(record.created_at_ms as i64)
        .execute(&mut **tx)
        .await
        .map_err(to_registry_error)?;
    Ok(())
}

pub(super) async fn upsert_resource_state_tx(
    tx: &mut Transaction<'_, Postgres>,
    tables: &RegistryTables,
    scope_id: &str,
    record: &VersionedRecord<Value>,
    previous: Option<VersionedResourceState>,
    now: u64,
) -> Result<(), VersionedRegistryError> {
    let created_at = previous.map_or(now, |state| state.created_at_ms);
    let sql = format!(
        "INSERT INTO {} (
            scope_id, kind, id, current_version, archived_at_ms,
            created_at_ms, updated_at_ms, metadata_json
         )
         VALUES ($1, $2, $3, $4, NULL, $5, $6, $7)
         ON CONFLICT (scope_id, kind, id) DO UPDATE SET
            current_version = $4,
            updated_at_ms = $6,
            metadata_json = $7",
        tables.resources
    );
    sqlx::query(&sql)
        .bind(scope_id)
        .bind(&record.kind)
        .bind(&record.id)
        .bind(record.version as i64)
        .bind(created_at as i64)
        .bind(now as i64)
        .bind(&record.metadata)
        .execute(&mut **tx)
        .await
        .map_err(to_registry_error)?;
    Ok(())
}

pub(super) async fn publish_new_resource_version_tx(
    tx: &mut Transaction<'_, Postgres>,
    tables: &RegistryTables,
    scope_id: &str,
    prepared: PreparedRegistryResourcePublish,
    previous: Option<VersionedResourceState>,
    now: u64,
) -> Result<VersionedRecord<Value>, VersionedRegistryError> {
    let version = next_version_tx(
        tx,
        tables,
        scope_id,
        &prepared.resource.kind,
        &prepared.resource.id,
    )
    .await?;
    let record = VersionedRecord {
        kind: prepared.resource.kind,
        id: prepared.resource.id,
        version,
        content_hash: prepared.content_hash,
        value_schema_version: prepared.resource.value_schema_version,
        value: prepared.resource.value,
        canonical_json_bytes: prepared.canonical_json_bytes,
        created_at_ms: now,
        metadata: prepared.resource.metadata,
    };
    insert_version_tx(
        tx,
        tables,
        scope_id,
        &record,
        &prepared.canonical_value_json,
    )
    .await?;
    upsert_resource_state_tx(tx, tables, scope_id, &record, previous, now).await?;
    Ok(record)
}

pub(super) async fn load_publication_with_entries(
    store: &PostgresStore,
    tables: &RegistryTables,
    row: Option<PgRow>,
) -> Result<Option<RegistryPublication>, VersionedRegistryError> {
    let Some(row) = row else {
        return Ok(None);
    };
    let scope_id: String = row.get("scope_id");
    let snapshot_version_i64: i64 = row.get("snapshot_version");
    let entries_sql = format!(
        "SELECT kind, id, version, content_hash
         FROM {}
         WHERE scope_id = $1 AND snapshot_version = $2
         ORDER BY kind ASC, id ASC",
        tables.publication_entries
    );
    let entries = sqlx::query(&entries_sql)
        .bind(&scope_id)
        .bind(snapshot_version_i64)
        .fetch_all(&store.pool)
        .await
        .map_err(to_registry_error)?
        .into_iter()
        .map(|entry| VersionRef {
            kind: entry.get("kind"),
            id: entry.get("id"),
            version: i64_to_u64(entry.get("version")),
        })
        .collect();
    let source_config_revisions: Value = row.get("source_config_revisions_json");
    let source_config_revisions =
        serde_json::from_value::<Vec<ConfigRevisionRef>>(source_config_revisions)
            .map_err(|error| VersionedRegistryError::Serialization(error.to_string()))?;
    Ok(Some(RegistryPublication {
        publication_id: row.get("publication_id"),
        scope_id,
        snapshot_version: snapshot_version_i64 as u64,
        entries,
        source_config_revisions,
        created_by: row.get("created_by"),
        created_at_ms: i64_to_u64(row.get("created_at_ms")),
        metadata: row.get("metadata_json"),
    }))
}

pub(super) fn validate_resource_publication_request(
    publication_id: &str,
    resources: &[RegistryResourcePublish],
) -> Result<(), VersionedRegistryError> {
    if publication_id.trim().is_empty() {
        return Err(VersionedRegistryError::InvalidRequest(
            "publication_id cannot be empty".to_string(),
        ));
    }
    if resources.is_empty() {
        return Err(VersionedRegistryError::InvalidRequest(
            "publication resources cannot be empty".to_string(),
        ));
    }
    let mut seen = HashSet::new();
    for resource in resources {
        if resource.kind.trim().is_empty() || resource.id.trim().is_empty() {
            return Err(VersionedRegistryError::InvalidRequest(
                "publication resource kind and id cannot be empty".to_string(),
            ));
        }
        if !seen.insert((resource.kind.as_str(), resource.id.as_str())) {
            return Err(VersionedRegistryError::InvalidRequest(format!(
                "duplicate publication resource {}",
                resource_name(&resource.kind, &resource.id)
            )));
        }
    }
    Ok(())
}

pub(super) fn validate_publication_request(
    publication_id: &str,
    entries: &[VersionRef],
) -> Result<(), VersionedRegistryError> {
    if publication_id.trim().is_empty() {
        return Err(VersionedRegistryError::InvalidRequest(
            "publication_id cannot be empty".to_string(),
        ));
    }
    if entries.is_empty() {
        return Err(VersionedRegistryError::InvalidRequest(
            "publication entries cannot be empty".to_string(),
        ));
    }
    let mut seen = HashSet::new();
    for entry in entries {
        if entry.version == 0 {
            return Err(VersionedRegistryError::InvalidRequest(format!(
                "{} version cannot be 0",
                resource_name(&entry.kind, &entry.id)
            )));
        }
        if !seen.insert((entry.kind.as_str(), entry.id.as_str())) {
            return Err(VersionedRegistryError::InvalidRequest(format!(
                "duplicate publication entry {}",
                resource_name(&entry.kind, &entry.id)
            )));
        }
    }
    Ok(())
}

pub(super) fn resource_from_row(row: PgRow) -> VersionedResourceState {
    VersionedResourceState {
        scope_id: row.get("scope_id"),
        kind: row.get("kind"),
        id: row.get("id"),
        current_version: row.get::<Option<i64>, _>("current_version").map(i64_to_u64),
        archived_at_ms: row.get::<Option<i64>, _>("archived_at_ms").map(i64_to_u64),
        created_at_ms: i64_to_u64(row.get("created_at_ms")),
        updated_at_ms: i64_to_u64(row.get("updated_at_ms")),
        metadata: row.get("metadata_json"),
    }
}

pub(super) fn record_from_row(row: PgRow) -> VersionedRecord<Value> {
    let canonical_value_json: String = row.get("canonical_value_json");
    VersionedRecord {
        kind: row.get("kind"),
        id: row.get("id"),
        version: i64_to_u64(row.get("version")),
        content_hash: row.get("content_hash"),
        value_schema_version: row.get::<i32, _>("value_schema_version") as u32,
        value: row.get("value_json"),
        canonical_json_bytes: canonical_value_json.into_bytes(),
        created_at_ms: i64_to_u64(row.get("created_at_ms")),
        metadata: row.get("metadata_json"),
    }
}

pub(super) fn canonical_json_string(bytes: &[u8]) -> Result<String, VersionedRegistryError> {
    String::from_utf8(bytes.to_vec())
        .map_err(|error| VersionedRegistryError::Serialization(error.to_string()))
}

pub(super) fn i64_to_u64(value: i64) -> u64 {
    value.try_into().unwrap_or_default()
}

pub(super) fn from_storage_error(error: StorageError) -> VersionedRegistryError {
    match error {
        StorageError::Serialization(message) => VersionedRegistryError::Serialization(message),
        other => VersionedRegistryError::Backend(other.to_string()),
    }
}

pub(super) fn to_registry_error(error: sqlx::Error) -> VersionedRegistryError {
    VersionedRegistryError::Backend(error.to_string())
}

pub(super) fn resource_name(kind: &str, id: &str) -> String {
    format!("{kind}/{id}")
}

pub(super) fn version_name(kind: &str, id: &str, version: u64) -> String {
    format!("{kind}/{id}@{version}")
}
