//! SQLite backend (`sqlite` feature): content-addressed blobs in a `BLOB`
//! column, over the crate's own `file_store` migration scope
//! ([`file_store_bundle`]) — the embedded sibling of [`PgFileStore`](crate::postgres::PgFileStore).
//! `put` is an idempotent `INSERT ... ON CONFLICT (id) DO NOTHING`, matching the
//! immutable/dedup contract; the id is computed in the core, so equal bytes yield
//! the same id on every backend. The `FileStore` trait is async, so the sync
//! `rusqlite` calls use the workspace's canonical SQLite scheduler boundary.

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::schema::{NS, file_store_bundle};
use crate::{
    ArtifactAssociationDecision, ArtifactCapture, CreateFileRecordOutcome, FileCatalog,
    FileCatalogError, FileRecord, FileStore, FileStoreError, ReplaceArtifactCapture,
    ReplaceArtifactCaptureOutcome, artifact_association_decision, content_id,
    decode_artifact_capture, same_harvest_identity,
};

fn e(x: impl ToString) -> FileStoreError {
    FileStoreError::Indeterminate {
        detail: x.to_string(),
    }
}

fn ce(x: impl ToString) -> FileCatalogError {
    FileCatalogError::Indeterminate {
        detail: x.to_string(),
    }
}

fn row_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileRecord> {
    Ok(FileRecord {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        blob_id: row.get(2)?,
        filename: row.get(3)?,
        mime_type: row.get(4)?,
        size_bytes: row.get::<_, i64>(5)? as u64,
        created_at: row.get(6)?,
        expires_at: row.get(7)?,
        downloadable: row.get::<_, i64>(8)? != 0,
        scope_id: row.get(9)?,
        logical_path: row.get(10)?,
        harvest_key: row.get(11)?,
        artifact_idempotency_scope: row.get(12)?,
        deleted: row.get::<_, i64>(13)? != 0,
    })
}

const FILE_COLUMNS: &str = "id, workspace_id, blob_id, filename, mime_type, \
size_bytes, created_at, expires_at, downloadable, scope_id, logical_path, harvest_key, \
artifact_idempotency_scope, deleted";

/// A SQLite-backed [`FileStore`] over a `file_store_blob(id, bytes, size, created_at)` table.
pub struct SqliteFileStore {
    conn: awaken_sqlite_pool::SharedSqliteConnection,
}

impl SqliteFileStore {
    /// Open (or create) a database file and apply the file-store migrations
    /// (one-step convenience for a store-owned database).
    pub fn open(path: &str) -> Result<Self, FileStoreError> {
        let store = Self::over(
            awaken_sqlite_pool::SqliteConnectionFactory::file(path)
                .open()
                .map_err(e)?,
        );
        store.ensure_schema()?;
        Ok(store)
    }

    /// Open a private in-memory database (tests / ephemeral).
    #[cfg(any(test, feature = "test-support"))]
    pub fn open_in_memory() -> Result<Self, FileStoreError> {
        let store = Self::over(
            awaken_sqlite_pool::SqliteConnectionFactory::memory()
                .open()
                .map_err(e)?,
        );
        store.ensure_schema()?;
        Ok(store)
    }

    /// Wrap an existing connection **without migrating**. Call
    /// [`Self::ensure_schema`], or let a unified migration pipeline own the
    /// `file_store` scope so this store shares the caller's database.
    pub fn over(conn: Connection) -> Self {
        Self {
            conn: awaken_sqlite_pool::SharedSqliteConnection::new(conn),
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
        awaken_sqlite_pool::with_connection(self.conn.clone(), move |conn| f(conn))
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
            let stored: Vec<u8> = conn
                .query_row(
                    &format!("SELECT bytes FROM {NS}_blob WHERE id = ?1"),
                    params![row_id],
                    |row| row.get(0),
                )
                .map_err(e)?;
            crate::verify_blob(&row_id, &stored)?;
            Ok(())
        })
        .await?;
        Ok(id)
    }

    async fn get(&self, id: &str) -> Result<Option<Vec<u8>>, FileStoreError> {
        let id = id.to_string();
        self.with_conn(move |conn| {
            let bytes: Option<Vec<u8>> = conn
                .query_row(
                    &format!("SELECT bytes FROM {NS}_blob WHERE id = ?1"),
                    params![id],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()
                .map_err(e)?;
            if let Some(bytes) = &bytes {
                crate::verify_blob(&id, bytes)?;
            }
            Ok(bytes)
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

#[async_trait]
impl FileCatalog for SqliteFileStore {
    async fn artifact_capture(
        &self,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<Option<ArtifactCapture>, FileCatalogError> {
        let workspace_id = workspace_id.to_string();
        let session_id = session_id.to_string();
        self.with_conn(move |conn| {
            let row: Option<(i64, String)> = conn.query_row(
                &format!("SELECT revision,file_ids_json FROM {NS}_artifact_capture WHERE workspace_id=?1 AND session_id=?2"),
                params![workspace_id, session_id], |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional().map_err(e)?;
            Ok(row.map(|(revision, ids)| decode_artifact_capture(workspace_id, session_id, revision, &ids)).transpose())
        }).await.map_err(ce)?
    }

    async fn replace_artifact_capture(
        &self,
        request: ReplaceArtifactCapture,
    ) -> Result<ReplaceArtifactCaptureOutcome, FileCatalogError> {
        self.with_conn(move |conn| {
            let transaction = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).map_err(e)?;
            let row: Option<(i64, String)> = transaction.query_row(
                &format!("SELECT revision,file_ids_json FROM {NS}_artifact_capture WHERE workspace_id=?1 AND session_id=?2"),
                params![request.workspace_id, request.session_id], |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional().map_err(e)?;
            let current = match row.map(|(revision, ids)| decode_artifact_capture(request.workspace_id.clone(), request.session_id.clone(), revision, &ids)).transpose() {
                Ok(current) => current, Err(error) => return Ok(Err(error)),
            };
            let outcome = match request.decide(current.as_ref()) {
                Ok(outcome) => outcome, Err(error) => return Ok(Err(error)),
            };
            if let ReplaceArtifactCaptureOutcome::Applied(capture) = &outcome {
                let mut records = Vec::new();
                for id in &capture.file_ids {
                    if let Some(record) = transaction.query_row(
                        &format!("SELECT {FILE_COLUMNS} FROM {NS}_file WHERE id=?1"),
                        params![id], row_record,
                    ).optional().map_err(e)? { records.push(record); }
                }
                if let Err(error) = capture.verify_files(&records) { return Ok(Err(error)); }
                transaction.execute(
                    &format!("INSERT INTO {NS}_artifact_capture (workspace_id,session_id,revision,file_ids_json) VALUES (?1,?2,?3,?4) ON CONFLICT(workspace_id,session_id) DO UPDATE SET revision=excluded.revision,file_ids_json=excluded.file_ids_json"),
                    params![capture.workspace_id, capture.session_id, awaken_sql_storage::StoredU64::try_from(capture.revision).map_err(e)?.database_value(), serde_json::to_string(&capture.file_ids).map_err(e)?],
                ).map_err(e)?;
            }
            transaction.commit().map_err(e)?;
            Ok(Ok(outcome))
        }).await.map_err(ce)?
    }

    async fn create_file(
        &self,
        record: FileRecord,
    ) -> Result<CreateFileRecordOutcome, FileCatalogError> {
        crate::validate_record(&record)?;
        self.with_conn(move |conn| {
            let transaction =
                Transaction::new_unchecked(conn, TransactionBehavior::Immediate).map_err(e)?;
            if let (Some(key), Some(requested)) = (
                record.harvest_key.as_deref(),
                record.artifact_idempotency_scope.as_deref(),
            ) {
                let matching = {
                    let mut statement = transaction
                        .prepare(&format!(
                            "SELECT {FILE_COLUMNS} FROM {NS}_file \
                             WHERE workspace_id=?1 AND harvest_key=?2 \
                             ORDER BY deleted ASC, created_at DESC, id DESC"
                        ))
                        .map_err(e)?;
                    let rows = statement
                        .query_map(params![record.workspace_id, key], row_record)
                        .map_err(e)?;
                    rows.collect::<Result<Vec<_>, _>>().map_err(e)?
                };
                match artifact_association_decision(&matching, &record) {
                    Ok(Some(ArtifactAssociationDecision::Existing(existing))) => {
                        transaction.commit().map_err(e)?;
                        return Ok(Ok(CreateFileRecordOutcome::Existing(existing)));
                    }
                    Ok(Some(ArtifactAssociationDecision::Associate(target))) => {
                        transaction
                            .execute(
                                &format!(
                                    "UPDATE {NS}_file SET artifact_idempotency_scope=?1 \
                                     WHERE id=?2 AND artifact_idempotency_scope IS NULL"
                                ),
                                params![requested, target.id],
                            )
                            .map_err(e)?;
                        let associated = transaction
                            .query_row(
                                &format!("SELECT {FILE_COLUMNS} FROM {NS}_file WHERE id=?1"),
                                params![target.id],
                                row_record,
                            )
                            .map_err(e)?;
                        if associated.artifact_idempotency_scope.as_deref() != Some(requested) {
                            return Ok(Err(FileCatalogError::Invalid(
                                "artifact File association raced another terminal operation".into(),
                            )));
                        }
                        transaction.commit().map_err(e)?;
                        return Ok(Ok(CreateFileRecordOutcome::Existing(associated)));
                    }
                    Ok(None) if record.deleted => {
                        return Ok(Err(FileCatalogError::Invalid(
                            "terminal association cannot recreate a missing tombstone".into(),
                        )));
                    }
                    Ok(None) => {}
                    Err(error) => return Ok(Err(error)),
                }
            }

            let inserted = transaction
                .execute(
                    &format!(
                        "INSERT OR IGNORE INTO {NS}_file ({FILE_COLUMNS}) \
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)"
                    ),
                    params![
                        record.id,
                        record.workspace_id,
                        record.blob_id,
                        record.filename,
                        record.mime_type,
                        record.size_bytes as i64,
                        record.created_at,
                        record.expires_at,
                        i64::from(record.downloadable),
                        record.scope_id,
                        record.logical_path,
                        record.harvest_key,
                        record.artifact_idempotency_scope,
                        i64::from(record.deleted),
                    ],
                )
                .map_err(e)?;
            let lookup = if inserted > 0 {
                transaction
                    .query_row(
                        &format!("SELECT {FILE_COLUMNS} FROM {NS}_file WHERE id=?1"),
                        params![record.id],
                        row_record,
                    )
                    .map_err(e)?
            } else if let Some(key) = record.harvest_key.as_deref() {
                transaction
                    .query_row(
                        &format!(
                            "SELECT {FILE_COLUMNS} FROM {NS}_file \
                         WHERE workspace_id=?1 AND harvest_key=?2 AND deleted=0"
                        ),
                        params![record.workspace_id, key],
                        row_record,
                    )
                    .map_err(e)?
            } else {
                return Ok(Err(FileCatalogError::Invalid(
                    format!("file id `{}` already exists", record.id).into(),
                )));
            };
            if inserted == 0 && !same_harvest_identity(&lookup, &record) {
                return Ok(Err(FileCatalogError::Invalid(
                    "artifact harvest key is bound to different File identity".into(),
                )));
            }
            let outcome = if inserted > 0 {
                CreateFileRecordOutcome::Inserted(lookup)
            } else {
                CreateFileRecordOutcome::Existing(lookup)
            };
            transaction.commit().map_err(e)?;
            Ok(Ok(outcome))
        })
        .await
        .map_err(ce)?
    }

    async fn get_file(
        &self,
        workspace_id: &str,
        file_id: &str,
        include_deleted: bool,
    ) -> Result<Option<FileRecord>, FileCatalogError> {
        let workspace = workspace_id.to_string();
        let id = file_id.to_string();
        self.with_conn(move |conn| {
            conn.query_row(
                &format!(
                    "SELECT {FILE_COLUMNS} FROM {NS}_file \
                     WHERE id=?1 AND workspace_id=?2 AND (?3=1 OR deleted=0)"
                ),
                params![id, workspace, i64::from(include_deleted)],
                row_record,
            )
            .optional()
            .map_err(e)
        })
        .await
        .map_err(ce)
    }

    async fn list_files(
        &self,
        workspace_id: &str,
        scope_id: Option<&str>,
    ) -> Result<Vec<FileRecord>, FileCatalogError> {
        let workspace = workspace_id.to_string();
        let scope = scope_id.map(str::to_string);
        self.with_conn(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {FILE_COLUMNS} FROM {NS}_file \
                     WHERE workspace_id=?1 AND deleted=0 \
                     AND (?2 IS NULL OR scope_id=?2) ORDER BY created_at DESC, id DESC"
                ))
                .map_err(e)?;
            let rows = stmt
                .query_map(params![workspace, scope], row_record)
                .map_err(e)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(e)
        })
        .await
        .map_err(ce)
    }

    async fn list_files_including_deleted(
        &self,
        workspace_id: &str,
        scope_id: Option<&str>,
    ) -> Result<Vec<FileRecord>, FileCatalogError> {
        let workspace = workspace_id.to_string();
        let scope = scope_id.map(str::to_string);
        self.with_conn(move |conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {FILE_COLUMNS} FROM {NS}_file \
                     WHERE workspace_id=?1 AND (?2 IS NULL OR scope_id=?2) \
                     ORDER BY created_at DESC, id DESC"
                ))
                .map_err(e)?;
            let rows = stmt
                .query_map(params![workspace, scope], row_record)
                .map_err(e)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(e)
        })
        .await
        .map_err(ce)
    }

    async fn mark_file_deleted(
        &self,
        workspace_id: &str,
        file_id: &str,
    ) -> Result<Option<FileRecord>, FileCatalogError> {
        let workspace = workspace_id.to_string();
        let id = file_id.to_string();
        self.with_conn(move |conn| {
            conn.execute(
                &format!("UPDATE {NS}_file SET deleted=1 WHERE id=?1 AND workspace_id=?2"),
                params![id, workspace],
            )
            .map_err(e)?;
            conn.query_row(
                &format!("SELECT {FILE_COLUMNS} FROM {NS}_file WHERE id=?1 AND workspace_id=?2"),
                params![id, workspace],
                row_record,
            )
            .optional()
            .map_err(e)
        })
        .await
        .map_err(ce)
    }

    async fn active_size_bytes(&self, workspace_id: &str) -> Result<u64, FileCatalogError> {
        let workspace = workspace_id.to_string();
        self.with_conn(move |conn| {
            conn.query_row(
                &format!(
                    "SELECT COALESCE(SUM(size_bytes),0) FROM {NS}_file \
                     WHERE workspace_id=?1 AND deleted=0"
                ),
                params![workspace],
                |row| row.get::<_, i64>(0),
            )
            .map(|value| value as u64)
            .map_err(e)
        })
        .await
        .map_err(ce)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn corrupted_identity_rejects_read_and_replay_without_repairing_bytes() {
        // C1 stored bytes match their id => read/replay succeed. C2 external
        // corruption retains the id but changes bytes => both return the exact
        // CorruptContent identity; neither overwrites damaged evidence. The
        // filesystem and object-store suites exercise the same partitions.
        let store = SqliteFileStore::open_in_memory().unwrap();
        let id = store.put(b"original").await.unwrap();
        assert_eq!(store.get(&id).await.unwrap(), Some(b"original".to_vec()));
        assert_eq!(store.put(b"original").await.unwrap(), id);
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                &format!("UPDATE {NS}_blob SET bytes = ?1 WHERE id = ?2"),
                params![b"damaged".as_slice(), &id],
            )
            .unwrap();
        assert!(matches!(store.get(&id).await,
            Err(FileStoreError::CorruptContent { content_id }) if content_id == id));
        assert!(matches!(store.put(b"original").await,
            Err(FileStoreError::CorruptContent { content_id }) if content_id == id));
        let persisted: Vec<u8> = store
            .conn
            .lock()
            .unwrap()
            .query_row(
                &format!("SELECT bytes FROM {NS}_blob WHERE id = ?1"),
                params![&id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(persisted, b"damaged");
    }

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
