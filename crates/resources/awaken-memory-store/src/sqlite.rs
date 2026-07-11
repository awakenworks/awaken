//! SQLite [`MemoryBlobStore`] over the crate's `memory_store` migration scope.

use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};

use crate::schema::memory_store_bundle;
use crate::{MemoryBlobStore, MemoryStoreError, sanitize_stem};

const NS: &str = "memory_store";

/// Errors from opening or migrating the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("open: {0}")]
    Open(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

fn open_migrated(conn: Connection) -> Result<Arc<Mutex<Connection>>, StoreError> {
    let bundle = memory_store_bundle().map_err(|e| StoreError::Migrate(e.to_string()))?;
    awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
        .map_err(|e| StoreError::Migrate(e.to_string()))?
        .run_bundle(&conn, &bundle)
        .map_err(|e| StoreError::Migrate(e.to_string()))?;
    Ok(Arc::new(Mutex::new(conn)))
}

fn storage(err: impl std::fmt::Display) -> MemoryStoreError {
    MemoryStoreError::Storage(err.to_string())
}

async fn with_conn<T, F>(conn: &Arc<Mutex<Connection>>, f: F) -> Result<T, MemoryStoreError>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> Result<T, MemoryStoreError> + Send + 'static,
{
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let guard = conn.lock().map_err(|_| storage("memory_store poisoned"))?;
        f(&guard)
    })
    .await
    .map_err(storage)?
}

/// A SQLite-backed [`MemoryBlobStore`].
pub struct SqliteMemoryBlobStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteMemoryBlobStore {
    /// Open (or create) a database file and apply the memory-store migrations.
    pub fn open(path: &str) -> Result<Self, StoreError> {
        let conn = Connection::open(path).map_err(|e| StoreError::Open(e.to_string()))?;
        Ok(Self {
            conn: open_migrated(conn)?,
        })
    }

    /// Open a private in-memory database (tests / ephemeral).
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory().map_err(|e| StoreError::Open(e.to_string()))?;
        Ok(Self {
            conn: open_migrated(conn)?,
        })
    }
}

#[async_trait::async_trait]
impl MemoryBlobStore for SqliteMemoryBlobStore {
    async fn create(&self, workspace_id: &str) -> Result<String, MemoryStoreError> {
        let ws = workspace_id.to_string();
        with_conn(&self.conn, move |conn| {
            // Dense global id: max ordinal + 1, inserted empty, in one statement so a
            // concurrent create cannot mint the same id (the Mutex serializes anyway).
            conn.query_row(
                &format!(
                    "INSERT INTO {NS}_blob (workspace_id, id, ordinal, content) \
                     SELECT ?1, 'memstore_' || x.n, x.n, x'' \
                     FROM (SELECT COALESCE(MAX(ordinal), 0) + 1 AS n FROM {NS}_blob) x \
                     RETURNING id"
                ),
                params![ws],
                |r| r.get::<_, String>(0),
            )
            .map_err(storage)
        })
        .await
    }

    async fn put(
        &self,
        workspace_id: &str,
        id: &str,
        bytes: &[u8],
    ) -> Result<(), MemoryStoreError> {
        let ws = workspace_id.to_string();
        let stem = sanitize_stem(id);
        let bytes = bytes.to_vec();
        with_conn(&self.conn, move |conn| {
            conn.execute(
                &format!(
                    "INSERT INTO {NS}_blob (workspace_id, id, ordinal, content) VALUES (?1, ?2, 0, ?3) \
                     ON CONFLICT(workspace_id, id) DO UPDATE SET content = excluded.content"
                ),
                params![ws, stem, bytes],
            )
            .map_err(storage)?;
            Ok(())
        })
        .await
    }

    async fn get(&self, workspace_id: &str, id: &str) -> Result<Option<Vec<u8>>, MemoryStoreError> {
        let ws = workspace_id.to_string();
        let stem = sanitize_stem(id);
        with_conn(&self.conn, move |conn| {
            conn.query_row(
                &format!("SELECT content FROM {NS}_blob WHERE workspace_id = ?1 AND id = ?2"),
                params![ws, stem],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(storage)
        })
        .await
    }

    async fn exists(&self, workspace_id: &str, id: &str) -> Result<bool, MemoryStoreError> {
        Ok(self.get(workspace_id, id).await?.is_some())
    }
}
