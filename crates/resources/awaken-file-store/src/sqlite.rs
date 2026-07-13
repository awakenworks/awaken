//! SQLite backend (`sqlite` feature): content-addressed blobs in a `BLOB`
//! column, over the crate's own `file_store` migration scope
//! ([`file_store_bundle`]) — the embedded sibling of [`PgFileStore`](crate::postgres::PgFileStore).
//! `put` is an idempotent `INSERT ... ON CONFLICT (id) DO NOTHING`, matching the
//! immutable/dedup contract; the id is computed in the core, so equal bytes yield
//! the same id on every backend. The `FileStore` trait is async, so the sync
//! `rusqlite` calls run under `spawn_blocking` behind a connection mutex.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, params};

use crate::schema::{NS, file_store_bundle};
use crate::{FileStore, FileStoreError, content_id};

fn e(x: impl ToString) -> FileStoreError {
    FileStoreError(x.to_string())
}

/// A SQLite-backed [`FileStore`] over a `file_store_blob(id, bytes, size, created_at)` table.
pub struct SqliteFileStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteFileStore {
    /// Open (or create) a database file and apply the file-store migrations
    /// (one-step convenience for a store-owned database).
    pub fn open(path: &str) -> Result<Self, FileStoreError> {
        let store = Self::over(Connection::open(path).map_err(e)?);
        store.ensure_schema()?;
        Ok(store)
    }

    /// Open a private in-memory database (tests / ephemeral).
    pub fn open_in_memory() -> Result<Self, FileStoreError> {
        let store = Self::over(Connection::open_in_memory().map_err(e)?);
        store.ensure_schema()?;
        Ok(store)
    }

    /// Wrap an existing connection **without migrating**. Call
    /// [`Self::ensure_schema`], or let a unified migration pipeline own the
    /// `file_store` scope so this store shares the caller's database.
    pub fn over(conn: Connection) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
        }
    }

    /// Apply the `file_store` scoped migration bundle (idempotent). Optional:
    /// skip it when the schema is owned externally.
    pub fn ensure_schema(&self) -> Result<(), FileStoreError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| e("file store connection poisoned"))?;
        let bundle = file_store_bundle().map_err(e)?;
        awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
            .map_err(e)?
            .run_bundle(&conn, &bundle)
            .map_err(e)?;
        Ok(())
    }

    async fn with_conn<T, F>(&self, f: F) -> Result<T, FileStoreError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, FileStoreError> + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn
                .lock()
                .map_err(|_| e("file store connection poisoned"))?;
            f(&guard)
        })
        .await
        .map_err(e)?
    }
}

#[async_trait]
impl FileStore for SqliteFileStore {
    async fn put(&self, bytes: &[u8]) -> Result<String, FileStoreError> {
        let id = content_id(bytes);
        let row_id = id.clone();
        let size = bytes.len() as i64;
        let bytes = bytes.to_vec();
        self.with_conn(move |conn| {
            conn.execute(
                &format!(
                    "INSERT INTO {NS}_blob (id, bytes, size) VALUES (?1, ?2, ?3) \
                     ON CONFLICT(id) DO NOTHING"
                ),
                params![row_id, bytes, size],
            )
            .map_err(e)?;
            Ok(())
        })
        .await?;
        Ok(id)
    }

    async fn get(&self, id: &str) -> Result<Option<Vec<u8>>, FileStoreError> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            conn.query_row(
                &format!("SELECT bytes FROM {NS}_blob WHERE id = ?1"),
                params![id],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(e)
        })
        .await
    }

    async fn list(&self) -> Result<Vec<String>, FileStoreError> {
        self.with_conn(move |conn| {
            let mut stmt = conn
                .prepare(&format!("SELECT id FROM {NS}_blob ORDER BY id"))
                .map_err(e)?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(e)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(e)
        })
        .await
    }

    async fn delete(&self, id: &str) -> Result<bool, FileStoreError> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let affected = conn
                .execute(&format!("DELETE FROM {NS}_blob WHERE id = ?1"), params![id])
                .map_err(e)?;
            Ok(affected > 0)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `over` wraps a connection but must NOT migrate: the store is unusable until
    /// the caller opts into the `file_store` scope via `ensure_schema` (or lets a
    /// unified pipeline own the schema). This is the seam that lets the store share
    /// a caller's single database instead of forcing a parallel migration.
    #[tokio::test]
    async fn over_does_not_migrate_but_ensure_schema_does() {
        let store = SqliteFileStore::over(Connection::open_in_memory().unwrap());
        // No schema yet → the blob table is absent, so writes fail.
        assert!(store.put(b"hello").await.is_err());

        // Opting in applies the scoped bundle; now the store works.
        store.ensure_schema().unwrap();
        let id = store.put(b"hello").await.unwrap();
        assert_eq!(store.get(&id).await.unwrap(), Some(b"hello".to_vec()));

        // ensure_schema is idempotent (safe to re-run).
        store.ensure_schema().unwrap();
    }
}
