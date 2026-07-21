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
use crate::{
    MemErr, Memory, MemoryEntry, MemoryFs, MemoryVersion, MemoryVersionOperation, sha256_hex,
};

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

    /// One-time, idempotent upgrade from the removed runtime-host
    /// `resource-api.db::memory_versions` sidecar. The imported rows become normal
    /// aggregate history; no runtime read or write ever returns to the legacy DB.
    pub fn import_legacy_versions(
        &self,
        legacy_path: &std::path::Path,
    ) -> Result<usize, StoreError> {
        if !legacy_path.exists() {
            return Ok(0);
        }
        let legacy =
            Connection::open(legacy_path).map_err(|error| StoreError::Open(error.to_string()))?;
        let mut statement = match legacy.prepare(
            "SELECT seq, store_id, version_id, data FROM memory_versions \
             WHERE version_id IS NOT NULL AND data <> '' ORDER BY seq",
        ) {
            Ok(statement) => statement,
            Err(_) => return Ok(0),
        };
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .map_err(|error| StoreError::Migrate(error.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| StoreError::Migrate(error.to_string()))?;

        let guard = self
            .conn
            .lock()
            .map_err(|_| StoreError::Migrate("memory_store connection poisoned".into()))?;
        let tx = guard
            .unchecked_transaction()
            .map_err(|error| StoreError::Migrate(error.to_string()))?;
        let mut imported = 0usize;
        let mut max_ordinal = 0i64;
        for (ordinal, store, version_id, data) in rows {
            let value: serde_json::Value = serde_json::from_str(&data)
                .map_err(|error| StoreError::Migrate(error.to_string()))?;
            let memory_id = value
                .get("memory_id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    StoreError::Migrate("legacy memory version has no memory_id".into())
                })?;
            let operation = value
                .get("operation")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    StoreError::Migrate("legacy memory version has no operation".into())
                })?;
            let path = value
                .get("path")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| StoreError::Migrate("legacy memory version has no path".into()))?;
            let content = value.get("content").and_then(serde_json::Value::as_str);
            let redacted = value
                .get("redacted_at")
                .is_some_and(|value| !value.is_null())
                .then_some(1i64);
            imported += tx
                .execute(
                    &format!(
                        "INSERT OR IGNORE INTO {NS}_versions \
                         (store_id, ordinal, id, memory_id, operation, path, content, created, redacted) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8)"
                    ),
                    params![
                        store,
                        ordinal,
                        version_id,
                        memory_id,
                        operation,
                        path,
                        content.map(str::as_bytes),
                        redacted,
                    ],
                )
                .map_err(|error| StoreError::Migrate(error.to_string()))?;
            max_ordinal = max_ordinal.max(ordinal);
        }
        if max_ordinal > 0 {
            tx.execute(
                &format!(
                    "INSERT INTO {NS}_counters(name, next_value) VALUES ('memory_version', ?1) \
                     ON CONFLICT(name) DO UPDATE SET next_value = MAX(next_value, excluded.next_value)"
                ),
                params![max_ordinal],
            )
            .map_err(|error| StoreError::Migrate(error.to_string()))?;
        }
        tx.commit()
            .map_err(|error| StoreError::Migrate(error.to_string()))?;
        Ok(imported)
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

fn row_version(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryVersion> {
    let operation: String = row.get(2)?;
    let content: Option<Vec<u8>> = row.get(4)?;
    let content = content
        .map(String::from_utf8)
        .transpose()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Blob,
                Box::new(error),
            )
        })?;
    Ok(MemoryVersion {
        id: row.get(0)?,
        memory_id: row.get(1)?,
        operation: parse_operation(&operation).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        path: row.get(3)?,
        content,
        created_unix_nanos: row.get::<_, i64>(5)? as u128,
        redacted_unix_nanos: row.get::<_, Option<i64>>(6)?.map(|value| value as u128),
    })
}

fn next_counter(tx: &rusqlite::Transaction<'_>, name: &str, seed_sql: &str) -> Result<i64, MemErr> {
    let seed: i64 = tx
        .query_row(seed_sql, [], |row| row.get(0))
        .map_err(mem_err)?;
    tx.execute(
        &format!("INSERT OR IGNORE INTO {NS}_counters(name, next_value) VALUES (?1, ?2)"),
        params![name, seed],
    )
    .map_err(mem_err)?;
    tx.execute(
        &format!("UPDATE {NS}_counters SET next_value = next_value + 1 WHERE name = ?1"),
        params![name],
    )
    .map_err(mem_err)?;
    tx.query_row(
        &format!("SELECT next_value FROM {NS}_counters WHERE name = ?1"),
        params![name],
        |row| row.get(0),
    )
    .map_err(mem_err)
}

fn append_version(
    tx: &rusqlite::Transaction<'_>,
    store: &str,
    memory_id: &str,
    operation: MemoryVersionOperation,
    path: &str,
    content: Option<&str>,
    created: i64,
) -> Result<MemoryVersion, MemErr> {
    let ordinal = next_counter(
        tx,
        "memory_version",
        &format!("SELECT COALESCE(MAX(ordinal), 0) FROM {NS}_versions"),
    )?;
    let version = MemoryVersion {
        id: format!("memver_{ordinal:016}"),
        memory_id: memory_id.to_string(),
        operation,
        path: path.to_string(),
        content: content.map(str::to_string),
        created_unix_nanos: created as u128,
        redacted_unix_nanos: None,
    };
    tx.execute(
        &format!(
            "INSERT INTO {NS}_versions \
             (store_id, ordinal, id, memory_id, operation, path, content, created, redacted) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL)"
        ),
        params![
            store,
            ordinal,
            version.id,
            memory_id,
            operation_name(operation),
            path,
            content.map(str::as_bytes),
            created,
        ],
    )
    .map_err(mem_err)?;
    Ok(version)
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
            let tx = conn.unchecked_transaction().map_err(mem_err)?;
            let exists = tx
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
            let ordinal = next_counter(
                &tx,
                "memory_id",
                &format!("SELECT COALESCE(MAX(ordinal), 0) FROM {NS}_memories"),
            )?;
            let id = format!("mem_{ordinal}");
            let sha = sha256_hex(&content);
            let now = now_nanos() as i64;
            tx.execute(
                &format!(
                    "INSERT INTO {NS}_memories \
                     (store_id, path, id, ordinal, content, sha, version, created, updated) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?7)"
                ),
                params![store, path, id, ordinal, content.as_bytes(), sha, now],
            )
            .map_err(mem_err)?;
            append_version(
                &tx,
                &store,
                &id,
                MemoryVersionOperation::Created,
                &path,
                Some(&content),
                now,
            )?;
            tx.commit().map_err(mem_err)?;
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
            let tx = conn.unchecked_transaction().map_err(mem_err)?;
            let row = tx
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
            tx.execute(
                &format!(
                    "UPDATE {NS}_memories SET content = ?1, sha = ?2, version = version + 1, \
                     updated = ?3 WHERE store_id = ?4 AND id = ?5"
                ),
                params![content.as_bytes(), new_sha, now, store, id],
            )
            .map_err(mem_err)?;
            append_version(
                &tx,
                &store,
                &id,
                MemoryVersionOperation::Modified,
                &path,
                Some(&content),
                now,
            )?;
            tx.commit().map_err(mem_err)?;
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
            let displaced_id = tx
                .query_row(
                    &format!("SELECT id FROM {NS}_memories WHERE store_id = ?1 AND path = ?2"),
                    params![store, to],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(mem_err)?;
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
            if let Some(displaced_id) = displaced_id {
                append_version(
                    &tx,
                    &store,
                    &displaced_id,
                    MemoryVersionOperation::Deleted,
                    &to,
                    None,
                    now,
                )?;
            }
            let content = String::from_utf8(content).map_err(mem_err)?;
            append_version(
                &tx,
                &store,
                &id,
                MemoryVersionOperation::Modified,
                &to,
                Some(&content),
                now,
            )?;
            tx.commit().map_err(mem_err)?;
            Ok(Memory {
                content_size: content.len() as u64,
                content: Some(content),
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
            let tx = conn.unchecked_transaction().map_err(mem_err)?;
            let memory_id = tx
                .query_row(
                    &format!("SELECT id FROM {NS}_memories WHERE store_id = ?1 AND path = ?2"),
                    params![store, path],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(mem_err)?;
            let Some(memory_id) = memory_id else {
                return Ok(());
            };
            tx.execute(
                &format!("DELETE FROM {NS}_memories WHERE store_id = ?1 AND path = ?2"),
                params![store, path],
            )
            .map_err(mem_err)?;
            append_version(
                &tx,
                &store,
                &memory_id,
                MemoryVersionOperation::Deleted,
                &path,
                None,
                now_nanos() as i64,
            )?;
            tx.commit().map_err(mem_err)?;
            Ok(())
        })
        .await
    }

    async fn list_versions(&self, store: &str) -> Result<Vec<MemoryVersion>, MemErr> {
        let store = store.to_string();
        with_conn_mem(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT id, memory_id, operation, path, content, created, redacted \
                     FROM {NS}_versions WHERE store_id = ?1 ORDER BY ordinal"
                ))
                .map_err(mem_err)?;
            stmt.query_map(params![store], row_version)
                .map_err(mem_err)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(mem_err)
        })
        .await
    }

    async fn redact_version(
        &self,
        store: &str,
        version_id: &str,
    ) -> Result<Option<MemoryVersion>, MemErr> {
        let (store, version_id) = (store.to_string(), version_id.to_string());
        with_conn_mem(&self.conn, move |conn| {
            let tx = conn.unchecked_transaction().map_err(mem_err)?;
            let existing = tx
                .query_row(
                    &format!(
                        "SELECT id, memory_id, operation, path, content, created, redacted \
                         FROM {NS}_versions WHERE store_id = ?1 AND id = ?2"
                    ),
                    params![store, version_id],
                    row_version,
                )
                .optional()
                .map_err(mem_err)?;
            let Some(mut version) = existing else {
                return Ok(None);
            };
            if version.redacted_unix_nanos.is_none() {
                let redacted = now_nanos() as i64;
                tx.execute(
                    &format!(
                        "UPDATE {NS}_versions SET content = NULL, redacted = ?1 \
                         WHERE store_id = ?2 AND id = ?3"
                    ),
                    params![redacted, store, version_id],
                )
                .map_err(mem_err)?;
                version.content = None;
                version.redacted_unix_nanos = Some(redacted as u128);
            }
            tx.commit().map_err(mem_err)?;
            Ok(Some(version))
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

    /// Durable high-water counters prevent a deleted memory id from ever being
    /// reissued. This must agree with the in-memory aggregate.
    #[tokio::test]
    async fn durable_ids_remain_monotonic_after_delete() {
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
        assert_eq!(sqlite, "mem_3", "sqlite never reuses a deleted id");
        assert_eq!(monotonic, sqlite);
    }

    #[tokio::test]
    async fn sqlite_memory_fs_survives_reopen() {
        let dir = std::env::temp_dir().join(format!("awaken-sqlmemfs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("m.db");
        let path_str = path.to_str().unwrap();
        let version_id;
        {
            let fs = SqliteMemoryFs::open(path_str).unwrap();
            let created = fs.create("s", "/keep.md", "durable").await.unwrap();
            fs.update("s", &created.id, "updated", &created.content_sha256)
                .await
                .unwrap();
            version_id = fs.list_versions("s").await.unwrap()[0].id.clone();
            fs.redact_version("s", &version_id).await.unwrap().unwrap();
        }
        let fs = SqliteMemoryFs::open(path_str).unwrap();
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
        assert_eq!(versions[1].content.as_deref(), Some("updated"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn legacy_version_sidecar_import_is_idempotent_and_advances_high_water() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_path = dir.path().join("resource-api.db");
        let legacy = Connection::open(&legacy_path).unwrap();
        legacy
            .execute_batch(
                "CREATE TABLE memory_versions (\
                     seq INTEGER PRIMARY KEY AUTOINCREMENT,\
                     store_id TEXT NOT NULL,\
                     version_id TEXT UNIQUE,\
                     data TEXT NOT NULL\
                 );",
            )
            .unwrap();
        legacy
            .execute(
                "INSERT INTO memory_versions(seq, store_id, version_id, data) \
                 VALUES (42, 's', 'memver_0000000000000042', ?1)",
                params![
                    serde_json::json!({
                        "id": "memver_0000000000000042",
                        "memory_id": "mem_old",
                        "operation": "created",
                        "content": "legacy",
                        "path": "/legacy.md",
                        "redacted_at": null
                    })
                    .to_string()
                ],
            )
            .unwrap();
        drop(legacy);

        let store = SqliteMemoryFs::open(dir.path().join("memory.db").to_str().unwrap()).unwrap();
        assert_eq!(store.import_legacy_versions(&legacy_path).unwrap(), 1);
        assert_eq!(store.import_legacy_versions(&legacy_path).unwrap(), 0);
        let versions = store.list_versions("s").await.unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].content.as_deref(), Some("legacy"));

        store.create("s", "/new.md", "new").await.unwrap();
        let versions = store.list_versions("s").await.unwrap();
        assert_eq!(versions[1].id, "memver_0000000000000043");
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
