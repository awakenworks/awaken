//! Postgres [`MemoryRepository`] over the crate's `memory_store` migration scope — the
//! multi-node sibling of the SQLite backend over the same portable bundle.

use sqlx::Row;
use sqlx::postgres::PgPool;

use crate::schema::memory_store_bundle;

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
    let bundle = memory_store_bundle().map_err(|e| PgStoreError::Migrate(e.to_string()))?;
    awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
        .map_err(|e| PgStoreError::Migrate(e.to_string()))?
        .run_bundle(&bundle)
        .await
        .map_err(|e| PgStoreError::Migrate(e.to_string()))?;
    Ok(())
}

use crate::repository::{now_nanos, under_prefix, validate_path, validate_size};
use crate::{
    MemErr, Memory, MemoryEntry, MemoryPurgeSummary, MemoryRepository, MemoryVersion,
    MemoryVersionOperation, sha256_hex,
};

fn mem_err(err: impl std::fmt::Display) -> MemErr {
    MemErr::Storage(err.to_string())
}

/// A Postgres-backed [`MemoryRepository`] (path-addressed, CAS). Compare-and-swap and rename
/// run in a transaction with `SELECT … FOR UPDATE`, so the optimistic-concurrency
/// guarantee holds **across nodes** (the reason ADR-0057 chose Postgres).
pub struct PostgresMemoryRepository {
    pool: PgPool,
}

impl PostgresMemoryRepository {
    /// Connect and apply the memory-store migrations (one-step convenience).
    pub async fn connect(url: &str) -> Result<Self, PgStoreError> {
        let pool = PgPool::connect(url)
            .await
            .map_err(|e| PgStoreError::Connect(e.to_string()))?;
        let store = Self::with_pool(pool);
        store.ensure_schema().await?;
        Ok(store)
    }

    /// Wrap an existing pool **without migrating**, allowing a unified migration
    /// pipeline to own the shared database.
    pub fn with_pool(pool: PgPool) -> Self {
        Self { pool }
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
        version: version as u64,
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
        redacted_unix_nanos: row
            .get::<Option<i64>, _>("redacted")
            .map(|value| value as u128),
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
    store: &str,
    memory_id: &str,
    operation: MemoryVersionOperation,
    path: &str,
    content: Option<&str>,
    created: i64,
) -> Result<(), MemErr> {
    let ordinal = next_counter(
        tx,
        "memory_version",
        &format!("SELECT COALESCE(MAX(ordinal), 0) FROM {NS}_versions"),
    )
    .await?;
    sqlx::query(&format!(
        "INSERT INTO {NS}_versions \
         (store_id, ordinal, id, memory_id, operation, path, content, created, redacted) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NULL)"
    ))
    .bind(store)
    .bind(ordinal)
    .bind(format!("memver_{ordinal:016}"))
    .bind(memory_id)
    .bind(operation_name(operation))
    .bind(path)
    .bind(content.map(str::as_bytes))
    .bind(created)
    .execute(&mut **tx)
    .await
    .map_err(mem_err)?;
    Ok(())
}

#[async_trait::async_trait]
impl MemoryRepository for PostgresMemoryRepository {
    async fn list(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr> {
        let rows = sqlx::query(&format!(
            "SELECT id, path, sha, length(content) AS n, version, updated \
             FROM {NS}_memories WHERE store_id = $1"
        ))
        .bind(store)
        .fetch_all(&self.pool)
        .await
        .map_err(mem_err)?;
        Ok(rows
            .into_iter()
            .map(|r| MemoryEntry {
                id: r.get("id"),
                path: r.get("path"),
                content_sha256: r.get("sha"),
                content_size: r.get::<i32, _>("n") as u64,
                version: r.get::<i64, _>("version") as u64,
                updated_unix_nanos: r.get::<i64, _>("updated") as u128,
            })
            .filter(|e| under_prefix(&e.path, prefix))
            .collect())
    }

    async fn get_by_path(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr> {
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

    async fn create(&self, store: &str, path: &str, content: &str) -> Result<Memory, MemErr> {
        validate_path(path)?;
        validate_size(content)?;
        let mut tx = self.pool.begin().await.map_err(mem_err)?;
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
        let ordinal = next_counter(
            &mut tx,
            "memory_id",
            &format!("SELECT COALESCE(MAX(ordinal), 0) FROM {NS}_memories"),
        )
        .await?;
        let id = format!("mem_{ordinal}");
        let sha = sha256_hex(content);
        let now = now_nanos() as i64;
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
            store,
            &id,
            MemoryVersionOperation::Created,
            path,
            Some(content),
            now,
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

    async fn update(
        &self,
        store: &str,
        id: &str,
        content: &str,
        base_sha: &str,
    ) -> Result<Memory, MemErr> {
        validate_size(content)?;
        let mut tx = self.pool.begin().await.map_err(mem_err)?;
        let row = sqlx::query(&format!(
            "SELECT path, sha, content, version, created FROM {NS}_memories \
             WHERE store_id = $1 AND id = $2 FOR UPDATE"
        ))
        .bind(store)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(mem_err)?
        .ok_or_else(|| MemErr::NotFound(id.to_string()))?;
        let (path, cur_sha, cur_content, version, created): (String, String, Vec<u8>, i64, i64) = (
            row.get("path"),
            row.get("sha"),
            row.get("content"),
            row.get("version"),
            row.get("created"),
        );
        let new_sha = sha256_hex(content);
        if cur_sha != base_sha {
            if cur_sha == new_sha {
                return to_memory(
                    id.into(),
                    path,
                    cur_content,
                    cur_sha,
                    version,
                    created,
                    created,
                );
            }
            let current = to_memory(
                id.into(),
                path,
                cur_content,
                cur_sha,
                version,
                created,
                created,
            )?;
            return Err(MemErr::Conflict {
                current: Box::new(current),
            });
        }
        let now = now_nanos() as i64;
        sqlx::query(&format!(
            "UPDATE {NS}_memories SET content = $1, sha = $2, version = version + 1, updated = $3 \
             WHERE store_id = $4 AND id = $5"
        ))
        .bind(content.as_bytes())
        .bind(&new_sha)
        .bind(now)
        .bind(store)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(mem_err)?;
        append_version(
            &mut tx,
            store,
            id,
            MemoryVersionOperation::Modified,
            &path,
            Some(content),
            now,
        )
        .await?;
        tx.commit().await.map_err(mem_err)?;
        Ok(Memory {
            content_size: content.len() as u64,
            content: Some(content.to_string()),
            id: id.to_string(),
            path,
            content_sha256: new_sha,
            version: (version + 1) as u64,
            created_unix_nanos: created as u128,
            updated_unix_nanos: now as u128,
        })
    }

    async fn rename(&self, store: &str, from: &str, to: &str) -> Result<Memory, MemErr> {
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
        let now = now_nanos() as i64;
        let displaced_id = sqlx::query_scalar::<_, String>(&format!(
            "SELECT id FROM {NS}_memories WHERE store_id = $1 AND path = $2 FOR UPDATE"
        ))
        .bind(store)
        .bind(to)
        .fetch_optional(&mut *tx)
        .await
        .map_err(mem_err)?;
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
                store,
                &displaced_id,
                MemoryVersionOperation::Deleted,
                to,
                None,
                now,
            )
            .await?;
        }
        let content = String::from_utf8(content).map_err(mem_err)?;
        append_version(
            &mut tx,
            store,
            &id,
            MemoryVersionOperation::Modified,
            to,
            Some(&content),
            now,
        )
        .await?;
        tx.commit().await.map_err(mem_err)?;
        Ok(Memory {
            content_size: content.len() as u64,
            content: Some(content),
            id,
            path: to.to_string(),
            content_sha256: sha,
            version: (version + 1) as u64,
            created_unix_nanos: created as u128,
            updated_unix_nanos: now as u128,
        })
    }

    async fn delete_by_path(&self, store: &str, path: &str) -> Result<(), MemErr> {
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
            store,
            &memory_id,
            MemoryVersionOperation::Deleted,
            path,
            None,
            now_nanos() as i64,
        )
        .await?;
        tx.commit().await.map_err(mem_err)?;
        Ok(())
    }

    async fn delete_if_match(
        &self,
        store: &str,
        path: &str,
        base_id: &str,
        base_sha: &str,
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
        let Some(row) = row else {
            return Ok(false);
        };
        let id: String = row.get("id");
        let sha: String = row.get("sha");
        if id != base_id || sha != base_sha {
            return Err(MemErr::Conflict {
                current: Box::new(to_memory(
                    id,
                    path.to_string(),
                    row.get("content"),
                    sha,
                    row.get("version"),
                    row.get("created"),
                    row.get("updated"),
                )?),
            });
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
            store,
            base_id,
            MemoryVersionOperation::Deleted,
            path,
            None,
            now_nanos() as i64,
        )
        .await?;
        tx.commit().await.map_err(mem_err)?;
        Ok(true)
    }

    async fn list_versions(&self, store: &str) -> Result<Vec<MemoryVersion>, MemErr> {
        sqlx::query(&format!(
            "SELECT id, memory_id, operation, path, content, created, redacted \
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

    async fn redact_version(
        &self,
        store: &str,
        version_id: &str,
    ) -> Result<Option<MemoryVersion>, MemErr> {
        let mut tx = self.pool.begin().await.map_err(mem_err)?;
        let row = sqlx::query(&format!(
            "SELECT id, memory_id, operation, path, content, created, redacted \
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
            let redacted = now_nanos() as i64;
            sqlx::query(&format!(
                "UPDATE {NS}_versions SET content = NULL, redacted = $1 \
                 WHERE store_id = $2 AND id = $3"
            ))
            .bind(redacted)
            .bind(store)
            .bind(version_id)
            .execute(&mut *tx)
            .await
            .map_err(mem_err)?;
            version.content = None;
            version.redacted_unix_nanos = Some(redacted as u128);
        }
        tx.commit().await.map_err(mem_err)?;
        Ok(Some(version))
    }

    async fn purge_store(&self, store: &str) -> Result<MemoryPurgeSummary, MemErr> {
        let mut tx = self.pool.begin().await.map_err(mem_err)?;
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
        })
    }
}
