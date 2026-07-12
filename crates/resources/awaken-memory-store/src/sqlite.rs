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

// ---------------------------------------------------------------------------
// Path-addressed MemoryFs backend (ADR-0053)
// ---------------------------------------------------------------------------

use crate::memfs::{now_nanos, under_prefix, validate_path, validate_size};
use crate::{MemErr, Memory, MemoryEntry, MemoryFs, sha256_hex};

fn mem_err(err: impl std::fmt::Display) -> MemErr {
    MemErr::Storage(err.to_string())
}

/// Like [`with_conn`] but the closure carries [`MemErr`] (so it can raise
/// `PathConflict`/`Conflict`/`NotFound`, not only storage faults).
async fn with_conn_mem<T, F>(conn: &Arc<Mutex<Connection>>, f: F) -> Result<T, MemErr>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> Result<T, MemErr> + Send + 'static,
{
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let guard = conn.lock().map_err(|_| mem_err("memory_store poisoned"))?;
        f(&guard)
    })
    .await
    .map_err(mem_err)?
}

/// A SQLite-backed [`MemoryFs`] (path-addressed, CAS). Each mutation holds the
/// connection mutex, so a read-modify-write (compare-and-swap, rename-replace) is
/// atomic within the process; a cross-node CAS is the postgres backend's job.
pub struct SqliteMemoryFs {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteMemoryFs {
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
        version: version as u64,
        created_unix_nanos: created as u128,
        updated_unix_nanos: updated as u128,
    })
}

#[async_trait::async_trait]
impl MemoryFs for SqliteMemoryFs {
    async fn list(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr> {
        let (store, prefix) = (store.to_string(), prefix.to_string());
        with_conn_mem(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT id, path, sha, length(content), version, updated \
                     FROM {NS}_memories WHERE store_id = ?1"
                ))
                .map_err(mem_err)?;
            let rows = stmt
                .query_map(params![store], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, i64>(5)?,
                    ))
                })
                .map_err(mem_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(mem_err)?;
            Ok(rows
                .into_iter()
                .filter(|(_, path, ..)| under_prefix(path, &prefix))
                .map(|(id, path, sha, size, version, updated)| MemoryEntry {
                    id,
                    path,
                    content_sha256: sha,
                    content_size: size as u64,
                    version: version as u64,
                    updated_unix_nanos: updated as u128,
                })
                .collect())
        })
        .await
    }

    async fn get_by_path(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr> {
        let (store, path) = (store.to_string(), path.to_string());
        with_conn_mem(&self.conn, move |conn| {
            let row = conn
                .query_row(
                    &format!(
                        "SELECT id, path, content, sha, version, created, updated \
                         FROM {NS}_memories WHERE store_id = ?1 AND path = ?2"
                    ),
                    params![store, path],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Vec<u8>>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, i64>(4)?,
                            r.get::<_, i64>(5)?,
                            r.get::<_, i64>(6)?,
                        ))
                    },
                )
                .optional()
                .map_err(mem_err)?;
            match row {
                Some((id, path, content, sha, version, created, updated)) => Ok(Some(row_memory(
                    id, path, content, sha, version, created, updated,
                )?)),
                None => Ok(None),
            }
        })
        .await
    }

    async fn create(&self, store: &str, path: &str, content: &str) -> Result<Memory, MemErr> {
        validate_path(path)?;
        validate_size(content)?;
        let (store, path, content) = (store.to_string(), path.to_string(), content.to_string());
        with_conn_mem(&self.conn, move |conn| {
            let exists = conn
                .query_row(
                    &format!("SELECT 1 FROM {NS}_memories WHERE store_id = ?1 AND path = ?2"),
                    params![store, path],
                    |_| Ok(()),
                )
                .optional()
                .map_err(mem_err)?
                .is_some();
            if exists {
                return Err(MemErr::PathConflict(path));
            }
            let ordinal: i64 = conn
                .query_row(
                    &format!("SELECT COALESCE(MAX(ordinal), 0) + 1 FROM {NS}_memories"),
                    [],
                    |r| r.get(0),
                )
                .map_err(mem_err)?;
            let id = format!("mem_{ordinal}");
            let sha = sha256_hex(&content);
            let now = now_nanos() as i64;
            conn.execute(
                &format!(
                    "INSERT INTO {NS}_memories \
                     (store_id, path, id, ordinal, content, sha, version, created, updated) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?7)"
                ),
                params![store, path, id, ordinal, content.as_bytes(), sha, now],
            )
            .map_err(mem_err)?;
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

    async fn update(
        &self,
        store: &str,
        id: &str,
        content: &str,
        base_sha: &str,
    ) -> Result<Memory, MemErr> {
        validate_size(content)?;
        let (store, id, content, base_sha) = (
            store.to_string(),
            id.to_string(),
            content.to_string(),
            base_sha.to_string(),
        );
        with_conn_mem(&self.conn, move |conn| {
            let row = conn
                .query_row(
                    &format!(
                        "SELECT path, sha, content, version, created FROM {NS}_memories \
                         WHERE store_id = ?1 AND id = ?2"
                    ),
                    params![store, id],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Vec<u8>>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, i64>(4)?,
                        ))
                    },
                )
                .optional()
                .map_err(mem_err)?;
            let (path, cur_sha, cur_content, version, created) =
                row.ok_or_else(|| MemErr::NotFound(id.clone()))?;
            let new_sha = sha256_hex(&content);
            if cur_sha != base_sha {
                if cur_sha == new_sha {
                    return row_memory(id, path, cur_content, cur_sha, version, created, created);
                }
                let current =
                    row_memory(id, path, cur_content, cur_sha, version, created, created)?;
                return Err(MemErr::Conflict {
                    current: Box::new(current),
                });
            }
            let now = now_nanos() as i64;
            conn.execute(
                &format!(
                    "UPDATE {NS}_memories SET content = ?1, sha = ?2, version = version + 1, \
                     updated = ?3 WHERE store_id = ?4 AND id = ?5"
                ),
                params![content.as_bytes(), new_sha, now, store, id],
            )
            .map_err(mem_err)?;
            Ok(Memory {
                content_size: content.len() as u64,
                content: Some(content),
                id,
                path,
                content_sha256: new_sha,
                version: (version + 1) as u64,
                created_unix_nanos: created as u128,
                updated_unix_nanos: now as u128,
            })
        })
        .await
    }

    async fn rename(&self, store: &str, from: &str, to: &str) -> Result<Memory, MemErr> {
        validate_path(to)?;
        let (store, from, to) = (store.to_string(), from.to_string(), to.to_string());
        with_conn_mem(&self.conn, move |conn| {
            let row = conn
                .query_row(
                    &format!(
                        "SELECT id, content, sha, version, created FROM {NS}_memories \
                         WHERE store_id = ?1 AND path = ?2"
                    ),
                    params![store, from],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, Vec<u8>>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, i64>(4)?,
                        ))
                    },
                )
                .optional()
                .map_err(mem_err)?;
            let (id, content, sha, version, created) =
                row.ok_or_else(|| MemErr::NotFound(from.clone()))?;
            if from == to {
                return row_memory(id, from, content, sha, version, created, created);
            }
            let now = now_nanos() as i64;
            conn.execute(
                &format!("DELETE FROM {NS}_memories WHERE store_id = ?1 AND path = ?2"),
                params![store, to],
            )
            .map_err(mem_err)?;
            conn.execute(
                &format!(
                    "UPDATE {NS}_memories SET path = ?1, version = version + 1, updated = ?2 \
                     WHERE store_id = ?3 AND id = ?4"
                ),
                params![to, now, store, id],
            )
            .map_err(mem_err)?;
            Ok(Memory {
                content_size: content.len() as u64,
                content: Some(String::from_utf8(content).map_err(mem_err)?),
                id,
                path: to,
                content_sha256: sha,
                version: (version + 1) as u64,
                created_unix_nanos: created as u128,
                updated_unix_nanos: now as u128,
            })
        })
        .await
    }

    async fn delete_by_path(&self, store: &str, path: &str) -> Result<(), MemErr> {
        let (store, path) = (store.to_string(), path.to_string());
        with_conn_mem(&self.conn, move |conn| {
            conn.execute(
                &format!("DELETE FROM {NS}_memories WHERE store_id = ?1 AND path = ?2"),
                params![store, path],
            )
            .map_err(mem_err)?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod memfs_tests {
    use super::*;

    #[tokio::test]
    async fn sqlite_memory_fs_conforms() {
        let fs = SqliteMemoryFs::open_in_memory().unwrap();
        let store = "memstore_1";

        // create → version 1, sha, content.
        let m = fs.create(store, "/notes/today.md", "alpha").await.unwrap();
        assert_eq!(m.version, 1);
        assert_eq!(m.content_sha256, sha256_hex("alpha"));
        assert_eq!(m.content.as_deref(), Some("alpha"));
        assert!(m.created_unix_nanos > 0);

        // duplicate path → PathConflict.
        assert!(matches!(
            fs.create(store, "/notes/today.md", "x").await,
            Err(MemErr::PathConflict(_))
        ));

        // get_by_path.
        assert_eq!(
            fs.get_by_path(store, "/notes/today.md")
                .await
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("alpha")
        );
        assert!(fs.get_by_path(store, "/nope.md").await.unwrap().is_none());

        // update CAS: right base → bump; stale base → Conflict, no clobber.
        let up = fs
            .update(store, &m.id, "beta", &m.content_sha256)
            .await
            .unwrap();
        assert_eq!(up.version, 2);
        assert_eq!(up.content.as_deref(), Some("beta"));
        match fs.update(store, &m.id, "gamma", &m.content_sha256).await {
            Err(MemErr::Conflict { current }) => {
                assert_eq!(current.content.as_deref(), Some("beta"))
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
        assert_eq!(
            fs.get_by_path(store, "/notes/today.md")
                .await
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("beta")
        );
        // idempotent update (stale base but content already current).
        assert_eq!(
            fs.update(store, &m.id, "beta", "deadbeef")
                .await
                .unwrap()
                .content
                .as_deref(),
            Some("beta")
        );

        // list + prefix.
        fs.create(store, "/notes/other.md", "o").await.unwrap();
        fs.create(store, "/root.md", "r").await.unwrap();
        let mut notes: Vec<_> = fs
            .list(store, "/notes")
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.path)
            .collect();
        notes.sort();
        assert_eq!(notes, vec!["/notes/other.md", "/notes/today.md"]);
        assert_eq!(fs.list(store, "/").await.unwrap().len(), 3);

        // rename-replace: keeps the id, replaces the destination atomically.
        let before = fs
            .get_by_path(store, "/notes/today.md")
            .await
            .unwrap()
            .unwrap();
        let moved = fs
            .rename(store, "/notes/today.md", "/notes/other.md")
            .await
            .unwrap();
        assert_eq!(moved.id, before.id);
        assert_eq!(moved.content.as_deref(), Some("beta"));
        assert!(
            fs.get_by_path(store, "/notes/today.md")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            fs.get_by_path(store, "/notes/other.md")
                .await
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("beta")
        );

        // delete (idempotent) + path/size validation.
        fs.delete_by_path(store, "/notes/other.md").await.unwrap();
        assert!(
            fs.get_by_path(store, "/notes/other.md")
                .await
                .unwrap()
                .is_none()
        );
        fs.delete_by_path(store, "/notes/other.md").await.unwrap();
        assert!(matches!(
            fs.create(store, "rel.md", "x").await,
            Err(MemErr::InvalidPath(_))
        ));
        assert!(matches!(
            fs.create(
                store,
                "/big.md",
                &"x".repeat(super::super::MAX_MEMORY_BYTES + 1)
            )
            .await,
            Err(MemErr::TooLarge)
        ));
    }

    #[tokio::test]
    async fn sqlite_memory_fs_survives_reopen() {
        let dir = std::env::temp_dir().join(format!("awaken-sqlmemfs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("m.db");
        let path_str = path.to_str().unwrap();
        {
            let fs = SqliteMemoryFs::open(path_str).unwrap();
            fs.create("s", "/keep.md", "durable").await.unwrap();
        }
        let fs = SqliteMemoryFs::open(path_str).unwrap();
        assert_eq!(
            fs.get_by_path("s", "/keep.md")
                .await
                .unwrap()
                .unwrap()
                .content
                .as_deref(),
            Some("durable")
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
