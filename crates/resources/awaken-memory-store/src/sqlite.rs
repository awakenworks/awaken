// @formal entrypoint memory.absent_conditional_delete_is_idempotent
// @formal entrypoint memory.cas_has_one_winner_per_generation
// @formal entrypoint memory.idempotent_write_does_not_advance_generation
// @formal entrypoint memory.matched_conditional_delete_removes_exact_head
// @formal entrypoint memory.rename_replace_preserves_source_identity
// @formal entrypoint memory.stale_cas_never_clobbers_live_content
// @formal entrypoint memory.stale_conditional_delete_never_removes_live_head
//! SQLite [`MemoryRepository`] over the crate's `memory_store` migration scope.

use crate::stored_u64::StoredU64;
use awaken_scoped_migration::sqlite::SqliteMigrationRunner;
use awaken_sqlite_pool::EmbeddedPool;
use awaken_sqlite_pool::EmbeddedSqlite;
use awaken_time::WallClock;
use sqlx::SqliteConnection;
use std::future::Future;
use std::sync::Arc;

use crate::schema::{BUNDLE_ID, converged_memory_store_bundle, selected_memory_store_bundle};

const NS: &str = "memory_store";

/// Errors from opening or migrating the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("open: {0}")]
    Open(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

/// Apply the `memory_store` scoped migration bundle to `pool` (idempotent).
/// Shared by both SQLite stores, since they live under one scope.
async fn migrate_pool(pool: &EmbeddedPool) -> Result<(), StoreError> {
    let ledger_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
    )
    .bind(format!("{NS}_schema_migrations"))
    .fetch_one(pool)
    .await
    .map_err(|error| StoreError::Migrate(error.to_string()))?;
    let v1_checksum = if ledger_exists {
        sqlx::query_scalar::<_, String>(&format!(
            "SELECT checksum FROM {NS}_schema_migrations WHERE bundle_id = ?1 AND version = 1"
        ))
        .bind(BUNDLE_ID)
        .fetch_optional(pool)
        .await
        .map_err(|error| StoreError::Migrate(error.to_string()))?
    } else {
        None
    };
    let published = selected_memory_store_bundle(v1_checksum.as_deref())
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
    let converged =
        converged_memory_store_bundle().map_err(|error| StoreError::Migrate(error.to_string()))?;
    let runner = SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), NS)
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
    awaken_sqlite_pool::held(runner.run_bundle(&published))
        .await
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
    awaken_sqlite_pool::held(runner.run_bundle(&converged))
        .await
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
    Ok(())
}

use crate::repository::{
    ConditionalDeleteAdmission, HeadUpdateAdmission, rename_replace_identity, stored_now_nanos,
    under_prefix, validate_path, validate_size,
};
use crate::{
    MemErr, Memory, MemoryActor, MemoryEntry, MemoryGeneration, MemoryPurgeSummary,
    MemoryRepository, MemoryVersion, MemoryVersionOperation, VersionAppend, sha256_hex,
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

/// Run one mutation on the store's connection to completion.
///
/// The connection is admitted in the caller (first come, first served), then
/// the transaction runs on its own task: a caller that stops waiting after
/// admission does not roll back a write already in flight, exactly as the
/// former blocking-thread bridge kept running its admitted operation.
async fn with_conn_mem<T, F, Fut>(pool: &EmbeddedPool, operation: F) -> Result<T, MemErr>
where
    T: Send + 'static,
    F: FnOnce(awaken_sqlite_pool::EmbeddedConnection) -> Fut,
    Fut: Future<Output = Result<T, MemErr>> + Send + 'static,
{
    let connection = pool.acquire().await.map_err(mem_err)?;
    tokio::spawn(operation(connection)).await.map_err(mem_err)?
}

/// A SQLite-backed [`MemoryRepository`] (path-addressed, CAS). Each mutation holds the
/// store's one connection, so a read-modify-write (compare-and-swap, rename-replace) is
/// atomic within the process; a cross-node CAS is the postgres backend's job.
pub struct SqliteMemoryRepository {
    pool: EmbeddedPool,
    clock: Arc<dyn WallClock>,
}

impl SqliteMemoryRepository {
    /// Open (or create) a database file and apply the memory-store migrations
    /// (one-step convenience for a store-owned database).
    pub async fn open(path: &str, clock: Arc<dyn WallClock>) -> Result<Self, StoreError> {
        let pool = EmbeddedSqlite::file(path)
            .open()
            .await
            .map_err(|e| StoreError::Open(e.to_string()))?;
        let store = Self::over(pool, clock);
        store.ensure_schema().await?;
        Ok(store)
    }

    /// Open a private in-memory database for tests and scenario fixtures.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn open_in_memory(clock: Arc<dyn WallClock>) -> Result<Self, StoreError> {
        let pool = EmbeddedSqlite::memory()
            .open()
            .await
            .map_err(|e| StoreError::Open(e.to_string()))?;
        let store = Self::over(pool, clock);
        store.ensure_schema().await?;
        Ok(store)
    }

    /// Wrap an existing pool **without migrating**, allowing a unified
    /// migration pipeline to own the shared database.
    pub fn over(pool: EmbeddedPool, clock: Arc<dyn WallClock>) -> Self {
        Self { pool, clock }
    }

    /// Apply the `memory_store` scoped migration bundle (idempotent). Optional.
    pub async fn ensure_schema(&self) -> Result<(), StoreError> {
        migrate_pool(&self.pool).await
    }
}

/// Build a [`Memory`] from a memories row (content included).
fn row_memory(
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

/// One `{NS}_versions` row: id, memory_id, operation, path, content, created,
/// redacted, created_by_json, redacted_by_json.
type VersionRow = (
    String,
    String,
    String,
    String,
    Option<Vec<u8>>,
    i64,
    Option<i64>,
    Option<String>,
    Option<String>,
);

fn row_version(row: VersionRow) -> Result<MemoryVersion, MemErr> {
    let (
        id,
        memory_id,
        operation,
        path,
        content,
        created,
        redacted,
        created_by_json,
        redacted_by_json,
    ) = row;
    let content = content
        .map(String::from_utf8)
        .transpose()
        .map_err(mem_err)?;
    let actor = |json: Option<String>| -> Result<Option<MemoryActor>, MemErr> {
        json.map(|json| serde_json::from_str(&json).map_err(mem_err))
            .transpose()
    };
    Ok(MemoryVersion {
        id,
        memory_id,
        operation: parse_operation(&operation)?,
        path,
        content,
        created_unix_nanos: created as u128,
        created_by: actor(created_by_json)?,
        redacted_unix_nanos: redacted.map(|value| value as u128),
        redacted_by: actor(redacted_by_json)?,
    })
}

async fn next_counter(
    tx: &mut SqliteConnection,
    name: &str,
    seed_sql: &str,
) -> Result<i64, MemErr> {
    let seed: i64 = sqlx::query_scalar(seed_sql)
        .fetch_one(&mut *tx)
        .await
        .map_err(mem_err)?;
    sqlx::query(&format!(
        "INSERT OR IGNORE INTO {NS}_counters(name, next_value) VALUES (?1, ?2)"
    ))
    .bind(name)
    .bind(seed)
    .execute(&mut *tx)
    .await
    .map_err(mem_err)?;
    sqlx::query(&format!(
        "UPDATE {NS}_counters SET next_value = next_value + 1 WHERE name = ?1"
    ))
    .bind(name)
    .execute(&mut *tx)
    .await
    .map_err(mem_err)?;
    sqlx::query_scalar(&format!(
        "SELECT next_value FROM {NS}_counters WHERE name = ?1"
    ))
    .bind(name)
    .fetch_one(&mut *tx)
    .await
    .map_err(mem_err)
}

async fn append_version(
    tx: &mut SqliteConnection,
    append: VersionAppend<'_>,
) -> Result<MemoryVersion, MemErr> {
    let ordinal = next_counter(
        tx,
        "memory_version",
        &format!("SELECT COALESCE(MAX(ordinal), 0) FROM {NS}_versions"),
    )
    .await?;
    let version = MemoryVersion {
        id: format!("memver_{ordinal:016}"),
        memory_id: append.memory_id.to_string(),
        operation: append.operation,
        path: append.path.to_string(),
        content: append.content.map(str::to_string),
        created_unix_nanos: append.created as u128,
        created_by: append.actor.cloned(),
        redacted_unix_nanos: None,
        redacted_by: None,
    };
    let created_by = append
        .actor
        .map(serde_json::to_string)
        .transpose()
        .map_err(mem_err)?;
    sqlx::query(&format!(
        "INSERT INTO {NS}_versions \
         (store_id, ordinal, id, memory_id, operation, path, content, created, redacted, created_by_json, redacted_by_json) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, NULL)"
    ))
    .bind(append.store)
    .bind(ordinal)
    .bind(&version.id)
    .bind(append.memory_id)
    .bind(operation_name(append.operation))
    .bind(append.path)
    .bind(append.content.map(str::as_bytes))
    .bind(append.created)
    .bind(created_by)
    .execute(&mut *tx)
    .await
    .map_err(mem_err)?;
    Ok(version)
}

#[async_trait::async_trait]
impl MemoryRepository for SqliteMemoryRepository {
    async fn preserve_copy_conflict(
        &self,
        store: &str,
        candidate: &awaken_resource_contract::MemoryCopyConflict,
    ) -> Result<(), MemErr> {
        crate::repository::validate_copy_conflict(candidate)?;
        let store = store.to_owned();
        let id = candidate.id();
        let encoded = serde_json::to_string(candidate).map_err(mem_err)?;
        with_conn_mem(&self.pool, move |mut conn| async move {
            // Reserve the SQLite write transaction BEFORE observing absence or
            // capacity, including independent connections after Worker restart.
            let mut tx = sqlx::Connection::begin_with(&mut *conn, "BEGIN IMMEDIATE")
                .await
                .map_err(mem_err)?;
            let existing: Option<String> = sqlx::query_scalar(&format!(
                "SELECT candidate_json FROM {NS}_copy_conflicts WHERE store_id = ?1 AND id = ?2"
            ))
            .bind(&store)
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
                "SELECT COUNT(*) FROM {NS}_copy_conflicts WHERE store_id = ?1"
            ))
            .bind(&store)
            .fetch_one(&mut *tx)
            .await
            .map_err(mem_err)?;
            if count >= crate::MAX_MEMORIES_PER_STORE as i64 {
                return Err(MemErr::AtCapacity);
            }
            sqlx::query(&format!(
                "INSERT INTO {NS}_copy_conflicts (store_id, id, candidate_json) VALUES (?1, ?2, ?3)"
            ))
            .bind(&store)
            .bind(&id)
            .bind(&encoded)
            .execute(&mut *tx)
            .await
            .map_err(mem_err)?;
            tx.commit().await.map_err(mem_err)
        })
        .await
    }

    async fn list_copy_conflicts(
        &self,
        store: &str,
    ) -> Result<Vec<awaken_resource_contract::MemoryCopyConflict>, MemErr> {
        let rows: Vec<(String, String)> = sqlx::query_as(&format!(
            "SELECT id, candidate_json FROM {NS}_copy_conflicts WHERE store_id = ?1 ORDER BY id"
        ))
        .bind(store)
        .fetch_all(&self.pool)
        .await
        .map_err(mem_err)?;
        rows.into_iter()
            .map(|(id, encoded)| crate::repository::decode_copy_conflict(&id, &encoded))
            .collect()
    }

    async fn snapshot_heads(&self, store: &str) -> Result<Vec<Memory>, MemErr> {
        let rows: Vec<(String, String, Vec<u8>, String, i64, i64, i64)> = sqlx::query_as(&format!(
            "SELECT id, path, content, sha, version, created, updated \
                 FROM {NS}_memories WHERE store_id = ?1 ORDER BY path"
        ))
        .bind(store)
        .fetch_all(&self.pool)
        .await
        .map_err(mem_err)?;
        rows.into_iter()
            .map(|(id, path, content, sha, version, created, updated)| {
                row_memory(id, path, content, sha, version, created, updated)
            })
            .collect()
    }

    async fn list(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr> {
        let rows: Vec<(String, String, String, i64, i64, i64)> = sqlx::query_as(&format!(
            "SELECT id, path, sha, length(content), version, updated \
             FROM {NS}_memories WHERE store_id = ?1"
        ))
        .bind(store)
        .fetch_all(&self.pool)
        .await
        .map_err(mem_err)?;
        rows.into_iter()
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
            .collect::<Result<Vec<_>, MemErr>>()
    }

    async fn get_by_path(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr> {
        let row: Option<(String, String, Vec<u8>, String, i64, i64, i64)> =
            sqlx::query_as(&format!(
                "SELECT id, path, content, sha, version, created, updated \
                 FROM {NS}_memories WHERE store_id = ?1 AND path = ?2"
            ))
            .bind(store)
            .bind(path)
            .fetch_optional(&self.pool)
            .await
            .map_err(mem_err)?;
        match row {
            Some((id, path, content, sha, version, created, updated)) => Ok(Some(row_memory(
                id, path, content, sha, version, created, updated,
            )?)),
            None => Ok(None),
        }
    }

    async fn create(&self, store: &str, path: &str, content: &str) -> Result<Memory, MemErr> {
        self.create_as(store, path, content, None).await
    }

    async fn create_as(
        &self,
        store: &str,
        path: &str,
        content: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        validate_path(path)?;
        validate_size(content)?;
        let (store, path, content) = (store.to_string(), path.to_string(), content.to_string());
        let actor = actor.cloned();
        let clock = self.clock.clone();
        with_conn_mem(&self.pool, move |mut conn| async move {
            let mut tx = sqlx::Connection::begin(&mut *conn).await.map_err(mem_err)?;
            let exists = sqlx::query_scalar::<_, i64>(&format!(
                "SELECT 1 FROM {NS}_memories WHERE store_id = ?1 AND path = ?2"
            ))
            .bind(&store)
            .bind(&path)
            .fetch_optional(&mut *tx)
            .await
            .map_err(mem_err)?
            .is_some();
            if exists {
                return Err(MemErr::PathConflict(path));
            }
            let live_count: i64 = sqlx::query_scalar(&format!(
                "SELECT COUNT(*) FROM {NS}_memories WHERE store_id = ?1"
            ))
            .bind(&store)
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
            let sha = sha256_hex(&content);
            let now = stored_now_nanos(clock.as_ref())?;
            sqlx::query(&format!(
                "INSERT INTO {NS}_memories \
                 (store_id, path, id, ordinal, content, sha, version, created, updated) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?7)"
            ))
            .bind(&store)
            .bind(&path)
            .bind(&id)
            .bind(ordinal)
            .bind(content.as_bytes())
            .bind(&sha)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(mem_err)?;
            append_version(
                &mut tx,
                VersionAppend {
                    store: &store,
                    memory_id: &id,
                    operation: MemoryVersionOperation::Created,
                    path: &path,
                    content: Some(&content),
                    created: now,
                    actor: actor.as_ref(),
                },
            )
            .await?;
            tx.commit().await.map_err(mem_err)?;
            Ok(Memory {
                content_size: content.len() as u64,
                content: Some(content),
                id,
                path,
                content_sha256: sha,
                version: 1,
                created_unix_nanos: now as u128,
                updated_unix_nanos: now as u128,
            })
        })
        .await
    }

    async fn update_head(
        &self,
        store: &str,
        base: &MemoryGeneration,
        content: &str,
        target_path: Option<&str>,
    ) -> Result<Memory, MemErr> {
        self.update_head_as(store, base, content, target_path, None)
            .await
    }

    async fn update_head_as(
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

    async fn update_materialized(
        &self,
        store: &str,
        path: &str,
        base: &MemoryGeneration,
        content: &str,
    ) -> Result<Memory, MemErr> {
        self.update_head_checked(store, base, content, None, None, Some(path))
            .await
    }

    async fn rename(&self, store: &str, from: &str, to: &str) -> Result<Memory, MemErr> {
        validate_path(to)?;
        let (store, from, to) = (store.to_string(), from.to_string(), to.to_string());
        let clock = self.clock.clone();
        with_conn_mem(&self.pool, move |mut conn| async move {
            // Atomic POSIX replace: drop the destination and move the source in ONE
            // transaction, so a crash between the two writes can never leave the
            // destination deleted without the move completing (an uncommitted
            // transaction is rolled back on recovery). The transaction rolls back
            // if dropped before `commit` (any early return here — e.g. a mid-op
            // fault — undoes the DELETE).
            let mut tx = sqlx::Connection::begin(&mut *conn).await.map_err(mem_err)?;
            let row: Option<(String, Vec<u8>, String, i64, i64)> = sqlx::query_as(&format!(
                "SELECT id, content, sha, version, created FROM {NS}_memories \
                 WHERE store_id = ?1 AND path = ?2"
            ))
            .bind(&store)
            .bind(&from)
            .fetch_optional(&mut *tx)
            .await
            .map_err(mem_err)?;
            let (id, content, sha, version, created) =
                row.ok_or_else(|| MemErr::NotFound(from.clone()))?;
            if from == to {
                return row_memory(id, from, content, sha, version, created, created);
            }
            let now = stored_now_nanos(clock.as_ref())?;
            let displaced_id: Option<String> = sqlx::query_scalar(&format!(
                "SELECT id FROM {NS}_memories WHERE store_id = ?1 AND path = ?2"
            ))
            .bind(&store)
            .bind(&to)
            .fetch_optional(&mut *tx)
            .await
            .map_err(mem_err)?;
            let id = rename_replace_identity(id, displaced_id.as_ref());
            sqlx::query(&format!(
                "DELETE FROM {NS}_memories WHERE store_id = ?1 AND path = ?2"
            ))
            .bind(&store)
            .bind(&to)
            .execute(&mut *tx)
            .await
            .map_err(mem_err)?;
            sqlx::query(&format!(
                "UPDATE {NS}_memories SET path = ?1, version = version + 1, updated = ?2 \
                 WHERE store_id = ?3 AND id = ?4"
            ))
            .bind(&to)
            .bind(now)
            .bind(&store)
            .bind(&id)
            .execute(&mut *tx)
            .await
            .map_err(mem_err)?;
            if let Some(displaced_id) = displaced_id {
                append_version(
                    &mut tx,
                    VersionAppend {
                        store: &store,
                        memory_id: &displaced_id,
                        operation: MemoryVersionOperation::Deleted,
                        path: &to,
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
                    store: &store,
                    memory_id: &id,
                    operation: MemoryVersionOperation::Modified,
                    path: &to,
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
                path: to,
                content_sha256: sha,
                version: next_domain_version(version)?,
                created_unix_nanos: created as u128,
                updated_unix_nanos: now as u128,
            })
        })
        .await
    }

    async fn delete_by_path(&self, store: &str, path: &str) -> Result<(), MemErr> {
        self.delete_by_path_as(store, path, None).await
    }

    async fn delete_by_path_as(
        &self,
        store: &str,
        path: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<(), MemErr> {
        let (store, path) = (store.to_string(), path.to_string());
        let actor = actor.cloned();
        let clock = self.clock.clone();
        with_conn_mem(&self.pool, move |mut conn| async move {
            let mut tx = sqlx::Connection::begin(&mut *conn).await.map_err(mem_err)?;
            let memory_id: Option<String> = sqlx::query_scalar(&format!(
                "SELECT id FROM {NS}_memories WHERE store_id = ?1 AND path = ?2"
            ))
            .bind(&store)
            .bind(&path)
            .fetch_optional(&mut *tx)
            .await
            .map_err(mem_err)?;
            let Some(memory_id) = memory_id else {
                return Ok(());
            };
            sqlx::query(&format!(
                "DELETE FROM {NS}_memories WHERE store_id = ?1 AND path = ?2"
            ))
            .bind(&store)
            .bind(&path)
            .execute(&mut *tx)
            .await
            .map_err(mem_err)?;
            append_version(
                &mut tx,
                VersionAppend {
                    store: &store,
                    memory_id: &memory_id,
                    operation: MemoryVersionOperation::Deleted,
                    path: &path,
                    content: None,
                    created: stored_now_nanos(clock.as_ref())?,
                    actor: actor.as_ref(),
                },
            )
            .await?;
            tx.commit().await.map_err(mem_err)?;
            Ok(())
        })
        .await
    }

    async fn delete_if_match(
        &self,
        store: &str,
        path: &str,
        base: &MemoryGeneration,
    ) -> Result<bool, MemErr> {
        self.delete_if_match_as(store, path, base, None).await
    }

    async fn delete_if_match_as(
        &self,
        store: &str,
        path: &str,
        base: &MemoryGeneration,
        actor: Option<&MemoryActor>,
    ) -> Result<bool, MemErr> {
        let (store, path, base, actor) = (
            store.to_string(),
            path.to_string(),
            base.clone(),
            actor.cloned(),
        );
        let clock = self.clock.clone();
        with_conn_mem(&self.pool, move |mut conn| async move {
            let mut tx = sqlx::Connection::begin(&mut *conn).await.map_err(mem_err)?;
            let row: Option<(String, Vec<u8>, String, i64, i64, i64)> = sqlx::query_as(&format!(
                "SELECT id, content, sha, version, created, updated FROM {NS}_memories \
                 WHERE store_id = ?1 AND path = ?2"
            ))
            .bind(&store)
            .bind(&path)
            .fetch_optional(&mut *tx)
            .await
            .map_err(mem_err)?;
            let admission =
                ConditionalDeleteAdmission::new(row.as_ref().map(|(id, _, sha, version, _, _)| {
                    (
                        id == base.memory_id(),
                        domain_version(*version).is_ok_and(|version| version == base.version()),
                        sha == base.content_sha256(),
                    )
                }));
            match admission {
                ConditionalDeleteAdmission::Absent => return Ok(false),
                ConditionalDeleteAdmission::Conflict => {
                    let (id, content, sha, version, created, updated) =
                        row.expect("conflict requires the observed head");
                    return Err(MemErr::Conflict {
                        current: Box::new(row_memory(
                            id, path, content, sha, version, created, updated,
                        )?),
                    });
                }
                ConditionalDeleteAdmission::Delete => {}
            }
            sqlx::query(&format!(
                "DELETE FROM {NS}_memories WHERE store_id = ?1 AND path = ?2"
            ))
            .bind(&store)
            .bind(&path)
            .execute(&mut *tx)
            .await
            .map_err(mem_err)?;
            append_version(
                &mut tx,
                VersionAppend {
                    store: &store,
                    memory_id: base.memory_id(),
                    operation: MemoryVersionOperation::Deleted,
                    path: &path,
                    content: None,
                    created: stored_now_nanos(clock.as_ref())?,
                    actor: actor.as_ref(),
                },
            )
            .await?;
            tx.commit().await.map_err(mem_err)?;
            Ok(true)
        })
        .await
    }

    async fn list_versions(&self, store: &str) -> Result<Vec<MemoryVersion>, MemErr> {
        let rows: Vec<VersionRow> = sqlx::query_as(&format!(
            "SELECT id, memory_id, operation, path, content, created, redacted, created_by_json, redacted_by_json \
             FROM {NS}_versions WHERE store_id = ?1 ORDER BY ordinal"
        ))
        .bind(store)
        .fetch_all(&self.pool)
        .await
        .map_err(mem_err)?;
        rows.into_iter().map(row_version).collect()
    }

    async fn redact_version(
        &self,
        store: &str,
        version_id: &str,
    ) -> Result<Option<MemoryVersion>, MemErr> {
        self.redact_version_as(store, version_id, None).await
    }

    async fn redact_version_as(
        &self,
        store: &str,
        version_id: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Option<MemoryVersion>, MemErr> {
        let (store, version_id) = (store.to_string(), version_id.to_string());
        let actor = actor.cloned();
        let clock = self.clock.clone();
        with_conn_mem(&self.pool, move |mut conn| async move {
            let mut tx = sqlx::Connection::begin(&mut *conn).await.map_err(mem_err)?;
            let existing: Option<VersionRow> = sqlx::query_as(&format!(
                "SELECT id, memory_id, operation, path, content, created, redacted, created_by_json, redacted_by_json \
                 FROM {NS}_versions WHERE store_id = ?1 AND id = ?2"
            ))
            .bind(&store)
            .bind(&version_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(mem_err)?;
            let Some(mut version) = existing.map(row_version).transpose()? else {
                return Ok(None);
            };
            if version.redacted_unix_nanos.is_none() {
                let redacted = stored_now_nanos(clock.as_ref())?;
                sqlx::query(&format!(
                    "UPDATE {NS}_versions SET content = NULL, redacted = ?1, redacted_by_json = ?2 \
                     WHERE store_id = ?3 AND id = ?4"
                ))
                .bind(redacted)
                .bind(
                    actor
                        .as_ref()
                        .map(serde_json::to_string)
                        .transpose()
                        .map_err(mem_err)?,
                )
                .bind(&store)
                .bind(&version_id)
                .execute(&mut *tx)
                .await
                .map_err(mem_err)?;
                version.content = None;
                version.redacted_unix_nanos = Some(redacted as u128);
                version.redacted_by = actor.clone();
            }
            tx.commit().await.map_err(mem_err)?;
            Ok(Some(version))
        })
        .await
    }

    async fn purge_store(&self, store: &str) -> Result<MemoryPurgeSummary, MemErr> {
        let store = store.to_string();
        with_conn_mem(&self.pool, move |mut conn| async move {
            let mut tx = sqlx::Connection::begin(&mut *conn).await.map_err(mem_err)?;
            let copy_conflicts_deleted = sqlx::query(&format!(
                "DELETE FROM {NS}_copy_conflicts WHERE store_id = ?1"
            ))
            .bind(&store)
            .execute(&mut *tx)
            .await
            .map_err(mem_err)?
            .rows_affected();
            let versions_deleted =
                sqlx::query(&format!("DELETE FROM {NS}_versions WHERE store_id = ?1"))
                    .bind(&store)
                    .execute(&mut *tx)
                    .await
                    .map_err(mem_err)?
                    .rows_affected();
            let heads_deleted =
                sqlx::query(&format!("DELETE FROM {NS}_memories WHERE store_id = ?1"))
                    .bind(&store)
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
        })
        .await
    }
}

impl SqliteMemoryRepository {
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
        let (store, base, content, target_path) = (
            store.to_string(),
            base.clone(),
            content.to_string(),
            target_path.map(str::to_string),
        );
        let actor = actor.cloned();
        let expected_path = expected_path.map(str::to_owned);
        let clock = self.clock.clone();
        with_conn_mem(&self.pool, move |mut conn| async move {
            let mut tx = sqlx::Connection::begin(&mut *conn).await.map_err(mem_err)?;
            let row: Option<(String, String, Vec<u8>, i64, i64, i64)> = sqlx::query_as(&format!(
                "SELECT path, sha, content, version, created, updated FROM {NS}_memories \
                 WHERE store_id = ?1 AND id = ?2"
            ))
            .bind(&store)
            .bind(base.memory_id())
            .fetch_optional(&mut *tx)
            .await
            .map_err(mem_err)?;
            let (path, cur_sha, cur_content, version, created, updated) =
                row.ok_or_else(|| MemErr::NotFound(base.memory_id().to_owned()))?;
            let requested_path = target_path.unwrap_or_else(|| path.clone());
            let new_sha = sha256_hex(&content);
            let already_current = cur_sha == new_sha && path == requested_path;
            match HeadUpdateAdmission::new(
                expected_path
                    .as_deref()
                    .is_none_or(|expected| expected == path),
                true,
                domain_version(version)? == base.version(),
                cur_sha == base.content_sha256(),
                already_current,
            ) {
                HeadUpdateAdmission::Conflict => {
                    let current = row_memory(
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
                    return row_memory(
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
            let now = stored_now_nanos(clock.as_ref())?;
            let displaced_id: Option<String> = if requested_path == path {
                None
            } else {
                sqlx::query_scalar(&format!(
                    "SELECT id FROM {NS}_memories WHERE store_id = ?1 AND path = ?2"
                ))
                .bind(&store)
                .bind(&requested_path)
                .fetch_optional(&mut *tx)
                .await
                .map_err(mem_err)?
            };
            if displaced_id.is_some() {
                sqlx::query(&format!(
                    "DELETE FROM {NS}_memories WHERE store_id = ?1 AND path = ?2"
                ))
                .bind(&store)
                .bind(&requested_path)
                .execute(&mut *tx)
                .await
                .map_err(mem_err)?;
            }
            sqlx::query(&format!(
                "UPDATE {NS}_memories SET path = ?1, content = ?2, sha = ?3, \
                 version = version + 1, updated = ?4 WHERE store_id = ?5 AND id = ?6"
            ))
            .bind(&requested_path)
            .bind(content.as_bytes())
            .bind(&new_sha)
            .bind(now)
            .bind(&store)
            .bind(base.memory_id())
            .execute(&mut *tx)
            .await
            .map_err(mem_err)?;
            if let Some(displaced_id) = displaced_id {
                append_version(
                    &mut tx,
                    VersionAppend {
                        store: &store,
                        memory_id: &displaced_id,
                        operation: MemoryVersionOperation::Deleted,
                        path: &requested_path,
                        content: None,
                        created: now,
                        actor: actor.as_ref(),
                    },
                )
                .await?;
            }
            append_version(
                &mut tx,
                VersionAppend {
                    store: &store,
                    memory_id: base.memory_id(),
                    operation: MemoryVersionOperation::Modified,
                    path: &requested_path,
                    content: Some(&content),
                    created: now,
                    actor: actor.as_ref(),
                },
            )
            .await?;
            tx.commit().await.map_err(mem_err)?;
            Ok(Memory {
                content_size: content.len() as u64,
                content: Some(content),
                id: base.memory_id().to_owned(),
                path: requested_path,
                content_sha256: new_sha,
                version: next_domain_version(version)?,
                created_unix_nanos: created as u128,
                updated_unix_nanos: now as u128,
            })
        })
        .await
    }
}

#[cfg(test)]
mod migration_seam_tests {
    use super::*;
    use crate::schema::memory_store_bundle;

    #[tokio::test]
    async fn current_baseline_exposes_only_the_memory_aggregate() {
        // Causes: C1 empty ledger; C2 current V1/V2/V3 stream; C3 exact replay.
        // Effects: E1 create heads, versions, and counters only; E2 apply both
        // versions once; E3 apply nothing. Decision rules M1=C1+C2=>E1+E2;
        // M2=C3=>E3. Drift rejection is owned by the common runner tests.
        let conn = EmbeddedSqlite::memory().open().await.expect("open sqlite");
        let full = memory_store_bundle().expect("bundle builds");
        let runner = SqliteMigrationRunner::with_prefix(conn.sqlx().clone(), NS).expect("runner");

        let first = runner.run_bundle(&full).await.expect("M1 apply V1/V2/V3");
        assert_eq!(
            first.iter().map(|m| m.version).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type='table' \
             AND name LIKE 'memory_store_%' \
             AND name NOT LIKE 'memory_store_schema_migrations%' ORDER BY name",
        )
        .fetch_all(&conn)
        .await
        .unwrap();
        assert_eq!(
            tables,
            [
                "memory_store_counters",
                "memory_store_memories",
                "memory_store_versions"
            ],
            "M1/E1"
        );
        let retired: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='memory_store_blob'",
        )
        .fetch_one(&conn)
        .await
        .unwrap();
        assert_eq!(retired, 0, "M1/E1");
        assert!(runner.run_bundle(&full).await.expect("M2").is_empty());
    }

    /// `over` wraps a pool without migrating; the store only works once the
    /// caller opts into the `memory_store` scope via `ensure_schema`.
    #[tokio::test]
    async fn over_does_not_migrate_but_ensure_schema_does() {
        let fs = SqliteMemoryRepository::over(
            EmbeddedSqlite::memory().open().await.unwrap(),
            awaken_time::system_clock(),
        );
        assert!(fs.create("s", "/a.md", "alpha").await.is_err());
        fs.ensure_schema().await.unwrap();
        assert_eq!(
            fs.create("s", "/a.md", "alpha")
                .await
                .unwrap()
                .content
                .as_deref(),
            Some("alpha")
        );
    }
}

#[cfg(test)]
mod memory_repository_tests {
    use super::*;

    #[tokio::test]
    async fn sqlite_memory_repository_conforms() {
        crate::conformance::conformance(
            &SqliteMemoryRepository::open_in_memory(awaken_time::system_clock())
                .await
                .unwrap(),
        )
        .await;
    }

    /// The same cause-effect-graph edge cases as the ephemeral backend, exercised in
    /// `crate::conformance::extended_conformance`, run against SQLite so the
    /// CAS/rename **precedence** and prefix-boundary rules are pinned here too.
    #[tokio::test]
    async fn sqlite_memory_repository_extended_conformance() {
        crate::conformance::extended_conformance(
            &SqliteMemoryRepository::open_in_memory(awaken_time::system_clock())
                .await
                .unwrap(),
        )
        .await;
    }

    #[tokio::test]
    async fn sqlite_head_update_is_one_atomic_transaction() {
        crate::conformance::atomic_head_update_conformance(
            &SqliteMemoryRepository::open_in_memory(awaken_time::system_clock())
                .await
                .unwrap(),
        )
        .await;
    }

    /// The shared "who masks whom" edge: a rename-replace over SQLite
    /// drops the destination row entirely, so the replaced destination's id is
    /// `NotFound` and the destination path carries the source id + content.
    #[tokio::test]
    async fn sqlite_rename_replace_orphans_destination_id() {
        crate::conformance::rename_replace_orphans_the_destination_id(
            &SqliteMemoryRepository::open_in_memory(awaken_time::system_clock())
                .await
                .unwrap(),
        )
        .await;
    }

    /// Durable high-water counters prevent a deleted memory id from ever being
    /// reissued. This must agree with the in-memory aggregate.
    #[tokio::test]
    async fn durable_ids_remain_monotonic_after_delete() {
        crate::conformance::ids_stay_monotonic_across_delete(
            &SqliteMemoryRepository::open_in_memory(awaken_time::system_clock())
                .await
                .unwrap(),
        )
        .await;
    }

    #[tokio::test]
    async fn sqlite_memory_repository_survives_reopen() {
        // Actor durability decision table: D1 attributed create -> created_by;
        // D2 attributed update -> its own created_by; D3 attributed redact ->
        // redacted_by while created_by is immutable; D4 reopen -> all actor DTOs
        // remain byte-equivalent. Actor JSON shares the version transaction.
        let dir = std::env::temp_dir().join(format!("awaken-sql-memory-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("m.db");
        let path_str = path.to_str().unwrap();
        let version_id;
        {
            let fs = SqliteMemoryRepository::open(path_str, awaken_time::system_clock())
                .await
                .unwrap();
            let api_actor = MemoryActor::ApiActor {
                api_key_id: "api_durable".into(),
            };
            let redact_actor = MemoryActor::ServiceAccountActor {
                service_account_id: "svac_redactor".into(),
            };
            let created = fs
                .create_as("s", "/keep.md", "durable", Some(&api_actor))
                .await
                .unwrap();
            fs.update_head_as(
                "s",
                &created.generation(),
                "updated",
                None,
                Some(&api_actor),
            )
            .await
            .unwrap();
            version_id = fs.list_versions("s").await.unwrap()[0].id.clone();
            fs.redact_version_as("s", &version_id, Some(&redact_actor))
                .await
                .unwrap()
                .unwrap();
        }
        let fs = SqliteMemoryRepository::open(path_str, awaken_time::system_clock())
            .await
            .unwrap();
        assert_eq!(
            fs.get_by_path("s", "/keep.md")
                .await
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("updated")
        );
        let versions = fs.list_versions("s").await.unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].id, version_id);
        assert!(versions[0].content.is_none());
        assert!(versions[0].redacted_unix_nanos.is_some());
        assert_eq!(
            versions[0].created_by,
            Some(MemoryActor::ApiActor {
                api_key_id: "api_durable".into()
            }),
            "D1/D4"
        );
        assert_eq!(
            versions[0].redacted_by,
            Some(MemoryActor::ServiceAccountActor {
                service_account_id: "svac_redactor".into()
            }),
            "D3/D4"
        );
        assert_eq!(versions[1].created_by, versions[0].created_by, "D2/D4");
        assert_eq!(versions[1].content.as_deref(), Some("updated"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A crash mid-`rename` (after the destination is deleted, before the source is
    /// moved) must lose nothing: the rename wraps both writes in one transaction, so
    /// an un-committed transaction rolls back on recovery. We model the crash by
    /// running the destination DELETE in a transaction that is dropped before commit,
    /// then assert both memories are intact.
    #[tokio::test]
    async fn rename_replace_is_crash_atomic() {
        let fs = SqliteMemoryRepository::open_in_memory(awaken_time::system_clock())
            .await
            .unwrap();
        fs.create("s", "/from.md", "src").await.unwrap();
        fs.create("s", "/to.md", "dst").await.unwrap();

        // Simulate a crash: delete the destination inside a transaction, then abandon
        // it (drop without commit) — exactly the interrupted-rename window.
        {
            let mut tx = fs.pool.begin().await.unwrap();
            sqlx::query(&format!(
                "DELETE FROM {NS}_memories WHERE store_id = ?1 AND path = ?2"
            ))
            .bind("s")
            .bind("/to.md")
            .execute(&mut *tx)
            .await
            .unwrap();
            // tx dropped here without commit == process died mid-rename.
        }
        assert!(
            fs.get_by_path("s", "/to.md").await.unwrap().is_some(),
            "an aborted rename must not lose the destination"
        );
        assert!(fs.get_by_path("s", "/from.md").await.unwrap().is_some());

        // A real rename still replaces atomically and preserves the source's id.
        let src = fs.get_by_path("s", "/from.md").await.unwrap().unwrap();
        let renamed = fs.rename("s", "/from.md", "/to.md").await.unwrap();
        assert_eq!(renamed.id, src.id, "the moved memory keeps its id");
        assert_eq!(renamed.path, "/to.md");
        assert!(fs.get_by_path("s", "/from.md").await.unwrap().is_none());
        assert_eq!(
            fs.get_by_path("s", "/to.md")
                .await
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("src"),
            "the destination now holds the moved content"
        );
    }

    #[tokio::test]
    async fn sqlite_store_enforces_the_shared_capacity_contract() {
        crate::repository::tests::capacity_conformance(
            &SqliteMemoryRepository::open_in_memory(awaken_time::system_clock())
                .await
                .unwrap(),
        )
        .await;
    }
}
