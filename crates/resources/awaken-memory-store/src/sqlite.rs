//! SQLite [`MemoryRepository`] over the crate's `memory_store` migration scope.

use awaken_sql_storage::StoredU64;
use rusqlite::{Connection, OptionalExtension, params};

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

/// Apply the `memory_store` scoped migration bundle to `conn` (idempotent).
/// Shared by both SQLite stores, since they live under one scope.
fn migrate_conn(conn: &Connection) -> Result<(), StoreError> {
    let ledger_exists = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [format!("{NS}_schema_migrations")],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
    let v1_checksum = if ledger_exists {
        conn.query_row(
            &format!(
                "SELECT checksum FROM {NS}_schema_migrations WHERE bundle_id = ?1 AND version = 1"
            ),
            [BUNDLE_ID],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| StoreError::Migrate(error.to_string()))?
    } else {
        None
    };
    let published = selected_memory_store_bundle(v1_checksum.as_deref())
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
    let converged =
        converged_memory_store_bundle().map_err(|error| StoreError::Migrate(error.to_string()))?;
    let runner = awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS)
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
    runner
        .run_bundle(conn, &published)
        .and_then(|_| runner.run_bundle(conn, &converged))
        .map_err(|error| StoreError::Migrate(error.to_string()))?;
    Ok(())
}

fn migrate_guarded(conn: &awaken_sqlite_pool::SharedSqliteConnection) -> Result<(), StoreError> {
    let guard = conn
        .lock()
        .map_err(|_| StoreError::Migrate("memory_store connection poisoned".into()))?;
    migrate_conn(&guard)
}

use crate::repository::{now_nanos, under_prefix, validate_path, validate_size};
use crate::{
    MemErr, Memory, MemoryActor, MemoryEntry, MemoryPurgeSummary, MemoryRepository, MemoryVersion,
    MemoryVersionOperation, VersionAppend, sha256_hex,
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

/// Like [`with_conn`] but the closure carries [`MemErr`] (so it can raise
/// `PathConflict`/`Conflict`/`NotFound`, not only storage faults).
async fn with_conn_mem<T, F>(
    conn: &awaken_sqlite_pool::SharedSqliteConnection,
    f: F,
) -> Result<T, MemErr>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> Result<T, MemErr> + Send + 'static,
{
    awaken_sqlite_pool::with_connection(conn.clone(), move |connection| f(connection))
        .await
        .map_err(mem_err)?
}

/// A SQLite-backed [`MemoryRepository`] (path-addressed, CAS). Each mutation holds the
/// connection mutex, so a read-modify-write (compare-and-swap, rename-replace) is
/// atomic within the process; a cross-node CAS is the postgres backend's job.
pub struct SqliteMemoryRepository {
    conn: awaken_sqlite_pool::SharedSqliteConnection,
}

impl SqliteMemoryRepository {
    /// Open (or create) a database file and apply the memory-store migrations
    /// (one-step convenience for a store-owned database).
    pub fn open(path: &str) -> Result<Self, StoreError> {
        let conn = awaken_sqlite_pool::SqliteConnectionFactory::file(path)
            .open()
            .map_err(|e| StoreError::Open(e.to_string()))?;
        let store = Self::over(conn);
        store.ensure_schema()?;
        Ok(store)
    }

    /// Open a private in-memory database for tests and scenario fixtures.
    #[cfg(any(test, feature = "test-support"))]
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = awaken_sqlite_pool::SqliteConnectionFactory::memory()
            .open()
            .map_err(|e| StoreError::Open(e.to_string()))?;
        let store = Self::over(conn);
        store.ensure_schema()?;
        Ok(store)
    }

    /// Wrap an existing connection **without migrating**, allowing a unified
    /// migration pipeline to own the shared database.
    pub fn over(conn: Connection) -> Self {
        Self {
            conn: awaken_sqlite_pool::SharedSqliteConnection::new(conn),
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
    let actor = |index| -> rusqlite::Result<Option<MemoryActor>> {
        row.get::<_, Option<String>>(index)?
            .map(|json| {
                serde_json::from_str(&json).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        index,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })
            })
            .transpose()
    };
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
        created_by: actor(7)?,
        redacted_unix_nanos: row.get::<_, Option<i64>>(6)?.map(|value| value as u128),
        redacted_by: actor(8)?,
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
    append: VersionAppend<'_>,
) -> Result<MemoryVersion, MemErr> {
    let ordinal = next_counter(
        tx,
        "memory_version",
        &format!("SELECT COALESCE(MAX(ordinal), 0) FROM {NS}_versions"),
    )?;
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
    tx.execute(
        &format!(
            "INSERT INTO {NS}_versions \
             (store_id, ordinal, id, memory_id, operation, path, content, created, redacted, created_by_json, redacted_by_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, NULL)"
        ),
        params![
            append.store,
            ordinal,
            version.id,
            append.memory_id,
            operation_name(append.operation),
            append.path,
            append.content.map(str::as_bytes),
            append.created,
            append
                .actor
                .map(serde_json::to_string)
                .transpose()
                .map_err(mem_err)?,
        ],
    )
    .map_err(mem_err)?;
    Ok(version)
}

#[async_trait::async_trait]
impl MemoryRepository for SqliteMemoryRepository {
    async fn snapshot_heads(&self, store: &str) -> Result<Vec<Memory>, MemErr> {
        let store = store.to_string();
        with_conn_mem(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT id, path, content, sha, version, created, updated \
                     FROM {NS}_memories WHERE store_id = ?1 ORDER BY path"
                ))
                .map_err(mem_err)?;
            stmt.query_map(params![store], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })
            .map_err(mem_err)?
            .map(|row| {
                let (id, path, content, sha, version, created, updated) = row.map_err(mem_err)?;
                row_memory(id, path, content, sha, version, created, updated)
            })
            .collect()
        })
        .await
    }

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
            rows.into_iter()
                .filter(|(_, path, ..)| under_prefix(path, &prefix))
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
            let live_count: i64 = tx
                .query_row(
                    &format!("SELECT COUNT(*) FROM {NS}_memories WHERE store_id = ?1"),
                    params![store],
                    |row| row.get(0),
                )
                .map_err(mem_err)?;
            if live_count >= crate::MAX_MEMORIES_PER_STORE as i64 {
                return Err(MemErr::AtCapacity);
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
                VersionAppend {
                    store: &store,
                    memory_id: &id,
                    operation: MemoryVersionOperation::Created,
                    path: &path,
                    content: Some(&content),
                    created: now,
                    actor: actor.as_ref(),
                },
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

    async fn update_head(
        &self,
        store: &str,
        id: &str,
        content: &str,
        base_sha: &str,
        target_path: Option<&str>,
    ) -> Result<Memory, MemErr> {
        self.update_head_as(store, id, content, base_sha, target_path, None)
            .await
    }

    async fn update_head_as(
        &self,
        store: &str,
        id: &str,
        content: &str,
        base_sha: &str,
        target_path: Option<&str>,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr> {
        self.update_head_checked(store, id, content, base_sha, target_path, actor, None)
            .await
    }

    async fn update_materialized(
        &self,
        store: &str,
        path: &str,
        id: &str,
        content: &str,
        base_sha: &str,
    ) -> Result<Memory, MemErr> {
        self.update_head_checked(store, id, content, base_sha, None, None, Some(path))
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
                    VersionAppend {
                        store: &store,
                        memory_id: &displaced_id,
                        operation: MemoryVersionOperation::Deleted,
                        path: &to,
                        content: None,
                        created: now,
                        actor: None,
                    },
                )?;
            }
            let content = String::from_utf8(content).map_err(mem_err)?;
            append_version(
                &tx,
                VersionAppend {
                    store: &store,
                    memory_id: &id,
                    operation: MemoryVersionOperation::Modified,
                    path: &to,
                    content: Some(&content),
                    created: now,
                    actor: None,
                },
            )?;
            tx.commit().map_err(mem_err)?;
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
                VersionAppend {
                    store: &store,
                    memory_id: &memory_id,
                    operation: MemoryVersionOperation::Deleted,
                    path: &path,
                    content: None,
                    created: now_nanos() as i64,
                    actor: actor.as_ref(),
                },
            )?;
            tx.commit().map_err(mem_err)?;
            Ok(())
        })
        .await
    }

    async fn delete_if_match(
        &self,
        store: &str,
        path: &str,
        base_id: &str,
        base_sha: &str,
    ) -> Result<bool, MemErr> {
        self.delete_if_match_as(store, path, base_id, base_sha, None)
            .await
    }

    async fn delete_if_match_as(
        &self,
        store: &str,
        path: &str,
        base_id: &str,
        base_sha: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<bool, MemErr> {
        let (store, path, base_id, base_sha, actor) = (
            store.to_string(),
            path.to_string(),
            base_id.to_string(),
            base_sha.to_string(),
            actor.cloned(),
        );
        with_conn_mem(&self.conn, move |conn| {
            let tx = conn.unchecked_transaction().map_err(mem_err)?;
            let row = tx
                .query_row(
                    &format!(
                        "SELECT id, content, sha, version, created, updated FROM {NS}_memories \
                         WHERE store_id = ?1 AND path = ?2"
                    ),
                    params![store, path],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, Vec<u8>>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, i64>(4)?,
                            r.get::<_, i64>(5)?,
                        ))
                    },
                )
                .optional()
                .map_err(mem_err)?;
            let Some((id, content, sha, version, created, updated)) = row else {
                return Ok(false);
            };
            if id != base_id || sha != base_sha {
                return Err(MemErr::Conflict {
                    current: Box::new(row_memory(
                        id, path, content, sha, version, created, updated,
                    )?),
                });
            }
            tx.execute(
                &format!("DELETE FROM {NS}_memories WHERE store_id = ?1 AND path = ?2"),
                params![store, path],
            )
            .map_err(mem_err)?;
            append_version(
                &tx,
                VersionAppend {
                    store: &store,
                    memory_id: &base_id,
                    operation: MemoryVersionOperation::Deleted,
                    path: &path,
                    content: None,
                    created: now_nanos() as i64,
                    actor: actor.as_ref(),
                },
            )?;
            tx.commit().map_err(mem_err)?;
            Ok(true)
        })
        .await
    }

    async fn list_versions(&self, store: &str) -> Result<Vec<MemoryVersion>, MemErr> {
        let store = store.to_string();
        with_conn_mem(&self.conn, move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT id, memory_id, operation, path, content, created, redacted, created_by_json, redacted_by_json \
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
        with_conn_mem(&self.conn, move |conn| {
            let tx = conn.unchecked_transaction().map_err(mem_err)?;
            let existing = tx
                .query_row(
                    &format!(
                        "SELECT id, memory_id, operation, path, content, created, redacted, created_by_json, redacted_by_json \
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
                        "UPDATE {NS}_versions SET content = NULL, redacted = ?1, redacted_by_json = ?2 \
                         WHERE store_id = ?3 AND id = ?4"
                    ),
                    params![
                        redacted,
                        actor.as_ref().map(serde_json::to_string).transpose().map_err(mem_err)?,
                        store,
                        version_id
                    ],
                )
                .map_err(mem_err)?;
                version.content = None;
                version.redacted_unix_nanos = Some(redacted as u128);
                version.redacted_by = actor.clone();
            }
            tx.commit().map_err(mem_err)?;
            Ok(Some(version))
        })
        .await
    }

    async fn purge_store(&self, store: &str) -> Result<MemoryPurgeSummary, MemErr> {
        let store = store.to_string();
        with_conn_mem(&self.conn, move |conn| {
            let tx = conn.unchecked_transaction().map_err(mem_err)?;
            let versions_deleted = tx
                .execute(
                    &format!("DELETE FROM {NS}_versions WHERE store_id = ?1"),
                    params![store],
                )
                .map_err(mem_err)? as u64;
            let heads_deleted = tx
                .execute(
                    &format!("DELETE FROM {NS}_memories WHERE store_id = ?1"),
                    params![store],
                )
                .map_err(mem_err)? as u64;
            tx.commit().map_err(mem_err)?;
            Ok(MemoryPurgeSummary {
                heads_deleted,
                versions_deleted,
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
        id: &str,
        content: &str,
        base_sha: &str,
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
        let (store, id, content, base_sha, target_path) = (
            store.to_string(),
            id.to_string(),
            content.to_string(),
            base_sha.to_string(),
            target_path.map(str::to_string),
        );
        let actor = actor.cloned();
        let expected_path = expected_path.map(str::to_owned);
        with_conn_mem(&self.conn, move |conn| {
            let tx = conn.unchecked_transaction().map_err(mem_err)?;
            let row = tx
                .query_row(
                    &format!(
                        "SELECT path, sha, content, version, created, updated FROM {NS}_memories \
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
                            r.get::<_, i64>(5)?,
                        ))
                    },
                )
                .optional()
                .map_err(mem_err)?;
            let (path, cur_sha, cur_content, version, created, updated) =
                row.ok_or_else(|| MemErr::NotFound(id.clone()))?;
            let requested_path = target_path.unwrap_or_else(|| path.clone());
            let new_sha = sha256_hex(&content);
            let already_current = cur_sha == new_sha && path == requested_path;
            if expected_path
                .as_deref()
                .is_some_and(|expected| expected != path)
            {
                return Err(MemErr::Conflict {
                    current: Box::new(row_memory(
                        id,
                        path,
                        cur_content,
                        cur_sha,
                        version,
                        created,
                        updated,
                    )?),
                });
            }
            if cur_sha != base_sha {
                if already_current {
                    return row_memory(id, path, cur_content, cur_sha, version, created, updated);
                }
                let current =
                    row_memory(id, path, cur_content, cur_sha, version, created, updated)?;
                return Err(MemErr::Conflict {
                    current: Box::new(current),
                });
            }
            if already_current {
                return row_memory(id, path, cur_content, cur_sha, version, created, updated);
            }
            let now = now_nanos() as i64;
            let displaced_id = if requested_path == path {
                None
            } else {
                tx.query_row(
                    &format!("SELECT id FROM {NS}_memories WHERE store_id = ?1 AND path = ?2"),
                    params![store, requested_path],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(mem_err)?
            };
            if displaced_id.is_some() {
                tx.execute(
                    &format!("DELETE FROM {NS}_memories WHERE store_id = ?1 AND path = ?2"),
                    params![store, requested_path],
                )
                .map_err(mem_err)?;
            }
            tx.execute(
                &format!(
                    "UPDATE {NS}_memories SET path = ?1, content = ?2, sha = ?3, \
                     version = version + 1, updated = ?4 WHERE store_id = ?5 AND id = ?6"
                ),
                params![requested_path, content.as_bytes(), new_sha, now, store, id],
            )
            .map_err(mem_err)?;
            if let Some(displaced_id) = displaced_id {
                append_version(
                    &tx,
                    VersionAppend {
                        store: &store,
                        memory_id: &displaced_id,
                        operation: MemoryVersionOperation::Deleted,
                        path: &requested_path,
                        content: None,
                        created: now,
                        actor: actor.as_ref(),
                    },
                )?;
            }
            append_version(
                &tx,
                VersionAppend {
                    store: &store,
                    memory_id: &id,
                    operation: MemoryVersionOperation::Modified,
                    path: &requested_path,
                    content: Some(&content),
                    created: now,
                    actor: actor.as_ref(),
                },
            )?;
            tx.commit().map_err(mem_err)?;
            Ok(Memory {
                content_size: content.len() as u64,
                content: Some(content),
                id,
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

    #[test]
    fn current_baseline_exposes_only_the_memory_aggregate() {
        // Causes: C1 empty ledger; C2 current V1/V2/V3 stream; C3 exact replay.
        // Effects: E1 create heads, versions, and counters only; E2 apply both
        // versions once; E3 apply nothing. Decision rules M1=C1+C2=>E1+E2;
        // M2=C3=>E3. Drift rejection is owned by the common runner tests.
        let conn = Connection::open_in_memory().expect("open sqlite");
        let full = memory_store_bundle().expect("bundle builds");
        let runner =
            awaken_scoped_migration_sqlite::SqliteMigrationRunner::with_prefix(NS).expect("runner");

        let first = runner.run_bundle(&conn, &full).expect("M1 apply V1/V2/V3");
        assert_eq!(
            first.iter().map(|m| m.version).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        let tables = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='table' \
                 AND name LIKE 'memory_store_%' \
                 AND name NOT LIKE 'memory_store_schema_migrations%' ORDER BY name",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
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
        let retired: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='memory_store_blob'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retired, 0, "M1/E1");
        assert!(runner.run_bundle(&conn, &full).expect("M2").is_empty());
    }

    /// `over` wraps a connection without migrating; the store only works once the
    /// caller opts into the `memory_store` scope via `ensure_schema`.
    #[tokio::test]
    async fn over_does_not_migrate_but_ensure_schema_does() {
        let fs = SqliteMemoryRepository::over(Connection::open_in_memory().unwrap());
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
mod memory_repository_tests {
    use super::*;

    #[tokio::test]
    async fn sqlite_memory_repository_conforms() {
        crate::conformance::conformance(&SqliteMemoryRepository::open_in_memory().unwrap()).await;
    }

    /// The same cause-effect-graph edge cases as the ephemeral backend, exercised in
    /// `crate::conformance::extended_conformance`, run against SQLite so the
    /// CAS/rename **precedence** and prefix-boundary rules are pinned here too.
    #[tokio::test]
    async fn sqlite_memory_repository_extended_conformance() {
        crate::conformance::extended_conformance(
            &SqliteMemoryRepository::open_in_memory().unwrap(),
        )
        .await;
    }

    #[tokio::test]
    async fn sqlite_head_update_is_one_atomic_transaction() {
        crate::conformance::atomic_head_update_conformance(
            &SqliteMemoryRepository::open_in_memory().unwrap(),
        )
        .await;
    }

    /// The shared "who masks whom" edge: a rename-replace over SQLite
    /// drops the destination row entirely, so the replaced destination's id is
    /// `NotFound` and the destination path carries the source id + content.
    #[tokio::test]
    async fn sqlite_rename_replace_orphans_destination_id() {
        crate::conformance::rename_replace_orphans_the_destination_id(
            &SqliteMemoryRepository::open_in_memory().unwrap(),
        )
        .await;
    }

    /// Durable high-water counters prevent a deleted memory id from ever being
    /// reissued. This must agree with the in-memory aggregate.
    #[tokio::test]
    async fn durable_ids_remain_monotonic_after_delete() {
        crate::conformance::ids_stay_monotonic_across_delete(
            &SqliteMemoryRepository::open_in_memory().unwrap(),
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
            let fs = SqliteMemoryRepository::open(path_str).unwrap();
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
                &created.id,
                "updated",
                &created.content_sha256,
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
        let fs = SqliteMemoryRepository::open(path_str).unwrap();
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
        let fs = SqliteMemoryRepository::open_in_memory().unwrap();
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

    #[tokio::test]
    async fn sqlite_store_enforces_the_shared_capacity_contract() {
        crate::repository::tests::capacity_conformance(
            &SqliteMemoryRepository::open_in_memory().unwrap(),
        )
        .await;
    }
}
