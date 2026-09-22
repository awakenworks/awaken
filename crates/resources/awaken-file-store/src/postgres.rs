//! Postgres backend (`postgres` feature): content-addressed blobs in a `bytea`
//! column. `put` is an idempotent `INSERT ... ON CONFLICT (id) DO NOTHING`, matching
//! the immutable/dedup contract. Compile-verified; running needs a database.

use async_trait::async_trait;
use sqlx::{PgPool, Postgres, Row, Transaction};

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

fn row_record(row: &sqlx::postgres::PgRow) -> FileRecord {
    FileRecord {
        id: row.get("id"),
        workspace_id: row.get("workspace_id"),
        blob_id: row.get("blob_id"),
        filename: row.get("filename"),
        mime_type: row.get("mime_type"),
        size_bytes: row.get::<i64, _>("size_bytes") as u64,
        created_at: row.get("file_created_at"),
        expires_at: row.get("expires_at"),
        downloadable: row.get::<i64, _>("downloadable") != 0,
        scope_id: row.get("scope_id"),
        logical_path: row.get("logical_path"),
        harvest_key: row.get("harvest_key"),
        artifact_idempotency_scope: row.get("artifact_idempotency_scope"),
        deleted: row.get::<i64, _>("deleted") != 0,
    }
}

const FILE_COLUMNS: &str = "id, workspace_id, blob_id, filename, mime_type, size_bytes, \
created_at AS file_created_at, expires_at, downloadable, scope_id, logical_path, harvest_key, \
artifact_idempotency_scope, deleted";

async fn harvest_records_for_update(
    transaction: &mut Transaction<'_, Postgres>,
    workspace_id: &str,
    harvest_key: &str,
) -> Result<Vec<FileRecord>, FileCatalogError> {
    sqlx::query(&format!(
        "SELECT {FILE_COLUMNS} FROM file_store_file \
         WHERE workspace_id=$1 AND harvest_key=$2 \
         ORDER BY deleted ASC, created_at DESC, id DESC FOR UPDATE"
    ))
    .bind(workspace_id)
    .bind(harvest_key)
    .fetch_all(&mut **transaction)
    .await
    .map(|rows| rows.iter().map(row_record).collect())
    .map_err(ce)
}

async fn persist_artifact_association(
    transaction: &mut Transaction<'_, Postgres>,
    records: &[FileRecord],
    candidate: &FileRecord,
) -> Result<Option<CreateFileRecordOutcome>, FileCatalogError> {
    let requested_scope = candidate
        .artifact_idempotency_scope
        .as_deref()
        .ok_or_else(|| {
            FileCatalogError::Invalid(
                "artifact association requires a terminal idempotency scope".into(),
            )
        })?;
    match artifact_association_decision(records, candidate)? {
        Some(ArtifactAssociationDecision::Existing(existing)) => {
            Ok(Some(CreateFileRecordOutcome::Existing(existing)))
        }
        Some(ArtifactAssociationDecision::Associate(target)) => {
            let updated = sqlx::query(&format!(
                "UPDATE file_store_file SET artifact_idempotency_scope=$1 \
                 WHERE id=$2 AND artifact_idempotency_scope IS NULL \
                 RETURNING {FILE_COLUMNS}"
            ))
            .bind(requested_scope)
            .bind(&target.id)
            .fetch_optional(&mut **transaction)
            .await
            .map_err(ce)?
            .ok_or_else(|| {
                FileCatalogError::Invalid(
                    "artifact File association raced another terminal operation".into(),
                )
            })?;
            Ok(Some(CreateFileRecordOutcome::Existing(row_record(
                &updated,
            ))))
        }
        None => Ok(None),
    }
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

    /// Connect to an already-migrated schema without executing DDL.
    pub async fn connect_existing(url: &str) -> Result<Self, FileStoreError> {
        let pool = PgPool::connect(url).await.map_err(e)?;
        Self::with_existing_pool(pool).await
    }

    /// Wrap an existing pool and verify the externally-owned migration ledger.
    pub async fn with_existing_pool(pool: PgPool) -> Result<Self, FileStoreError> {
        let bundle = file_store_bundle().map_err(e)?;
        awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(pool.clone(), NS)
            .map_err(e)?
            .verify_bundle(&bundle)
            .await
            .map_err(e)?;
        Ok(Self { pool })
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
        self.get(&id)
            .await?
            .ok_or_else(|| FileStoreError::Indeterminate {
                detail: "content-addressed blob disappeared during idempotent put".into(),
            })?;
        Ok(id)
    }

    async fn get(&self, id: &str) -> Result<Option<Vec<u8>>, FileStoreError> {
        let row = sqlx::query("SELECT bytes FROM file_store_blob WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(e)?;
        let bytes = row.map(|r| r.get::<Vec<u8>, _>("bytes"));
        if let Some(bytes) = &bytes {
            crate::verify_blob(id, bytes)?;
        }
        Ok(bytes)
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

#[async_trait]
impl FileCatalog for PgFileStore {
    async fn artifact_capture(
        &self,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<Option<ArtifactCapture>, FileCatalogError> {
        let row = sqlx::query("SELECT revision,file_ids_json FROM file_store_artifact_capture WHERE workspace_id=$1 AND session_id=$2")
            .bind(workspace_id).bind(session_id).fetch_optional(&self.pool).await.map_err(ce)?;
        row.map(|row| {
            decode_artifact_capture(
                workspace_id.into(),
                session_id.into(),
                row.get("revision"),
                row.get("file_ids_json"),
            )
        })
        .transpose()
    }

    async fn replace_artifact_capture(
        &self,
        request: ReplaceArtifactCapture,
    ) -> Result<ReplaceArtifactCaptureOutcome, FileCatalogError> {
        // One row lock serializes first creation as well as later replacement.
        // The revision-zero seed is transaction-local and never a public capture.
        request.decide(None)?;
        let mut transaction = self.pool.begin().await.map_err(ce)?;
        let inserted = sqlx::query("INSERT INTO file_store_artifact_capture (workspace_id,session_id,revision,file_ids_json) VALUES ($1,$2,0,'[]') ON CONFLICT DO NOTHING")
            .bind(&request.workspace_id).bind(&request.session_id).execute(&mut *transaction).await.map_err(ce)?.rows_affected() == 1;
        let row = sqlx::query("SELECT revision,file_ids_json FROM file_store_artifact_capture WHERE workspace_id=$1 AND session_id=$2 FOR UPDATE")
            .bind(&request.workspace_id).bind(&request.session_id).fetch_one(&mut *transaction).await.map_err(ce)?;
        let revision: i64 = row.get("revision");
        let current = if inserted {
            None
        } else {
            Some(decode_artifact_capture(
                request.workspace_id.clone(),
                request.session_id.clone(),
                revision,
                row.get("file_ids_json"),
            )?)
        };
        let outcome = request.decide(current.as_ref())?;
        if let ReplaceArtifactCaptureOutcome::Applied(capture) = &outcome {
            let mut records = Vec::new();
            // Canonical id order also gives competing captures one File lock order.
            for id in &capture.file_ids {
                if let Some(row) = sqlx::query(&format!(
                    "SELECT {FILE_COLUMNS} FROM file_store_file WHERE id=$1 FOR SHARE"
                ))
                .bind(id)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(ce)?
                {
                    records.push(row_record(&row));
                }
            }
            capture.verify_files(&records)?;
            sqlx::query("UPDATE file_store_artifact_capture SET revision=$3,file_ids_json=$4 WHERE workspace_id=$1 AND session_id=$2")
                .bind(&capture.workspace_id).bind(&capture.session_id).bind(awaken_sql_storage::StoredU64::try_from(capture.revision).map_err(ce)?.database_value())
                .bind(serde_json::to_string(&capture.file_ids).map_err(ce)?)
                .execute(&mut *transaction).await.map_err(ce)?;
        } else if revision == 0 {
            // A conflicting first observation must not persist the private seed.
            return Ok(outcome);
        }
        transaction.commit().await.map_err(ce)?;
        Ok(outcome)
    }

    async fn create_file(
        &self,
        record: FileRecord,
    ) -> Result<CreateFileRecordOutcome, FileCatalogError> {
        crate::validate_record(&record)?;
        let mut transaction = self.pool.begin().await.map_err(ce)?;
        if let Some(key) = record.harvest_key.as_deref() {
            let lock_key = format!(
                "file-store:{}:{}:{key}",
                record.workspace_id.len(),
                record.workspace_id
            );
            sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
                .bind(lock_key)
                .execute(&mut *transaction)
                .await
                .map_err(ce)?;
            let matching =
                harvest_records_for_update(&mut transaction, &record.workspace_id, key).await?;
            if record.artifact_idempotency_scope.is_some() {
                if let Some(outcome) =
                    persist_artifact_association(&mut transaction, &matching, &record).await?
                {
                    transaction.commit().await.map_err(ce)?;
                    return Ok(outcome);
                }
                if record.deleted {
                    return Err(FileCatalogError::Invalid(
                        "terminal association cannot recreate a missing tombstone".into(),
                    ));
                }
            } else if let Some(existing) = matching.into_iter().find(|row| !row.deleted) {
                if !same_harvest_identity(&existing, &record) {
                    return Err(FileCatalogError::Invalid(
                        "artifact harvest key is bound to different File identity".into(),
                    ));
                }
                transaction.commit().await.map_err(ce)?;
                return Ok(CreateFileRecordOutcome::Existing(existing));
            }
        }
        let inserted = sqlx::query(&format!(
            "INSERT INTO file_store_file \
             (id,workspace_id,blob_id,filename,mime_type,size_bytes,created_at,expires_at,downloadable,scope_id,logical_path,harvest_key,artifact_idempotency_scope,deleted) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14) \
             ON CONFLICT DO NOTHING RETURNING {FILE_COLUMNS}"
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
        .fetch_optional(&mut *transaction)
        .await
        .map_err(ce)?;
        if let Some(row) = inserted {
            let inserted = row_record(&row);
            transaction.commit().await.map_err(ce)?;
            return Ok(CreateFileRecordOutcome::Inserted(inserted));
        }
        let existing = if let Some(key) = record.harvest_key.as_deref() {
            let matching =
                harvest_records_for_update(&mut transaction, &record.workspace_id, key).await?;
            if record.artifact_idempotency_scope.is_some()
                && let Some(outcome) =
                    persist_artifact_association(&mut transaction, &matching, &record).await?
            {
                transaction.commit().await.map_err(ce)?;
                return Ok(outcome);
            }
            let existing = matching
                .into_iter()
                .find(|row| !row.deleted)
                .ok_or_else(|| {
                    FileCatalogError::Invalid(
                        "artifact File conflict has no canonical active record".into(),
                    )
                })?;
            if !same_harvest_identity(&existing, &record) {
                return Err(FileCatalogError::Invalid(
                    "artifact harvest key is bound to different File identity".into(),
                ));
            }
            existing
        } else {
            return Err(FileCatalogError::Invalid(
                format!("file id `{}` already exists", record.id).into(),
            ));
        };
        transaction.commit().await.map_err(ce)?;
        Ok(CreateFileRecordOutcome::Existing(existing))
    }

    async fn get_file(
        &self,
        workspace_id: &str,
        file_id: &str,
        include_deleted: bool,
    ) -> Result<Option<FileRecord>, FileCatalogError> {
        sqlx::query(&format!(
            "SELECT {FILE_COLUMNS} FROM file_store_file \
             WHERE id=$1 AND workspace_id=$2 AND ($3 OR deleted=0)"
        ))
        .bind(file_id)
        .bind(workspace_id)
        .bind(include_deleted)
        .fetch_optional(&self.pool)
        .await
        .map(|row| row.map(|row| row_record(&row)))
        .map_err(ce)
    }

    async fn list_files(
        &self,
        workspace_id: &str,
        scope_id: Option<&str>,
    ) -> Result<Vec<FileRecord>, FileCatalogError> {
        let rows = sqlx::query(&format!(
            "SELECT {FILE_COLUMNS} FROM file_store_file \
             WHERE workspace_id=$1 AND deleted=0 AND ($2::TEXT IS NULL OR scope_id=$2) \
             ORDER BY created_at DESC, id DESC"
        ))
        .bind(workspace_id)
        .bind(scope_id)
        .fetch_all(&self.pool)
        .await
        .map_err(ce)?;
        Ok(rows.iter().map(row_record).collect())
    }

    async fn list_files_including_deleted(
        &self,
        workspace_id: &str,
        scope_id: Option<&str>,
    ) -> Result<Vec<FileRecord>, FileCatalogError> {
        let rows = sqlx::query(&format!(
            "SELECT {FILE_COLUMNS} FROM file_store_file \
             WHERE workspace_id=$1 AND ($2::TEXT IS NULL OR scope_id=$2) \
             ORDER BY created_at DESC, id DESC"
        ))
        .bind(workspace_id)
        .bind(scope_id)
        .fetch_all(&self.pool)
        .await
        .map_err(ce)?;
        Ok(rows.iter().map(row_record).collect())
    }

    async fn mark_file_deleted(
        &self,
        workspace_id: &str,
        file_id: &str,
    ) -> Result<Option<FileRecord>, FileCatalogError> {
        sqlx::query(&format!(
            "UPDATE file_store_file SET deleted=1 WHERE id=$1 AND workspace_id=$2 \
             RETURNING {FILE_COLUMNS}"
        ))
        .bind(file_id)
        .bind(workspace_id)
        .fetch_optional(&self.pool)
        .await
        .map(|row| row.map(|row| row_record(&row)))
        .map_err(ce)
    }

    async fn active_size_bytes(&self, workspace_id: &str) -> Result<u64, FileCatalogError> {
        sqlx::query(
            "SELECT COALESCE(SUM(size_bytes),0)::BIGINT AS total FROM file_store_file \
             WHERE workspace_id=$1 AND deleted=0",
        )
        .bind(workspace_id)
        .fetch_one(&self.pool)
        .await
        .map(|row| row.get::<i64, _>("total") as u64)
        .map_err(ce)
    }
}
