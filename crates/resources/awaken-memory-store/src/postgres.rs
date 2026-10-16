// @formal entrypoint memory.absent_conditional_delete_is_idempotent
// @formal entrypoint memory.cas_has_one_winner_per_generation
// @formal entrypoint memory.idempotent_write_does_not_advance_generation
// @formal entrypoint memory.matched_conditional_delete_removes_exact_head
// @formal entrypoint memory.rename_replace_preserves_source_identity
// @formal entrypoint memory.stale_cas_never_clobbers_live_content
// @formal entrypoint memory.stale_conditional_delete_never_removes_live_head
//! Postgres [`MemoryWriter`] over the crate's `memory_store` migration scope — the
//! multi-node sibling of the SQLite backend over the same portable bundle.

use awaken_sql_storage::StoredU64;
use awaken_time::WallClock;
use sqlx::Row;
use sqlx::postgres::PgPool;
use std::sync::Arc;

use crate::schema::{BUNDLE_ID, converged_memory_store_bundle, selected_memory_store_bundle};

const NS: &str = "memory_store";

/// Errors from connecting or migrating the Postgres store.
#[derive(Debug, thiserror::Error)]
pub enum PgStoreError {
    #[error("connect: {0}")]
    Connect(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

/// Apply the `memory_store` scoped migration bundle to `pool` (idempotent).
/// Shared by both Postgres stores, since they live under one scope.
async fn run_migrations(pool: &PgPool) -> Result<(), PgStoreError> {
    let (published, converged) = selected_bundles(pool).await?;
    let runner =
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|error| PgStoreError::Migrate(error.to_string()))?;
    runner
        .run_bundle(&published)
        .await
        .map_err(|error| PgStoreError::Migrate(error.to_string()))?;
    runner
        .run_bundle(&converged)
        .await
        .map_err(|error| PgStoreError::Migrate(error.to_string()))?;
    Ok(())
}

/// Verify the externally-owned `memory_store` bundle without executing DDL.
async fn verify_migrations(pool: &PgPool) -> Result<(), PgStoreError> {
    let (published, converged) = selected_bundles(pool).await?;
    let runner =
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(|error| PgStoreError::Migrate(error.to_string()))?;
    runner
        .verify_bundle(&published)
        .await
        .map_err(|error| PgStoreError::Migrate(error.to_string()))?;
    runner
        .verify_bundle(&converged)
        .await
        .map_err(|error| PgStoreError::Migrate(error.to_string()))?;
    Ok(())
}

async fn selected_bundles(
    pool: &PgPool,
) -> Result<
    (
        awaken_scoped_migration::MigrationBundle,
        awaken_scoped_migration::MigrationBundle,
    ),
    PgStoreError,
> {
    let ledger: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
        .bind(format!("{NS}_schema_migrations"))
        .fetch_one(pool)
        .await
        .map_err(|error| PgStoreError::Migrate(error.to_string()))?;
    let v1_checksum: Option<String> = if ledger.is_some() {
        sqlx::query_scalar(&format!(
            "SELECT checksum FROM {NS}_schema_migrations WHERE bundle_id = $1 AND version = 1"
        ))
        .bind(BUNDLE_ID)
        .fetch_optional(pool)
        .await
        .map_err(|error| PgStoreError::Migrate(error.to_string()))?
    } else {
        None
    };
    Ok((
        selected_memory_store_bundle(v1_checksum.as_deref())
            .map_err(|error| PgStoreError::Migrate(error.to_string()))?,
        converged_memory_store_bundle()
            .map_err(|error| PgStoreError::Migrate(error.to_string()))?,
    ))
}

use crate::repository::{
    ConditionalDeleteAdmission, HeadUpdateAdmission, rename_replace_identity, stored_now_nanos,
    under_prefix, validate_path, validate_size,
};
use crate::{
    MemErr, Memory, MemoryActor, MemoryContentPurge, MemoryCopyConflictPreservation,
    MemoryCopyConflictQueries, MemoryCopyWriteback, MemoryEntry, MemoryFilesystem,
    MemoryGeneration, MemoryPurgeSummary, MemoryQueries, MemorySnapshotSource, MemoryVersion,
    MemoryVersionOperation, MemoryVersionQueries, MemoryVersionRedaction, MemoryWriter,
    VersionAppend, sha256_hex,
};

fn mem_err(err: impl std::fmt::Display) -> MemErr {
    MemErr::Storage(err.to_string())
}

fn domain_version(value: i64) -> Result<u64, MemErr> {
    StoredU64::try_from(value)
        .map(StoredU64::domain_value)
        .map_err(mem_err)
}

fn next_domain_version(value: i64) -> Result<u64, MemErr> {
    StoredU64::try_from(value)
        .and_then(|value| value.checked_add(1))
        .map(StoredU64::domain_value)
        .map_err(mem_err)
}

/// A Postgres-backed [`MemoryWriter`] (path-addressed, CAS). Compare-and-swap and rename
/// run in a transaction with `SELECT … FOR UPDATE`, so the optimistic-concurrency
/// guarantee holds **across nodes** (the reason ADR-0057 chose Postgres).
pub struct PostgresMemoryRepository {
    pool: PgPool,
    clock: Arc<dyn WallClock>,
}

impl PostgresMemoryRepository {
    /// Connect and apply the memory-store migrations (one-step convenience).
    pub async fn connect(url: &str, clock: Arc<dyn WallClock>) -> Result<Self, PgStoreError> {
        let pool = awaken_sql_storage::postgres_pool_options()
            .connect(url)
            .await
            .map_err(|e| PgStoreError::Connect(e.to_string()))?;
        let store = Self::with_pool(pool, clock);
        store.ensure_schema().await?;
        Ok(store)
    }

    /// Connect to an already-migrated schema without executing DDL.
    pub async fn connect_existing(
        url: &str,
        clock: Arc<dyn WallClock>,
    ) -> Result<Self, PgStoreError> {
        let pool = awaken_sql_storage::postgres_pool_options()
            .connect(url)
            .await
            .map_err(|error| PgStoreError::Connect(error.to_string()))?;
        Self::with_existing_pool(pool, clock).await
    }

    /// Wrap an existing pool after verifying its scoped migration ledger.
    pub async fn with_existing_pool(
        pool: PgPool,
        clock: Arc<dyn WallClock>,
    ) -> Result<Self, PgStoreError> {
        verify_migrations(&pool).await?;
        Ok(Self { pool, clock })
    }

    /// Wrap an existing pool **without migrating**, allowing a unified migration
    /// pipeline to own the shared database.
    pub fn with_pool(pool: PgPool, clock: Arc<dyn WallClock>) -> Self {
        Self { pool, clock }
    }

    /// Apply the `memory_store` scoped migration bundle (idempotent). Optional.
    pub async fn ensure_schema(&self) -> Result<(), PgStoreError> {
        run_migrations(&self.pool).await
    }
}

fn to_memory(
    id: String,
    path: String,
    content: Vec<u8>,
    sha: String,
    version: i64,
    created: i64,
    updated: i64,
) -> Result<Memory, MemErr> {
    let content = String::from_utf8(content).map_err(mem_err)?;
    Ok(Memory {
        content_size: content.len() as u64,
        content: Some(content),
        id,
        path,
        content_sha256: sha,
        version: domain_version(version)?,
        created_unix_nanos: created as u128,
        updated_unix_nanos: updated as u128,
    })
}

fn operation_name(operation: MemoryVersionOperation) -> &'static str {
    match operation {
        MemoryVersionOperation::Created => "created",
        MemoryVersionOperation::Modified => "modified",
        MemoryVersionOperation::Deleted => "deleted",
    }
}

fn parse_operation(value: &str) -> Result<MemoryVersionOperation, MemErr> {
    match value {
        "created" => Ok(MemoryVersionOperation::Created),
        "modified" => Ok(MemoryVersionOperation::Modified),
        "deleted" => Ok(MemoryVersionOperation::Deleted),
        other => Err(mem_err(format!(
            "unknown memory version operation `{other}`"
        ))),
    }
}

fn to_version(row: sqlx::postgres::PgRow) -> Result<MemoryVersion, MemErr> {
    let content = row
        .get::<Option<Vec<u8>>, _>("content")
        .map(String::from_utf8)
        .transpose()
        .map_err(mem_err)?;
    Ok(MemoryVersion {
        id: row.get("id"),
        memory_id: row.get("memory_id"),
        operation: parse_operation(row.get::<String, _>("operation").as_str())?,
        path: row.get("path"),
        content,
        created_unix_nanos: row.get::<i64, _>("created") as u128,
        created_by: row
            .get::<Option<String>, _>("created_by_json")
            .map(|json| serde_json::from_str(&json).map_err(mem_err))
            .transpose()?,
        redacted_unix_nanos: row
            .get::<Option<i64>, _>("redacted")
            .map(|value| value as u128),
        redacted_by: row
            .get::<Option<String>, _>("redacted_by_json")
            .map(|json| serde_json::from_str(&json).map_err(mem_err))
            .transpose()?,
    })
}

async fn next_counter(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    name: &str,
    seed_sql: &str,
) -> Result<i64, MemErr> {
    let seed: i64 = sqlx::query_scalar(seed_sql)
        .fetch_one(&mut **tx)
        .await
        .map_err(mem_err)?;
    sqlx::query(&format!(
        "INSERT INTO {NS}_counters(name, next_value) VALUES ($1, $2) ON CONFLICT (name) DO NOTHING"
    ))
    .bind(name)
    .bind(seed)
    .execute(&mut **tx)
    .await
    .map_err(mem_err)?;
    sqlx::query_scalar(&format!(
        "UPDATE {NS}_counters SET next_value = next_value + 1 WHERE name = $1 RETURNING next_value"
    ))
    .bind(name)
    .fetch_one(&mut **tx)
    .await
    .map_err(mem_err)
}

async fn append_version(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    append: VersionAppend<'_>,
) -> Result<(), MemErr> {
    let ordinal = next_counter(
        tx,
        "memory_version",
        &format!("SELECT COALESCE(MAX(ordinal), 0) FROM {NS}_versions"),
    )
    .await?;
    sqlx::query(&format!(
        "INSERT INTO {NS}_versions \
         (store_id, ordinal, id, memory_id, operation, path, content, created, redacted, created_by_json, redacted_by_json) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NULL, $9, NULL)"
    ))
    .bind(append.store)
    .bind(ordinal)
    .bind(format!("memver_{ordinal:016}"))
    .bind(append.memory_id)
    .bind(operation_name(append.operation))
    .bind(append.path)
    .bind(append.content.map(str::as_bytes))
    .bind(append.created)
    .bind(
        append
            .actor
            .map(serde_json::to_string)
            .transpose()
            .map_err(mem_err)?,
    )
    .execute(&mut **tx)
    .await
    .map_err(mem_err)?;
    Ok(())
}

impl PostgresMemoryRepository {
    async fn preserve_copy_conflict_inner(
        &self,
        store: &str,
        candidate: &awaken_resource_contract::MemoryCopyConflict,
    ) -> Result<(), MemErr> {
        crate::repository::validate_copy_conflict(candidate)?;
        let id = candidate.id();
        let encoded = serde_json::to_string(candidate).map_err(mem_err)?;
        let mut tx = self.pool.begin().await.map_err(mem_err)?;
        // Same store-scoped admission authority as create; no separate quota ledger.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
            .bind(store)
            .execute(&mut *tx)
            .await
            .map_err(mem_err)?;
        let existing: Option<String> = sqlx::query_scalar(&format!(
            "SELECT candidate_json FROM {NS}_copy_conflicts WHERE store_id = $1 AND id = $2"
        ))
        .bind(store)
        .bind(&id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(mem_err)?;
        if let Some(existing) = existing {
            return if existing == encoded {
                Ok(())
            } else {
                Err(mem_err("Memory copy proposal identity mismatch"))
            };
        }
        let count: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {NS}_copy_conflicts WHERE store_id = $1"
        ))
        .bind(store)
        .fetch_one(&mut *tx)
        .await
        .map_err(mem_err)?;
        if count >= crate::MAX_MEMORIES_PER_STORE as i64 {
            return Err(MemErr::AtCapacity);
        }
        sqlx::query(&format!(
            "INSERT INTO {NS}_copy_conflicts (store_id, id, candidate_json) VALUES ($1, $2, $3)"
        ))
        .bind(store)
        .bind(id)
        .bind(encoded)
        .execute(&mut *tx)
        .await
        .map_err(mem_err)?;
        tx.commit().await.map_err(mem_err)
    }

    async fn list_copy_conflicts_inner(
        &self,
        store: &str,
    ) -> Result<Vec<awaken_resource_contract::MemoryCopyConflict>, MemErr> {
        let rows = sqlx::query(&format!(
            "SELECT id, candidate_json FROM {NS}_copy_conflicts WHERE store_id = $1 ORDER BY id"
        ))
        .bind(store)
        .fetch_all(&self.pool)
        .await
        .map_err(mem_err)?;
        rows.into_iter()
            .map(|row| {
                crate::repository::decode_copy_conflict(
                    &row.try_get::<String, _>("id").map_err(mem_err)?,
                    &row.try_get::<String, _>("candidate_json")
                        .map_err(mem_err)?,
                )
            })
            .collect()
    }
    async fn snapshot_heads_inner(&self, store: &str) -> Result<Vec<Memory>, MemErr> {
        let rows = sqlx::query(&format!(
            "SELECT id, path, content, sha, version, created, updated \
             FROM {NS}_memories WHERE store_id = $1 ORDER BY path"
        ))
        .bind(store)
        .fetch_all(&self.pool)
        .await
        .map_err(mem_err)?;
        rows.into_iter()
            .map(|row| {
                to_memory(
                    row.get("id"),
                    row.get("path"),
                    row.get("content"),
                    row.get("sha"),
                    row.get("version"),
                    row.get("created"),
                    row.get("updated"),
                )
            })
            .collect()
    }

    async fn list_inner(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr> {
        let rows = sqlx::query(&format!(
            "SELECT id, path, sha, length(content) AS n, version, updated \
             FROM {NS}_memories WHERE store_id = $1"
        ))
        .bind(store)
        .fetch_all(&self.pool)
        .await
        .map_err(mem_err)?;
        rows.into_iter()
            .map(|r| {
                (
                    r.get::<String, _>("id"),
                    r.get::<String, _>("path"),
                    r.get::<String, _>("sha"),
                    r.get::<i32, _>("n"),
                    r.get::<i64, _>("version"),
                    r.get::<i64, _>("updated"),
                )
            })
            .filter(|(_, path, ..)| under_prefix(path, prefix))
            .map(|(id, path, sha, size, version, updated)| {
                Ok(MemoryEntry {
                    id,
                    path,
                    content_sha256: sha,
                    content_size: u64::try_from(size).map_err(mem_err)?,
                    version: domain_version(version)?,
                    updated_unix_nanos: updated as u128,
                })
            })
            .collect()
    }

    async fn get_by_path_inner(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr> {
        let row = sqlx::query(&format!(
            "SELECT id, path, content, sha, version, created, updated \
             FROM {NS}_memories WHERE store_id = $1 AND path = $2"
        ))
        .bind(store)
        .bind(path)
        .fetch_optional(&self.pool)
        .await
        .map_err(mem_err)?;
        match row {
            Some(r) => Ok(Some(to_memory(
                r.get("id"),
                r.get("path"),
                r.get("content"),
                r.get("sha"),
                r.get("version"),
                r.get("created"),
                r.get("updated"),
            )?)),
            None => Ok(None),
        }
    }

    async fn create_inner(&self, store: &str, path: &str, content: &str) -> Result<Memory, MemErr> {
        self.create_as_inner(store, path, content, None).await
    }

    async fn create_as_inner(
        &self,
        store: &str,
        path: &str,
        content: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        validate_path(path)?;
        validate_size(content)?;
        let mut tx = self.pool.begin().await.map_err(mem_err)?;
        // One store-scoped transaction lock makes the 2,000-head admission
        // atomic across nodes without inventing a separate capacity registry.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
            .bind(store)
            .execute(&mut *tx)
            .await
            .map_err(mem_err)?;
        let exists = sqlx::query(&format!(
            "SELECT 1 FROM {NS}_memories WHERE store_id = $1 AND path = $2"
        ))
        .bind(store)
        .bind(path)
        .fetch_optional(&mut *tx)
        .await
        .map_err(mem_err)?
        .is_some();
        if exists {
            return Err(MemErr::PathConflict(path.to_string()));
        }
        let live_count: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {NS}_memories WHERE store_id = $1"
        ))
        .bind(store)
        .fetch_one(&mut *tx)
        .await
        .map_err(mem_err)?;
        if live_count >= crate::MAX_MEMORIES_PER_STORE as i64 {
            return Err(MemErr::AtCapacity);
        }
        let ordinal = next_counter(
            &mut tx,
            "memory_id",
            &format!("SELECT COALESCE(MAX(ordinal), 0) FROM {NS}_memories"),
        )
        .await?;
        let id = format!("mem_{ordinal}");
        let sha = sha256_hex(content);
        let now = stored_now_nanos(self.clock.as_ref())?;
        sqlx::query(&format!(
            "INSERT INTO {NS}_memories \
             (store_id, path, id, ordinal, content, sha, version, created, updated) \
             VALUES ($1, $2, $3, $4, $5, $6, 1, $7, $7)"
        ))
        .bind(store)
        .bind(path)
        .bind(&id)
        .bind(ordinal)
        .bind(content.as_bytes())
        .bind(&sha)
        .bind(now)
        .execute(&mut *tx)
        .await
        // The existence pre-check is unlocked (`SELECT 1`, no `FOR UPDATE`), so two
        // concurrent same-path creates can both pass it and race the INSERT. The
        // `(store_id, path)` primary key serializes them; the loser must surface the
        // DOMAIN conflict (`PathConflict`), not a leaked raw storage error, matching
        // the in-process backends.
        .map_err(|e| {
            if e.as_database_error()
                .is_some_and(sqlx::error::DatabaseError::is_unique_violation)
            {
                MemErr::PathConflict(path.to_string())
            } else {
                mem_err(e)
            }
        })?;
        append_version(
            &mut tx,
            VersionAppend {
                store,
                memory_id: &id,
                operation: MemoryVersionOperation::Created,
                path,
                content: Some(content),
                created: now,
                actor,
            },
        )
        .await?;
        tx.commit().await.map_err(mem_err)?;
        Ok(Memory {
            content_size: content.len() as u64,
            content: Some(content.to_string()),
            id,
            path: path.to_string(),
            content_sha256: sha,
            version: 1,
            created_unix_nanos: now as u128,
            updated_unix_nanos: now as u128,
        })
    }

    async fn update_head_inner(
        &self,
        store: &str,
        base: &MemoryGeneration,
        content: &str,
        target_path: Option<&str>,
    ) -> Result<Memory, MemErr> {
        self.update_head_as_inner(store, base, content, target_path, None)
            .await
    }

    async fn update_head_as_inner(
        &self,
        store: &str,
        base: &MemoryGeneration,
        content: &str,
        target_path: Option<&str>,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        self.update_head_checked(store, base, content, target_path, actor, None)
            .await
    }

    async fn update_materialized_inner(
        &self,
        store: &str,
        path: &str,
        base: &MemoryGeneration,
        content: &str,
    ) -> Result<Memory, MemErr> {
        self.update_head_checked(store, base, content, None, None, Some(path))
            .await
    }

    async fn rename_inner(&self, store: &str, from: &str, to: &str) -> Result<Memory, MemErr> {
        validate_path(to)?;
        let mut tx = self.pool.begin().await.map_err(mem_err)?;
        let row = sqlx::query(&format!(
            "SELECT id, content, sha, version, created FROM {NS}_memories \
             WHERE store_id = $1 AND path = $2 FOR UPDATE"
        ))
        .bind(store)
        .bind(from)
        .fetch_optional(&mut *tx)
        .await
        .map_err(mem_err)?
        .ok_or_else(|| MemErr::NotFound(from.to_string()))?;
        let (id, content, sha, version, created): (String, Vec<u8>, String, i64, i64) = (
            row.get("id"),
            row.get("content"),
            row.get("sha"),
            row.get("version"),
            row.get("created"),
        );
        if from == to {
            return to_memory(id, from.into(), content, sha, version, created, created);
        }
        let now = stored_now_nanos(self.clock.as_ref())?;
        let displaced_id = sqlx::query_scalar::<_, String>(&format!(
            "SELECT id FROM {NS}_memories WHERE store_id = $1 AND path = $2 FOR UPDATE"
        ))
        .bind(store)
        .bind(to)
        .fetch_optional(&mut *tx)
        .await
        .map_err(mem_err)?;
        let id = rename_replace_identity(id, displaced_id.as_ref());
        sqlx::query(&format!(
            "DELETE FROM {NS}_memories WHERE store_id = $1 AND path = $2"
        ))
        .bind(store)
        .bind(to)
        .execute(&mut *tx)
        .await
        .map_err(mem_err)?;
        sqlx::query(&format!(
            "UPDATE {NS}_memories SET path = $1, version = version + 1, updated = $2 \
             WHERE store_id = $3 AND id = $4"
        ))
        .bind(to)
        .bind(now)
        .bind(store)
        .bind(&id)
        .execute(&mut *tx)
        .await
        .map_err(mem_err)?;
        if let Some(displaced_id) = displaced_id {
            append_version(
                &mut tx,
                VersionAppend {
                    store,
                    memory_id: &displaced_id,
                    operation: MemoryVersionOperation::Deleted,
                    path: to,
                    content: None,
                    created: now,
                    actor: None,
                },
            )
            .await?;
        }
        let content = String::from_utf8(content).map_err(mem_err)?;
        append_version(
            &mut tx,
            VersionAppend {
                store,
                memory_id: &id,
                operation: MemoryVersionOperation::Modified,
                path: to,
                content: Some(&content),
                created: now,
                actor: None,
            },
        )
        .await?;
        tx.commit().await.map_err(mem_err)?;
        Ok(Memory {
            content_size: content.len() as u64,
            content: Some(content),
            id,
            path: to.to_string(),
            content_sha256: sha,
            version: next_domain_version(version)?,
            created_unix_nanos: created as u128,
            updated_unix_nanos: now as u128,
        })
    }

    async fn delete_by_path_inner(&self, store: &str, path: &str) -> Result<(), MemErr> {
        self.delete_by_path_as_inner(store, path, None).await
    }

    async fn delete_by_path_as_inner(
        &self,
        store: &str,
        path: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<(), MemErr> {
        let mut tx = self.pool.begin().await.map_err(mem_err)?;
        let memory_id = sqlx::query_scalar::<_, String>(&format!(
            "SELECT id FROM {NS}_memories WHERE store_id = $1 AND path = $2 FOR UPDATE"
        ))
        .bind(store)
        .bind(path)
        .fetch_optional(&mut *tx)
        .await
        .map_err(mem_err)?;
        let Some(memory_id) = memory_id else {
            return Ok(());
        };
        sqlx::query(&format!(
            "DELETE FROM {NS}_memories WHERE store_id = $1 AND path = $2"
        ))
        .bind(store)
        .bind(path)
        .execute(&mut *tx)
        .await
        .map_err(mem_err)?;
        append_version(
            &mut tx,
            VersionAppend {
                store,
                memory_id: &memory_id,
                operation: MemoryVersionOperation::Deleted,
                path,
                content: None,
                created: stored_now_nanos(self.clock.as_ref())?,
                actor,
            },
        )
        .await?;
        tx.commit().await.map_err(mem_err)?;
        Ok(())
    }

    async fn delete_if_match_inner(
        &self,
        store: &str,
        path: &str,
        base: &MemoryGeneration,
    ) -> Result<bool, MemErr> {
        self.delete_if_match_as_inner(store, path, base, None).await
    }

    async fn delete_if_match_as_inner(
        &self,
        store: &str,
        path: &str,
        base: &MemoryGeneration,
        actor: Option<&MemoryActor>,
    ) -> Result<bool, MemErr> {
        let mut tx = self.pool.begin().await.map_err(mem_err)?;
        let row = sqlx::query(&format!(
            "SELECT id, content, sha, version, created, updated FROM {NS}_memories \
             WHERE store_id = $1 AND path = $2 FOR UPDATE"
        ))
        .bind(store)
        .bind(path)
        .fetch_optional(&mut *tx)
        .await
        .map_err(mem_err)?;
        let admission = ConditionalDeleteAdmission::new(row.as_ref().map(|row| {
            let id: String = row.get("id");
            let sha: String = row.get("sha");
            (
                id == base.memory_id(),
                domain_version(row.get("version")).is_ok_and(|version| version == base.version()),
                sha == base.content_sha256(),
            )
        }));
        match admission {
            ConditionalDeleteAdmission::Absent => return Ok(false),
            ConditionalDeleteAdmission::Conflict => {
                let row = row.expect("conflict requires the observed head");
                return Err(MemErr::Conflict {
                    current: Box::new(to_memory(
                        row.get("id"),
                        path.to_string(),
                        row.get("content"),
                        row.get("sha"),
                        row.get("version"),
                        row.get("created"),
                        row.get("updated"),
                    )?),
                });
            }
            ConditionalDeleteAdmission::Delete => {}
        }
        sqlx::query(&format!(
            "DELETE FROM {NS}_memories WHERE store_id = $1 AND path = $2"
        ))
        .bind(store)
        .bind(path)
        .execute(&mut *tx)
        .await
        .map_err(mem_err)?;
        append_version(
            &mut tx,
            VersionAppend {
                store,
                memory_id: base.memory_id(),
                operation: MemoryVersionOperation::Deleted,
                path,
                content: None,
                created: stored_now_nanos(self.clock.as_ref())?,
                actor,
            },
        )
        .await?;
        tx.commit().await.map_err(mem_err)?;
        Ok(true)
    }

    async fn list_versions_inner(&self, store: &str) -> Result<Vec<MemoryVersion>, MemErr> {
        sqlx::query(&format!(
            "SELECT id, memory_id, operation, path, content, created, redacted, created_by_json, redacted_by_json \
             FROM {NS}_versions WHERE store_id = $1 ORDER BY ordinal"
        ))
        .bind(store)
        .fetch_all(&self.pool)
        .await
        .map_err(mem_err)?
        .into_iter()
        .map(to_version)
        .collect()
    }

    async fn redact_version_inner(
        &self,
        store: &str,
        version_id: &str,
    ) -> Result<Option<MemoryVersion>, MemErr> {
        self.redact_version_as_inner(store, version_id, None).await
    }

    async fn redact_version_as_inner(
        &self,
        store: &str,
        version_id: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Option<MemoryVersion>, MemErr> {
        let mut tx = self.pool.begin().await.map_err(mem_err)?;
        let row = sqlx::query(&format!(
            "SELECT id, memory_id, operation, path, content, created, redacted, created_by_json, redacted_by_json \
             FROM {NS}_versions WHERE store_id = $1 AND id = $2 FOR UPDATE"
        ))
        .bind(store)
        .bind(version_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(mem_err)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let mut version = to_version(row)?;
        if version.redacted_unix_nanos.is_none() {
            let redacted = stored_now_nanos(self.clock.as_ref())?;
            sqlx::query(&format!(
                "UPDATE {NS}_versions SET content = NULL, redacted = $1, redacted_by_json = $2 \
                 WHERE store_id = $3 AND id = $4"
            ))
            .bind(redacted)
            .bind(
                actor
                    .map(serde_json::to_string)
                    .transpose()
                    .map_err(mem_err)?,
            )
            .bind(store)
            .bind(version_id)
            .execute(&mut *tx)
            .await
            .map_err(mem_err)?;
            version.content = None;
            version.redacted_unix_nanos = Some(redacted as u128);
            version.redacted_by = actor.cloned();
        }
        tx.commit().await.map_err(mem_err)?;
        Ok(Some(version))
    }

    async fn purge_store_inner(&self, store: &str) -> Result<MemoryPurgeSummary, MemErr> {
        let mut tx = self.pool.begin().await.map_err(mem_err)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
            .bind(store)
            .execute(&mut *tx)
            .await
            .map_err(mem_err)?;
        let copy_conflicts_deleted = sqlx::query(&format!(
            "DELETE FROM {NS}_copy_conflicts WHERE store_id = $1"
        ))
        .bind(store)
        .execute(&mut *tx)
        .await
        .map_err(mem_err)?
        .rows_affected();
        let versions_deleted =
            sqlx::query(&format!("DELETE FROM {NS}_versions WHERE store_id = $1"))
                .bind(store)
                .execute(&mut *tx)
                .await
                .map_err(mem_err)?
                .rows_affected();
        let heads_deleted = sqlx::query(&format!("DELETE FROM {NS}_memories WHERE store_id = $1"))
            .bind(store)
            .execute(&mut *tx)
            .await
            .map_err(mem_err)?
            .rows_affected();
        tx.commit().await.map_err(mem_err)?;
        Ok(MemoryPurgeSummary {
            heads_deleted,
            versions_deleted,
            copy_conflicts_deleted,
        })
    }
}

impl PostgresMemoryRepository {
    // One mutation/history implementation for ID-following and path-pinned updates.
    #[allow(clippy::too_many_arguments)]
    async fn update_head_checked(
        &self,
        store: &str,
        base: &MemoryGeneration,
        content: &str,
        target_path: Option<&str>,
        actor: Option<&MemoryActor>,
        expected_path: Option<&str>,
    ) -> Result<Memory, MemErr> {
        validate_size(content)?;
        if let Some(path) = expected_path {
            validate_path(path)?;
        }
        if let Some(path) = target_path {
            validate_path(path)?;
        }
        let mut tx = self.pool.begin().await.map_err(mem_err)?;
        let row = sqlx::query(&format!(
            "SELECT path, sha, content, version, created, updated FROM {NS}_memories \
             WHERE store_id = $1 AND id = $2 FOR UPDATE"
        ))
        .bind(store)
        .bind(base.memory_id())
        .fetch_optional(&mut *tx)
        .await
        .map_err(mem_err)?
        .ok_or_else(|| MemErr::NotFound(base.memory_id().to_owned()))?;
        let (path, cur_sha, cur_content, version, created, updated): (
            String,
            String,
            Vec<u8>,
            i64,
            i64,
            i64,
        ) = (
            row.get("path"),
            row.get("sha"),
            row.get("content"),
            row.get("version"),
            row.get("created"),
            row.get("updated"),
        );
        let requested_path = target_path.unwrap_or(&path);
        let new_sha = sha256_hex(content);
        let already_current = cur_sha == new_sha && path == requested_path;
        match HeadUpdateAdmission::new(
            expected_path.is_none_or(|expected| expected == path),
            true,
            domain_version(version)? == base.version(),
            cur_sha == base.content_sha256(),
            already_current,
        ) {
            HeadUpdateAdmission::Conflict => {
                let current = to_memory(
                    base.memory_id().to_owned(),
                    path,
                    cur_content,
                    cur_sha,
                    version,
                    created,
                    updated,
                )?;
                return Err(MemErr::Conflict {
                    current: Box::new(current),
                });
            }
            HeadUpdateAdmission::Current => {
                return to_memory(
                    base.memory_id().to_owned(),
                    path,
                    cur_content,
                    cur_sha,
                    version,
                    created,
                    updated,
                );
            }
            HeadUpdateAdmission::Apply => {}
        }
        let now = stored_now_nanos(self.clock.as_ref())?;
        let displaced_id = if requested_path == path {
            None
        } else {
            sqlx::query_scalar::<_, String>(&format!(
                "SELECT id FROM {NS}_memories WHERE store_id = $1 AND path = $2 FOR UPDATE"
            ))
            .bind(store)
            .bind(requested_path)
            .fetch_optional(&mut *tx)
            .await
            .map_err(mem_err)?
        };
        if displaced_id.is_some() {
            sqlx::query(&format!(
                "DELETE FROM {NS}_memories WHERE store_id = $1 AND path = $2"
            ))
            .bind(store)
            .bind(requested_path)
            .execute(&mut *tx)
            .await
            .map_err(mem_err)?;
        }
        sqlx::query(&format!(
            "UPDATE {NS}_memories SET path = $1, content = $2, sha = $3, \
             version = version + 1, updated = $4 WHERE store_id = $5 AND id = $6"
        ))
        .bind(requested_path)
        .bind(content.as_bytes())
        .bind(&new_sha)
        .bind(now)
        .bind(store)
        .bind(base.memory_id())
        .execute(&mut *tx)
        .await
        .map_err(mem_err)?;
        if let Some(displaced_id) = displaced_id {
            append_version(
                &mut tx,
                VersionAppend {
                    store,
                    memory_id: &displaced_id,
                    operation: MemoryVersionOperation::Deleted,
                    path: requested_path,
                    content: None,
                    created: now,
                    actor,
                },
            )
            .await?;
        }
        append_version(
            &mut tx,
            VersionAppend {
                store,
                memory_id: base.memory_id(),
                operation: MemoryVersionOperation::Modified,
                path: requested_path,
                content: Some(content),
                created: now,
                actor,
            },
        )
        .await?;
        tx.commit().await.map_err(mem_err)?;
        Ok(Memory {
            content_size: content.len() as u64,
            content: Some(content.to_string()),
            id: base.memory_id().to_owned(),
            path: requested_path.to_string(),
            content_sha256: new_sha,
            version: next_domain_version(version)?,
            created_unix_nanos: created as u128,
            updated_unix_nanos: now as u128,
        })
    }
}

#[async_trait::async_trait]
impl MemoryQueries for PostgresMemoryRepository {
    async fn list(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr> {
        PostgresMemoryRepository::list_inner(self, store, prefix).await
    }
    async fn get_by_path(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr> {
        PostgresMemoryRepository::get_by_path_inner(self, store, path).await
    }
}

#[async_trait::async_trait]
impl MemorySnapshotSource for PostgresMemoryRepository {
    async fn snapshot_heads(&self, store: &str) -> Result<Vec<Memory>, MemErr> {
        PostgresMemoryRepository::snapshot_heads_inner(self, store).await
    }
}

#[async_trait::async_trait]
impl MemoryWriter for PostgresMemoryRepository {
    async fn create_as(
        &self,
        store: &str,
        path: &str,
        content: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        PostgresMemoryRepository::create_as_inner(self, store, path, content, actor).await
    }
    async fn update_head_as(
        &self,
        store: &str,
        base: &MemoryGeneration,
        content: &str,
        target_path: Option<&str>,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        PostgresMemoryRepository::update_head_as_inner(
            self,
            store,
            base,
            content,
            target_path,
            actor,
        )
        .await
    }
    async fn delete_by_path_as(
        &self,
        store: &str,
        path: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<(), MemErr> {
        PostgresMemoryRepository::delete_by_path_as_inner(self, store, path, actor).await
    }
    async fn delete_if_match_as(
        &self,
        store: &str,
        path: &str,
        base: &MemoryGeneration,
        actor: Option<&MemoryActor>,
    ) -> Result<bool, MemErr> {
        PostgresMemoryRepository::delete_if_match_as_inner(self, store, path, base, actor).await
    }
    async fn create(&self, store: &str, path: &str, content: &str) -> Result<Memory, MemErr> {
        PostgresMemoryRepository::create_inner(self, store, path, content).await
    }
    async fn update_head(
        &self,
        store: &str,
        base: &MemoryGeneration,
        content: &str,
        target_path: Option<&str>,
    ) -> Result<Memory, MemErr> {
        PostgresMemoryRepository::update_head_inner(self, store, base, content, target_path).await
    }
    async fn delete_by_path(&self, store: &str, path: &str) -> Result<(), MemErr> {
        PostgresMemoryRepository::delete_by_path_inner(self, store, path).await
    }
    async fn delete_if_match(
        &self,
        store: &str,
        path: &str,
        base: &MemoryGeneration,
    ) -> Result<bool, MemErr> {
        PostgresMemoryRepository::delete_if_match_inner(self, store, path, base).await
    }
}

#[async_trait::async_trait]
impl MemoryFilesystem for PostgresMemoryRepository {
    async fn list(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr> {
        PostgresMemoryRepository::list_inner(self, store, prefix).await
    }
    async fn get_by_path(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr> {
        PostgresMemoryRepository::get_by_path_inner(self, store, path).await
    }
    async fn create_as(
        &self,
        store: &str,
        path: &str,
        content: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        PostgresMemoryRepository::create_as_inner(self, store, path, content, actor).await
    }
    async fn update_head_as(
        &self,
        store: &str,
        base: &MemoryGeneration,
        content: &str,
        target_path: Option<&str>,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        PostgresMemoryRepository::update_head_as_inner(
            self,
            store,
            base,
            content,
            target_path,
            actor,
        )
        .await
    }
    async fn rename(&self, store: &str, from: &str, to: &str) -> Result<Memory, MemErr> {
        PostgresMemoryRepository::rename_inner(self, store, from, to).await
    }
    async fn delete_by_path_as(
        &self,
        store: &str,
        path: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<(), MemErr> {
        PostgresMemoryRepository::delete_by_path_as_inner(self, store, path, actor).await
    }
}

#[async_trait::async_trait]
impl MemoryCopyWriteback for PostgresMemoryRepository {
    async fn create_as(
        &self,
        store: &str,
        path: &str,
        content: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        PostgresMemoryRepository::create_as_inner(self, store, path, content, actor).await
    }
    async fn update_materialized(
        &self,
        store: &str,
        path: &str,
        base: &MemoryGeneration,
        content: &str,
    ) -> Result<Memory, MemErr> {
        PostgresMemoryRepository::update_materialized_inner(self, store, path, base, content).await
    }
    async fn delete_if_match_as(
        &self,
        store: &str,
        path: &str,
        base: &MemoryGeneration,
        actor: Option<&MemoryActor>,
    ) -> Result<bool, MemErr> {
        PostgresMemoryRepository::delete_if_match_as_inner(self, store, path, base, actor).await
    }
}

#[async_trait::async_trait]
impl MemoryCopyConflictPreservation for PostgresMemoryRepository {
    async fn preserve_copy_conflict(
        &self,
        store: &str,
        candidate: &awaken_resource_contract::MemoryCopyConflict,
    ) -> Result<(), MemErr> {
        PostgresMemoryRepository::preserve_copy_conflict_inner(self, store, candidate).await
    }
}

#[async_trait::async_trait]
impl MemoryCopyConflictQueries for PostgresMemoryRepository {
    async fn list_copy_conflicts(
        &self,
        store: &str,
    ) -> Result<Vec<awaken_resource_contract::MemoryCopyConflict>, MemErr> {
        PostgresMemoryRepository::list_copy_conflicts_inner(self, store).await
    }
}

#[async_trait::async_trait]
impl MemoryVersionQueries for PostgresMemoryRepository {
    async fn list_versions(&self, store: &str) -> Result<Vec<MemoryVersion>, MemErr> {
        PostgresMemoryRepository::list_versions_inner(self, store).await
    }
}

#[async_trait::async_trait]
impl MemoryVersionRedaction for PostgresMemoryRepository {
    async fn redact_version_as(
        &self,
        store: &str,
        version_id: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Option<MemoryVersion>, MemErr> {
        PostgresMemoryRepository::redact_version_as_inner(self, store, version_id, actor).await
    }
    async fn redact_version(
        &self,
        store: &str,
        version_id: &str,
    ) -> Result<Option<MemoryVersion>, MemErr> {
        PostgresMemoryRepository::redact_version_inner(self, store, version_id).await
    }
}

#[async_trait::async_trait]
impl MemoryContentPurge for PostgresMemoryRepository {
    async fn purge_store(&self, store: &str) -> Result<MemoryPurgeSummary, MemErr> {
        PostgresMemoryRepository::purge_store_inner(self, store).await
    }
}
