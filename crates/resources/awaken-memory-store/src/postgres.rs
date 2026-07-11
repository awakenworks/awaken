//! Postgres [`MemoryBlobStore`] over the crate's `memory_store` migration scope —
//! the multi-node sibling of the sqlite backend, the same portable bundle.

use sqlx::Row;
use sqlx::postgres::PgPool;

use crate::schema::memory_store_bundle;
use crate::{MemoryBlobStore, MemoryStoreError, sanitize_stem};

const NS: &str = "memory_store";

/// Errors from connecting or migrating the Postgres store.
#[derive(Debug, thiserror::Error)]
pub enum PgStoreError {
    #[error("connect: {0}")]
    Connect(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

async fn pool_migrated(pool: PgPool) -> Result<PgPool, PgStoreError> {
    let bundle = memory_store_bundle().map_err(|e| PgStoreError::Migrate(e.to_string()))?;
    awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
        .map_err(|e| PgStoreError::Migrate(e.to_string()))?
        .run_bundle(&bundle)
        .await
        .map_err(|e| PgStoreError::Migrate(e.to_string()))?;
    Ok(pool)
}

fn storage(err: impl std::fmt::Display) -> MemoryStoreError {
    MemoryStoreError::Storage(err.to_string())
}

/// A Postgres-backed [`MemoryBlobStore`].
pub struct PgMemoryBlobStore {
    pool: PgPool,
}

impl PgMemoryBlobStore {
    /// Connect and apply the memory-store migrations under the `memory_store` namespace.
    pub async fn connect(url: &str) -> Result<Self, PgStoreError> {
        let pool = PgPool::connect(url)
            .await
            .map_err(|e| PgStoreError::Connect(e.to_string()))?;
        Ok(Self {
            pool: pool_migrated(pool).await?,
        })
    }

    /// Build from an existing pool: apply the memory-store migrations.
    pub async fn with_pool(pool: PgPool) -> Result<Self, PgStoreError> {
        Ok(Self {
            pool: pool_migrated(pool).await?,
        })
    }
}

#[async_trait::async_trait]
impl MemoryBlobStore for PgMemoryBlobStore {
    async fn create(&self, workspace_id: &str) -> Result<String, MemoryStoreError> {
        // Dense global id: max ordinal + 1, inserted empty, in one statement.
        let row = sqlx::query(&format!(
            "INSERT INTO {NS}_blob (workspace_id, id, ordinal, content) \
             SELECT $1, 'memstore_' || x.n::text, x.n, ''::bytea \
             FROM (SELECT COALESCE(MAX(ordinal), 0) + 1 AS n FROM {NS}_blob) x \
             RETURNING id"
        ))
        .bind(workspace_id)
        .fetch_one(&self.pool)
        .await
        .map_err(storage)?;
        row.try_get::<String, _>("id").map_err(storage)
    }

    async fn put(
        &self,
        workspace_id: &str,
        id: &str,
        bytes: &[u8],
    ) -> Result<(), MemoryStoreError> {
        sqlx::query(&format!(
            "INSERT INTO {NS}_blob (workspace_id, id, ordinal, content) VALUES ($1, $2, 0, $3) \
             ON CONFLICT (workspace_id, id) DO UPDATE SET content = excluded.content"
        ))
        .bind(workspace_id)
        .bind(sanitize_stem(id))
        .bind(bytes)
        .execute(&self.pool)
        .await
        .map_err(storage)?;
        Ok(())
    }

    async fn get(&self, workspace_id: &str, id: &str) -> Result<Option<Vec<u8>>, MemoryStoreError> {
        let row = sqlx::query(&format!(
            "SELECT content FROM {NS}_blob WHERE workspace_id = $1 AND id = $2"
        ))
        .bind(workspace_id)
        .bind(sanitize_stem(id))
        .fetch_optional(&self.pool)
        .await
        .map_err(storage)?;
        row.map(|r| r.try_get::<Vec<u8>, _>("content").map_err(storage))
            .transpose()
    }

    async fn exists(&self, workspace_id: &str, id: &str) -> Result<bool, MemoryStoreError> {
        Ok(self.get(workspace_id, id).await?.is_some())
    }
}
