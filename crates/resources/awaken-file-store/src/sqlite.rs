// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: This source implements the repository-controlled store product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! SQLite backend (`sqlite` feature): content-addressed blobs in a `BLOB`
//! column, over the crate's own `file_store` migration scope
//! ([`file_store_bundle`]) — the embedded sibling of [`PgFileStore`](crate::postgres::PgFileStore).
//! `put` is an idempotent `INSERT ... ON CONFLICT (id) DO NOTHING`, matching the
//! immutable/dedup contract; the id is computed in the core, so equal bytes yield
//! the same id on every backend. The store reaches SQLite through the embedded
//! sqlx pool policy (ADR-0087).

use async_trait::async_trait;
use awaken_scoped_migration::sqlite::SqliteMigrationRunner;
use awaken_sqlite_pool::EmbeddedPool;
use awaken_sqlite_pool::EmbeddedSqlite;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteConnection};

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

/// A catalog storage failure: the store-level indeterminate error lifted into the
/// catalog's indeterminate outcome (the same text the store has always produced).
fn cs(x: impl ToString) -> FileCatalogError {
    ce(e(x))
}

fn row_record(row: &SqliteRow) -> Result<FileRecord, sqlx::Error> {
    Ok(FileRecord {
        id: row.try_get(0)?,
        workspace_id: row.try_get(1)?,
        blob_id: row.try_get(2)?,
        filename: row.try_get(3)?,
        mime_type: row.try_get(4)?,
        size_bytes: row.try_get::<i64, _>(5)? as u64,
        created_at: row.try_get(6)?,
        expires_at: row.try_get(7)?,
        downloadable: row.try_get::<i64, _>(8)? != 0,
        scope_id: row.try_get(9)?,
        logical_path: row.try_get(10)?,
        harvest_key: row.try_get(11)?,
        artifact_idempotency_scope: row.try_get(12)?,
        deleted: row.try_get::<i64, _>(13)? != 0,
    })
}

fn row_records(rows: Vec<SqliteRow>) -> Result<Vec<FileRecord>, sqlx::Error> {
    rows.iter().map(row_record).collect()
}

const FILE_COLUMNS: &str = "id, workspace_id, blob_id, filename, mime_type, \
size_bytes, created_at, expires_at, downloadable, scope_id, logical_path, harvest_key, \
artifact_idempotency_scope, deleted";

async fn file_by_id(
    conn: &mut SqliteConnection,
    id: &str,
) -> Result<Option<FileRecord>, FileStoreError> {
    sqlx::query(&format!("SELECT {FILE_COLUMNS} FROM {NS}_file WHERE id=?1"))
        .bind(id)
        .fetch_optional(conn)
        .await
        .map_err(e)?
        .map(|row| row_record(&row))
        .transpose()
        .map_err(e)
}

/// A SQLite-backed [`FileStore`] over a `file_store_blob(id, bytes, size, created_at)` table.
pub struct SqliteFileStore {
    pool: EmbeddedPool,
}

impl SqliteFileStore {
    /// Open (or create) a database file and apply the file-store migrations
    /// (one-step convenience for a store-owned database).
    pub async fn open(path: &str) -> Result<Self, FileStoreError> {
        Self::open_at(EmbeddedSqlite::file(path)).await
    }

    /// Open a private in-memory database (tests / ephemeral).
    #[cfg(any(test, feature = "test-support"))]
    pub async fn open_in_memory() -> Result<Self, FileStoreError> {
        Self::open_at(EmbeddedSqlite::memory()).await
    }

    async fn open_at(location: EmbeddedSqlite) -> Result<Self, FileStoreError> {
        let store = Self::over(location.open().await.map_err(e)?);
        store.ensure_schema().await?;
        Ok(store)
    }

    /// Wrap an existing pool **without migrating**. Call
    /// [`Self::ensure_schema`], or let a unified migration pipeline own the
    /// `file_store` scope so this store shares the caller's database.
    pub fn over(pool: EmbeddedPool) -> Self {
        Self { pool }
    }

    /// Apply the `file_store` scoped migration bundle (idempotent). Optional:
    /// skip it when the schema is owned externally.
    pub async fn ensure_schema(&self) -> Result<(), FileStoreError> {
        let bundle = file_store_bundle().map_err(e)?;
        awaken_sqlite_pool::held(
            SqliteMigrationRunner::with_prefix(self.pool.sqlx().clone(), NS)
                .map_err(e)?
                .run_bundle(&bundle),
        )
        .await
        .map_err(e)?;
        Ok(())
    }

    async fn replace_capture(
        &self,
        request: ReplaceArtifactCapture,
    ) -> Result<Result<ReplaceArtifactCaptureOutcome, FileCatalogError>, FileStoreError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await.map_err(e)?;
        let row = sqlx::query_as::<_, (i64, String, Option<String>)>(&format!(
            "SELECT revision,file_ids_json,origin_json FROM {NS}_artifact_capture WHERE workspace_id=?1 AND session_id=?2"
        ))
        .bind(&request.workspace_id)
        .bind(&request.session_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(e)?;
        let current = match row
            .map(|(revision, ids, origin)| {
                decode_artifact_capture(
                    request.workspace_id.clone(),
                    request.session_id.clone(),
                    revision,
                    &ids,
                    origin.as_deref(),
                )
            })
            .transpose()
        {
            Ok(current) => current,
            Err(error) => return Ok(Err(error)),
        };
        let outcome = match request.decide(current.as_ref()) {
            Ok(outcome) => outcome,
            Err(error) => return Ok(Err(error)),
        };
        if let ReplaceArtifactCaptureOutcome::Applied(capture) = &outcome {
            let mut records = Vec::new();
            for id in &capture.file_ids {
                if let Some(record) = file_by_id(&mut transaction, id).await? {
                    records.push(record);
                }
            }
            if let Err(error) = capture.verify_files(&records) {
                return Ok(Err(error));
            }
            sqlx::query(&format!(
                "INSERT INTO {NS}_artifact_capture (workspace_id,session_id,revision,file_ids_json,origin_json) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(workspace_id,session_id) DO UPDATE SET revision=excluded.revision,file_ids_json=excluded.file_ids_json,origin_json=excluded.origin_json"
            ))
            .bind(&capture.workspace_id)
            .bind(&capture.session_id)
            .bind(
                awaken_sql_storage::StoredU64::try_from(capture.revision)
                    .map_err(e)?
                    .database_value(),
            )
            .bind(serde_json::to_string(&capture.file_ids).map_err(e)?)
            .bind(capture.origin.as_ref().map(serde_json::to_string).transpose().map_err(e)?)
            .execute(&mut *transaction)
            .await
            .map_err(e)?;
        }
        transaction.commit().await.map_err(e)?;
        Ok(Ok(outcome))
    }

    async fn create_record(
        &self,
        record: FileRecord,
    ) -> Result<Result<CreateFileRecordOutcome, FileCatalogError>, FileStoreError> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await.map_err(e)?;
        if let (Some(key), Some(requested)) = (
            record.harvest_key.as_deref(),
            record.artifact_idempotency_scope.as_deref(),
        ) {
            let matching = row_records(
                sqlx::query(&format!(
                    "SELECT {FILE_COLUMNS} FROM {NS}_file \
                     WHERE workspace_id=?1 AND harvest_key=?2 \
                     ORDER BY deleted ASC, created_at DESC, id DESC"
                ))
                .bind(&record.workspace_id)
                .bind(key)
                .fetch_all(&mut *transaction)
                .await
                .map_err(e)?,
            )
            .map_err(e)?;
            match artifact_association_decision(&matching, &record) {
                Ok(Some(ArtifactAssociationDecision::Existing(existing))) => {
                    transaction.commit().await.map_err(e)?;
                    return Ok(Ok(CreateFileRecordOutcome::Existing(existing)));
                }
                Ok(Some(ArtifactAssociationDecision::Associate(target))) => {
                    sqlx::query(&format!(
                        "UPDATE {NS}_file SET artifact_idempotency_scope=?1 \
                         WHERE id=?2 AND artifact_idempotency_scope IS NULL"
                    ))
                    .bind(requested)
                    .bind(&target.id)
                    .execute(&mut *transaction)
                    .await
                    .map_err(e)?;
                    let associated = file_by_id(&mut transaction, &target.id)
                        .await?
                        .ok_or_else(|| e(sqlx::Error::RowNotFound))?;
                    if associated.artifact_idempotency_scope.as_deref() != Some(requested) {
                        return Ok(Err(FileCatalogError::AssociationConflict {
                            detail: "artifact File association raced another terminal operation"
                                .into(),
                        }));
                    }
                    transaction.commit().await.map_err(e)?;
                    return Ok(Ok(CreateFileRecordOutcome::Existing(associated)));
                }
                Ok(None) if record.deleted => {
                    return Ok(Err(FileCatalogError::AssociationConflict {
                        detail: "terminal association cannot recreate a missing tombstone".into(),
                    }));
                }
                Ok(None) => {}
                Err(error) => return Ok(Err(error)),
            }
        }

        let inserted = sqlx::query(&format!(
            "INSERT OR IGNORE INTO {NS}_file ({FILE_COLUMNS}) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)"
        ))
        .bind(&record.id)
        .bind(&record.workspace_id)
        .bind(&record.blob_id)
        .bind(&record.filename)
        .bind(&record.mime_type)
        .bind(record.size_bytes as i64)
        .bind(&record.created_at)
        .bind(&record.expires_at)
        .bind(i64::from(record.downloadable))
        .bind(&record.scope_id)
        .bind(&record.logical_path)
        .bind(&record.harvest_key)
        .bind(&record.artifact_idempotency_scope)
        .bind(i64::from(record.deleted))
        .execute(&mut *transaction)
        .await
        .map_err(e)?
        .rows_affected();
        let lookup = if inserted > 0 {
            file_by_id(&mut transaction, &record.id)
                .await?
                .ok_or_else(|| e(sqlx::Error::RowNotFound))?
        } else if let Some(key) = record.harvest_key.as_deref() {
            let row = sqlx::query(&format!(
                "SELECT {FILE_COLUMNS} FROM {NS}_file \
                 WHERE workspace_id=?1 AND harvest_key=?2 AND deleted=0"
            ))
            .bind(&record.workspace_id)
            .bind(key)
            .fetch_one(&mut *transaction)
            .await
            .map_err(e)?;
            row_record(&row).map_err(e)?
        } else {
            return Ok(Err(FileCatalogError::IdentityConflict {
                detail: format!("file id `{}` already exists", record.id),
            }));
        };
        if inserted == 0 && !same_harvest_identity(&lookup, &record) {
            return Ok(Err(FileCatalogError::IdentityConflict {
                detail: "artifact harvest key is bound to different File identity".into(),
            }));
        }
        let outcome = if inserted > 0 {
            CreateFileRecordOutcome::Inserted(lookup)
        } else {
            CreateFileRecordOutcome::Existing(lookup)
        };
        transaction.commit().await.map_err(e)?;
        Ok(Ok(outcome))
    }
}

#[async_trait]
impl FileStore for SqliteFileStore {
    async fn put(&self, bytes: &[u8]) -> Result<String, FileStoreError> {
        let id = content_id(bytes);
        let size = bytes.len() as i64;
        let mut conn = self.pool.acquire().await.map_err(e)?;
        sqlx::query(&format!(
            "INSERT INTO {NS}_blob (id, bytes, size) VALUES (?1, ?2, ?3) \
             ON CONFLICT(id) DO NOTHING"
        ))
        .bind(&id)
        .bind(bytes)
        .bind(size)
        .execute(&mut *conn)
        .await
        .map_err(e)?;
        let stored: Vec<u8> =
            sqlx::query_scalar(&format!("SELECT bytes FROM {NS}_blob WHERE id = ?1"))
                .bind(&id)
                .fetch_one(&mut *conn)
                .await
                .map_err(e)?;
        crate::verify_blob(&id, &stored)?;
        Ok(id)
    }

    async fn get(&self, id: &str) -> Result<Option<Vec<u8>>, FileStoreError> {
        let bytes: Option<Vec<u8>> =
            sqlx::query_scalar(&format!("SELECT bytes FROM {NS}_blob WHERE id = ?1"))
                .bind(id)
                .fetch_optional(&self.pool)
                .await
                .map_err(e)?;
        if let Some(bytes) = &bytes {
            crate::verify_blob(id, bytes)?;
        }
        Ok(bytes)
    }

    async fn list(&self) -> Result<Vec<String>, FileStoreError> {
        sqlx::query_scalar(&format!("SELECT id FROM {NS}_blob ORDER BY id"))
            .fetch_all(&self.pool)
            .await
            .map_err(e)
    }

    async fn delete(&self, id: &str) -> Result<bool, FileStoreError> {
        let affected = sqlx::query(&format!("DELETE FROM {NS}_blob WHERE id = ?1"))
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(e)?
            .rows_affected();
        Ok(affected > 0)
    }
}

#[async_trait]
impl FileCatalog for SqliteFileStore {
    async fn artifact_capture(
        &self,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<Option<ArtifactCapture>, FileCatalogError> {
        let row = sqlx::query_as::<_, (i64, String, Option<String>)>(&format!(
            "SELECT revision,file_ids_json,origin_json FROM {NS}_artifact_capture WHERE workspace_id=?1 AND session_id=?2"
        ))
        .bind(workspace_id)
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(cs)?;
        row.map(|(revision, ids, origin)| {
            decode_artifact_capture(
                workspace_id.to_string(),
                session_id.to_string(),
                revision,
                &ids,
                origin.as_deref(),
            )
        })
        .transpose()
    }

    async fn replace_artifact_capture(
        &self,
        request: ReplaceArtifactCapture,
    ) -> Result<ReplaceArtifactCaptureOutcome, FileCatalogError> {
        self.replace_capture(request).await.map_err(ce)?
    }

    async fn create_file(
        &self,
        record: FileRecord,
    ) -> Result<CreateFileRecordOutcome, FileCatalogError> {
        crate::validate_record(&record)?;
        self.create_record(record).await.map_err(ce)?
    }

    async fn get_file(
        &self,
        workspace_id: &str,
        file_id: &str,
        include_deleted: bool,
    ) -> Result<Option<FileRecord>, FileCatalogError> {
        sqlx::query(&format!(
            "SELECT {FILE_COLUMNS} FROM {NS}_file \
             WHERE id=?1 AND workspace_id=?2 AND (?3=1 OR deleted=0)"
        ))
        .bind(file_id)
        .bind(workspace_id)
        .bind(i64::from(include_deleted))
        .fetch_optional(&self.pool)
        .await
        .map_err(cs)?
        .map(|row| row_record(&row))
        .transpose()
        .map_err(cs)
    }

    async fn list_files(
        &self,
        workspace_id: &str,
        scope_id: Option<&str>,
    ) -> Result<Vec<FileRecord>, FileCatalogError> {
        row_records(
            sqlx::query(&format!(
                "SELECT {FILE_COLUMNS} FROM {NS}_file \
                 WHERE workspace_id=?1 AND deleted=0 \
                 AND (?2 IS NULL OR scope_id=?2) ORDER BY created_at DESC, id DESC"
            ))
            .bind(workspace_id)
            .bind(scope_id)
            .fetch_all(&self.pool)
            .await
            .map_err(cs)?,
        )
        .map_err(cs)
    }

    async fn list_files_including_deleted(
        &self,
        workspace_id: &str,
        scope_id: Option<&str>,
    ) -> Result<Vec<FileRecord>, FileCatalogError> {
        row_records(
            sqlx::query(&format!(
                "SELECT {FILE_COLUMNS} FROM {NS}_file \
                 WHERE workspace_id=?1 AND (?2 IS NULL OR scope_id=?2) \
                 ORDER BY created_at DESC, id DESC"
            ))
            .bind(workspace_id)
            .bind(scope_id)
            .fetch_all(&self.pool)
            .await
            .map_err(cs)?,
        )
        .map_err(cs)
    }

    async fn mark_file_deleted(
        &self,
        workspace_id: &str,
        file_id: &str,
    ) -> Result<Option<FileRecord>, FileCatalogError> {
        let mut conn = self.pool.acquire().await.map_err(cs)?;
        sqlx::query(&format!(
            "UPDATE {NS}_file SET deleted=1 WHERE id=?1 AND workspace_id=?2"
        ))
        .bind(file_id)
        .bind(workspace_id)
        .execute(&mut *conn)
        .await
        .map_err(cs)?;
        sqlx::query(&format!(
            "SELECT {FILE_COLUMNS} FROM {NS}_file WHERE id=?1 AND workspace_id=?2"
        ))
        .bind(file_id)
        .bind(workspace_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(cs)?
        .map(|row| row_record(&row))
        .transpose()
        .map_err(cs)
    }

    async fn active_size_bytes(&self, workspace_id: &str) -> Result<u64, FileCatalogError> {
        sqlx::query_scalar::<_, i64>(&format!(
            "SELECT COALESCE(SUM(size_bytes),0) FROM {NS}_file \
             WHERE workspace_id=?1 AND deleted=0"
        ))
        .bind(workspace_id)
        .fetch_one(&self.pool)
        .await
        .map(|value| value as u64)
        .map_err(cs)
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
        let store = SqliteFileStore::open_in_memory().await.unwrap();
        let id = store.put(b"original").await.unwrap();
        assert_eq!(store.get(&id).await.unwrap(), Some(b"original".to_vec()));
        assert_eq!(store.put(b"original").await.unwrap(), id);
        sqlx::query(&format!("UPDATE {NS}_blob SET bytes = ?1 WHERE id = ?2"))
            .bind(b"damaged".as_slice())
            .bind(&id)
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(matches!(store.get(&id).await,
            Err(FileStoreError::CorruptContent { content_id }) if content_id == id));
        assert!(matches!(store.put(b"original").await,
            Err(FileStoreError::CorruptContent { content_id }) if content_id == id));
        let persisted: Vec<u8> =
            sqlx::query_scalar(&format!("SELECT bytes FROM {NS}_blob WHERE id = ?1"))
                .bind(&id)
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(persisted, b"damaged");
    }

    /// `over` wraps a connection but must NOT migrate: the store is unusable until
    /// the caller opts into the `file_store` scope via `ensure_schema` (or lets a
    /// unified pipeline own the schema). This is the seam that lets the store share
    /// a caller's single database instead of forcing a parallel migration.
    #[tokio::test]
    async fn over_does_not_migrate_but_ensure_schema_does() {
        let store = SqliteFileStore::over(EmbeddedSqlite::memory().open().await.unwrap());
        // No schema yet → the blob table is absent, so writes fail.
        assert!(store.put(b"hello").await.is_err());

        // Opting in applies the scoped bundle; now the store works.
        store.ensure_schema().await.unwrap();
        let id = store.put(b"hello").await.unwrap();
        assert_eq!(store.get(&id).await.unwrap(), Some(b"hello".to_vec()));

        // ensure_schema is idempotent (safe to re-run).
        store.ensure_schema().await.unwrap();
    }
}
