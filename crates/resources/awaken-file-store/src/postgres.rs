//! Postgres backend (`postgres` feature): content-addressed blobs in a `bytea`
//! column. `put` is an idempotent `INSERT ... ON CONFLICT (id) DO NOTHING`, matching
//! the immutable/dedup contract. Compile-verified; running needs a database.

use async_trait::async_trait;
use sqlx::{PgPool, Row};

use crate::schema::{NS, file_store_bundle};
use crate::{FileStore, FileStoreError, content_id};

fn e(x: impl ToString) -> FileStoreError {
    FileStoreError(x.to_string())
}

/// A Postgres-backed [`FileStore`] over a `file_store_blob(id, bytes, size, created_at)` table.
pub struct PgFileStore {
    pool: PgPool,
}

impl PgFileStore {
    /// Connect to `url` and ensure the schema exists.
    pub async fn connect(url: &str) -> Result<Self, FileStoreError> {
        let pool = PgPool::connect(url).await.map_err(e)?;
        let store = Self { pool };
        store.ensure_schema().await?;
        Ok(store)
    }

    /// Wrap an existing pool (schema assumed present, or call [`Self::ensure_schema`]).
    pub fn with_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Apply the versioned `awaken.file_store` migration bundle (idempotent; the
    /// runner records applied versions in `file_store_schema_migrations`). Replaces
    /// the former unversioned `CREATE TABLE IF NOT EXISTS`.
    pub async fn ensure_schema(&self) -> Result<(), FileStoreError> {
        let bundle = file_store_bundle().map_err(e)?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(
            self.pool.clone(),
            NS,
        )
        .map_err(e)?
        .run_bundle(&bundle)
        .await
        .map_err(e)?;
        Ok(())
    }
}

#[async_trait]
impl FileStore for PgFileStore {
    async fn put(&self, bytes: &[u8]) -> Result<String, FileStoreError> {
        let id = content_id(bytes);
        sqlx::query(
            "INSERT INTO file_store_blob (id, bytes, size) VALUES ($1, $2, $3) \
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(&id)
        .bind(bytes)
        .bind(bytes.len() as i64)
        .execute(&self.pool)
        .await
        .map_err(e)?;
        Ok(id)
    }

    async fn get(&self, id: &str) -> Result<Option<Vec<u8>>, FileStoreError> {
        let row = sqlx::query("SELECT bytes FROM file_store_blob WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(e)?;
        Ok(row.map(|r| r.get::<Vec<u8>, _>("bytes")))
    }

    async fn list(&self) -> Result<Vec<String>, FileStoreError> {
        // COLLATE "C" = raw byte order, matching fs/in-mem/sqlite (the FileStore
        // contract requires every backend's `list` to be byte-for-byte comparable).
        let rows = sqlx::query("SELECT id FROM file_store_blob ORDER BY id COLLATE \"C\"")
            .fetch_all(&self.pool)
            .await
            .map_err(e)?;
        Ok(rows.iter().map(|r| r.get::<String, _>("id")).collect())
    }

    async fn delete(&self, id: &str) -> Result<bool, FileStoreError> {
        let res = sqlx::query("DELETE FROM file_store_blob WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(e)?;
        Ok(res.rows_affected() > 0)
    }
}
