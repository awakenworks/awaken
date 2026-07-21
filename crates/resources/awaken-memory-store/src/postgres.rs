//! Postgres [`MemoryFs`] over the crate's `memory_store` migration scope — the
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

use crate::memfs::{now_nanos, under_prefix, validate_path, validate_size};
use crate::{MemErr, Memory, MemoryEntry, MemoryFs, sha256_hex};

fn mem_err(err: impl std::fmt::Display) -> MemErr {
    MemErr::Storage(err.to_string())
}

/// A Postgres-backed [`MemoryFs`] (path-addressed, CAS). Compare-and-swap and rename
/// run in a transaction with `SELECT … FOR UPDATE`, so the optimistic-concurrency
/// guarantee holds **across nodes** (the reason ADR-0057 chose Postgres).
pub struct PgMemoryFs {
    pool: PgPool,
}

impl PgMemoryFs {
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

#[async_trait::async_trait]
impl MemoryFs for PgMemoryFs {
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
        let ordinal: i64 = sqlx::query_scalar(&format!(
            "SELECT COALESCE(MAX(ordinal), 0) + 1 FROM {NS}_memories"
        ))
        .fetch_one(&mut *tx)
        .await
        .map_err(mem_err)?;
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
        tx.commit().await.map_err(mem_err)?;
        Ok(Memory {
            content_size: content.len() as u64,
            content: Some(String::from_utf8(content).map_err(mem_err)?),
            id,
            path: to.to_string(),
            content_sha256: sha,
            version: (version + 1) as u64,
            created_unix_nanos: created as u128,
            updated_unix_nanos: now as u128,
        })
    }

    async fn delete_by_path(&self, store: &str, path: &str) -> Result<(), MemErr> {
        sqlx::query(&format!(
            "DELETE FROM {NS}_memories WHERE store_id = $1 AND path = $2"
        ))
        .bind(store)
        .bind(path)
        .execute(&self.pool)
        .await
        .map_err(mem_err)?;
        Ok(())
    }
}
