//! SQLite [`MemoryFs`] over the crate's `memory_store` migration scope.

use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};

use crate::schema::memory_store_bundle;

const NS: &str = "memory_store";

/// Errors from opening or migrating the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("open: {0}")]
    Open(String),
    #[error("migrate: {0}")]
    Migrate(String),
}

/// Apply the `memory_store` scoped migration bundle to `conn` (idempotent).
/// Shared by both SQLite stores, since they live under one scope.
fn migrate_conn(conn: &Connection) -> Result<(), StoreError> {
    let bundle = memory_store_bundle().map_err(|e| StoreError::Migrate(e.to_string()))?;
    awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
        .map_err(|e| StoreError::Migrate(e.to_string()))?
        .run_bundle(conn, &bundle)
        .map_err(|e| StoreError::Migrate(e.to_string()))?;
    Ok(())
}

fn migrate_guarded(conn: &Arc<Mutex<Connection>>) -> Result<(), StoreError> {
    let guard = conn
        .lock()
        .map_err(|_| StoreError::Migrate("memory_store connection poisoned".into()))?;
    migrate_conn(&guard)
}

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
    /// Open (or create) a database file and apply the memory-store migrations
    /// (one-step convenience for a store-owned database).
    pub fn open(path: &str) -> Result<Self, StoreError> {
        let conn = Connection::open(path).map_err(|e| StoreError::Open(e.to_string()))?;
        let store = Self::over(conn);
        store.ensure_schema()?;
        Ok(store)
    }

    /// Open a private in-memory database (tests / ephemeral).
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory().map_err(|e| StoreError::Open(e.to_string()))?;
        let store = Self::over(conn);
        store.ensure_schema()?;
        Ok(store)
    }

    /// Wrap an existing connection **without migrating**, allowing a unified
    /// migration pipeline to own the shared database.
    pub fn over(conn: Connection) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
        }
    }

    /// Apply the `memory_store` scoped migration bundle (idempotent). Optional.
    pub fn ensure_schema(&self) -> Result<(), StoreError> {
        migrate_guarded(&self.conn)
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
            // Atomic POSIX replace: drop the destination and move the source in ONE
            // transaction, so a crash between the two writes can never leave the
            // destination deleted without the move completing (an uncommitted
            // transaction is rolled back on recovery). `unchecked_transaction` gives a
            // transaction from a shared `&Connection`; it rolls back if dropped before
            // `commit` (any early return here — e.g. a mid-op fault — undoes the DELETE).
            let tx = conn.unchecked_transaction().map_err(mem_err)?;
            let row = tx
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
            tx.execute(
                &format!("DELETE FROM {NS}_memories WHERE store_id = ?1 AND path = ?2"),
                params![store, to],
            )
            .map_err(mem_err)?;
            tx.execute(
                &format!(
                    "UPDATE {NS}_memories SET path = ?1, version = version + 1, updated = ?2 \
                     WHERE store_id = ?3 AND id = ?4"
                ),
                params![to, now, store, id],
            )
            .map_err(mem_err)?;
            tx.commit().map_err(mem_err)?;
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
mod migration_seam_tests {
    use super::*;

    /// `over` wraps a connection without migrating; the store only works once the
    /// caller opts into the `memory_store` scope via `ensure_schema`.
    #[tokio::test]
    async fn over_does_not_migrate_but_ensure_schema_does() {
        let fs = SqliteMemoryFs::over(Connection::open_in_memory().unwrap());
        assert!(fs.create("s", "/a.md", "alpha").await.is_err());
        fs.ensure_schema().unwrap();
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

    /// The same cause-effect-graph edge cases the in-memory/fs backends get in
    /// `memfs::tests::extended_conformance`, run against the SQLite backend so the
    /// CAS/rename **precedence** and prefix-boundary rules are pinned here too.
    #[tokio::test]
    async fn sqlite_memory_fs_extended_conformance() {
        use crate::memfs::MAX_PATH_BYTES;
        let fs = SqliteMemoryFs::open_in_memory().unwrap();
        let store = "ext";

        // G-C1: the remaining validate_path rejections on create.
        let over_cap = format!("/{}", "a".repeat(MAX_PATH_BYTES)); // len == cap + 1
        for bad in ["/", "/ctrl\nseg.md", "/a/./b.md", over_cap.as_str()] {
            assert!(
                matches!(
                    fs.create(store, bad, "x").await,
                    Err(MemErr::InvalidPath(_))
                ),
                "expected InvalidPath for {bad:?}"
            );
        }

        // G-C2: content exactly at the cap is accepted.
        let at_cap = "x".repeat(super::super::MAX_MEMORY_BYTES);
        let m_cap = fs.create(store, "/at-cap.md", &at_cap).await.unwrap();
        assert_eq!(m_cap.content_size, super::super::MAX_MEMORY_BYTES as u64);

        // G-U1: validate_size runs before the id lookup — an oversized update to a
        // nonexistent id reports TooLarge, not NotFound.
        let big = "x".repeat(super::super::MAX_MEMORY_BYTES + 1);
        assert!(matches!(
            fs.update(store, "mem_does_not_exist", &big, "sha").await,
            Err(MemErr::TooLarge)
        ));

        // G-U2: an idempotent update leaves the version unchanged.
        let m = fs.create(store, "/idem.md", "v0").await.unwrap();
        let up = fs
            .update(store, &m.id, "v1", &m.content_sha256)
            .await
            .unwrap();
        assert_eq!(up.version, 2);
        let idem = fs
            .update(store, &m.id, "v1", "stale-base-sha")
            .await
            .unwrap();
        assert_eq!(
            idem.version, 2,
            "idempotent write leaves the version unchanged"
        );

        // G-R1: rename to an invalid path fails before touching the source.
        fs.create(store, "/src.md", "s").await.unwrap();
        assert!(matches!(
            fs.rename(store, "/src.md", "relative").await,
            Err(MemErr::InvalidPath(_))
        ));
        assert!(fs.get_by_path(store, "/src.md").await.unwrap().is_some());

        // G-R2: from == to returns current, no version bump.
        let src = fs.get_by_path(store, "/src.md").await.unwrap().unwrap();
        let same = fs.rename(store, "/src.md", "/src.md").await.unwrap();
        assert_eq!(same.id, src.id);
        assert_eq!(same.version, src.version);

        // G-R3: a pure move preserves the id and bumps the version.
        let moved = fs.rename(store, "/src.md", "/moved.md").await.unwrap();
        assert_eq!(moved.id, src.id);
        assert_eq!(moved.version, src.version + 1);
        assert!(fs.get_by_path(store, "/src.md").await.unwrap().is_none());

        // G-L1/G-L2: prefix boundary + empty prefix.
        let pstore = "prefix";
        fs.create(pstore, "/notes", "a").await.unwrap();
        fs.create(pstore, "/notes/x.md", "b").await.unwrap();
        fs.create(pstore, "/notesbar", "c").await.unwrap();
        let mut under: Vec<_> = fs
            .list(pstore, "/notes")
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.path)
            .collect();
        under.sort();
        assert_eq!(under, vec!["/notes", "/notes/x.md"]);
        assert_eq!(fs.list(pstore, "").await.unwrap().len(), 3);

        // delete on a store that was never created is a no-op Ok.
        fs.delete_by_path("never_created_store", "/x.md")
            .await
            .unwrap();
    }

    /// The same "who masks whom" edge as `memfs::tests`: a rename-replace over SQLite
    /// drops the destination row entirely, so the replaced destination's id is
    /// `NotFound` and the destination path carries the source id + content.
    #[tokio::test]
    async fn sqlite_rename_replace_orphans_destination_id() {
        let fs = SqliteMemoryFs::open_in_memory().unwrap();
        let dst = fs.create("s", "/dst.md", "old-dst").await.unwrap();
        let src = fs.create("s", "/src.md", "src").await.unwrap();
        assert_ne!(dst.id, src.id);
        let moved = fs.rename("s", "/src.md", "/dst.md").await.unwrap();
        assert_eq!(moved.id, src.id);
        assert!(matches!(
            fs.update("s", &dst.id, "zombie", &dst.content_sha256).await,
            Err(MemErr::NotFound(_))
        ));
        let at_dst = fs.get_by_path("s", "/dst.md").await.unwrap().unwrap();
        assert_eq!(at_dst.id, src.id);
        assert_eq!(at_dst.content.as_deref(), Some("src"));
    }

    /// KNOWN DIVERGENCE / TRIPWIRE (see the CEG report): the SQLite (and Postgres)
    /// backend mints the next id from `MAX(ordinal)` over the *live* rows, so deleting
    /// the highest-ordinal memory and creating again **reuses the deleted id**
    /// (`mem_2`), whereas the in-memory backend advances a monotonic counter and mints
    /// a fresh `mem_3`. The contract calls the id "globally-unique", and every other
    /// backend honors monotonicity in-process, so this reuse is a bug: an in-flight
    /// reference (e.g. a cached FUSE inode→id) to the deleted memory would silently
    /// re-resolve to the new one. This test pins the current behavior so a durable-
    /// counter fix (see report) flips it deliberately — update `mem_2` → `mem_3` then.
    #[tokio::test]
    async fn id_reuse_after_delete_diverges_from_monotonic_backends() {
        use crate::memfs::InMemoryFs;

        async fn top_delete_then_create(fs: &dyn MemoryFs) -> String {
            fs.create("s", "/a.md", "a").await.unwrap();
            fs.create("s", "/b.md", "b").await.unwrap(); // mem_2 (top ordinal)
            fs.delete_by_path("s", "/b.md").await.unwrap();
            fs.create("s", "/c.md", "c").await.unwrap().id
        }

        let monotonic = top_delete_then_create(&InMemoryFs::new()).await;
        assert_eq!(monotonic, "mem_3", "in-memory never reuses a deleted id");

        let sqlite = top_delete_then_create(&SqliteMemoryFs::open_in_memory().unwrap()).await;
        assert_eq!(
            sqlite, "mem_2",
            "BUG(pinned): sqlite reuses the deleted top ordinal instead of minting mem_3"
        );
        assert_ne!(
            monotonic, sqlite,
            "the two backends disagree on the id after a top-ordinal delete"
        );
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

    /// A crash mid-`rename` (after the destination is deleted, before the source is
    /// moved) must lose nothing: the rename wraps both writes in one transaction, so
    /// an un-committed transaction rolls back on recovery. We model the crash by
    /// running the destination DELETE in a transaction that is dropped before commit,
    /// then assert both memories are intact.
    #[tokio::test]
    async fn rename_replace_is_crash_atomic() {
        let fs = SqliteMemoryFs::open_in_memory().unwrap();
        fs.create("s", "/from.md", "src").await.unwrap();
        fs.create("s", "/to.md", "dst").await.unwrap();

        // Simulate a crash: delete the destination inside a transaction, then abandon
        // it (drop without commit) — exactly the interrupted-rename window.
        {
            let guard = fs.conn.lock().unwrap();
            let tx = guard.unchecked_transaction().unwrap();
            tx.execute(
                &format!("DELETE FROM {NS}_memories WHERE store_id = ?1 AND path = ?2"),
                params!["s", "/to.md"],
            )
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
}
