//! PostgreSQL schema for the published versioned registry store.

use awaken_server_contract::contract::storage::StorageError;

use crate::postgres::PostgresStore;
use crate::postgres_versioned_registry::RegistryTables;

pub(crate) async fn ensure_versioned_registry_schema(
    store: &PostgresStore,
) -> Result<(), StorageError> {
    let tables = RegistryTables::from_store(store);
    let statements = vec![
        format!(
            "CREATE TABLE IF NOT EXISTS {} (
                scope_id TEXT NOT NULL DEFAULT 'default',
                kind TEXT NOT NULL,
                id TEXT NOT NULL,
                current_version BIGINT,
                archived_at_ms BIGINT,
                created_at_ms BIGINT NOT NULL,
                updated_at_ms BIGINT NOT NULL,
                metadata_json JSONB NOT NULL DEFAULT '{{}}',
                PRIMARY KEY (scope_id, kind, id)
            )",
            tables.resources
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS {} (
                scope_id TEXT NOT NULL DEFAULT 'default',
                kind TEXT NOT NULL,
                id TEXT NOT NULL,
                version BIGINT NOT NULL,
                content_hash TEXT NOT NULL,
                value_schema_version INTEGER NOT NULL,
                canonical_value_json TEXT NOT NULL,
                value_json JSONB NOT NULL,
                metadata_json JSONB NOT NULL DEFAULT '{{}}',
                created_at_ms BIGINT NOT NULL,
                PRIMARY KEY (scope_id, kind, id, version)
            )",
            tables.versions
        ),
        format!(
            "CREATE INDEX IF NOT EXISTS idx_{}_hash
             ON {} (scope_id, kind, id, content_hash)",
            tables.versions, tables.versions
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS {} (
                scope_id TEXT NOT NULL DEFAULT 'default',
                snapshot_version BIGINT NOT NULL,
                publication_id TEXT NOT NULL,
                source_config_revisions_json JSONB NOT NULL DEFAULT '[]',
                created_by TEXT,
                metadata_json JSONB NOT NULL DEFAULT '{{}}',
                created_at_ms BIGINT NOT NULL,
                PRIMARY KEY (scope_id, snapshot_version),
                UNIQUE (scope_id, publication_id)
            )",
            tables.publications
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS {} (
                scope_id TEXT NOT NULL DEFAULT 'default',
                snapshot_version BIGINT NOT NULL,
                kind TEXT NOT NULL,
                id TEXT NOT NULL,
                version BIGINT NOT NULL,
                content_hash TEXT NOT NULL,
                PRIMARY KEY (scope_id, snapshot_version, kind, id),
                FOREIGN KEY (scope_id, snapshot_version)
                    REFERENCES {} (scope_id, snapshot_version),
                FOREIGN KEY (scope_id, kind, id, version)
                    REFERENCES {} (scope_id, kind, id, version)
            )",
            tables.publication_entries, tables.publications, tables.versions
        ),
    ];

    for stmt in statements {
        sqlx::query(&stmt)
            .execute(&store.pool)
            .await
            .map_err(|error| StorageError::Io(error.to_string()))?;
    }
    Ok(())
}
