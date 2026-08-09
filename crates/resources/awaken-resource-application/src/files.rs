//! Logical File commands owned by the canonical Resources application.
//!
//! This is the sole command path which coordinates immutable bytes, public File
//! metadata, Workspace/reference ownership, quotas, and logical deletion. HTTP,
//! Runtime artifact harvesting, and other driving adapters reuse this service;
//! none of them may reproduce this ordering over the repositories.

use std::sync::Arc;

use awaken_resource_contract::{
    CreateFileRecordOutcome, FileApplicationService, FileCatalog, FileCatalogError, FileRecord,
    FileStore, MAX_MANAGED_FILE_SIZE_BYTES, MAX_WORKSPACE_FILE_BYTES, ResourceKind,
    ResourceLifecycleRepository, ResourcePurgeError, ResourcePurgeIntent, ResourceReference,
    ResourceReferenceKind, ResourceReferenceRecord, ResourceTarget,
};

/// Complete logical-File creation command. Public uploads, generated outputs,
/// and Session artifact harvesting differ only in these declared facts.
pub struct CreateFileCommand<'a> {
    pub workspace_id: &'a str,
    pub filename: String,
    pub mime_type: String,
    pub bytes: &'a [u8],
    pub downloadable: bool,
    pub scope_id: Option<String>,
    pub logical_path: Option<String>,
    pub idempotency_key: Option<String>,
}

#[derive(Clone)]
pub struct FileApplication {
    store: Arc<dyn FileStore>,
    catalog: Arc<dyn FileCatalog>,
    lifecycle: Arc<dyn ResourceLifecycleRepository>,
}

impl FileApplication {
    #[must_use]
    pub fn new(
        store: Arc<dyn FileStore>,
        catalog: Arc<dyn FileCatalog>,
        lifecycle: Arc<dyn ResourceLifecycleRepository>,
    ) -> Self {
        Self {
            store,
            catalog,
            lifecycle,
        }
    }

    pub async fn get(
        &self,
        workspace_id: &str,
        file_id: &str,
    ) -> Result<Option<FileRecord>, ResourcePurgeError> {
        self.catalog
            .get_file(workspace_id, file_id, false)
            .await
            .map_err(file_catalog_error)
    }

    pub async fn list(
        &self,
        workspace_id: &str,
        scope_id: Option<&str>,
    ) -> Result<Vec<FileRecord>, ResourcePurgeError> {
        self.catalog
            .list_files(workspace_id, scope_id)
            .await
            .map_err(file_catalog_error)
    }

    pub async fn create(
        &self,
        command: CreateFileCommand<'_>,
    ) -> Result<FileRecord, ResourcePurgeError> {
        if let Some(key) = command.idempotency_key.as_deref()
            && let Some(existing) = self
                .catalog
                .list_files(command.workspace_id, command.scope_id.as_deref())
                .await
                .map_err(file_catalog_error)?
                .into_iter()
                .find(|record| record.harvest_key.as_deref() == Some(key))
        {
            return Ok(existing);
        }

        let active = self
            .catalog
            .active_size_bytes(command.workspace_id)
            .await
            .map_err(file_catalog_error)?;
        validate_file_capacity(command.bytes.len() as u64, active)?;
        let blob_id = self
            .store
            .put(command.bytes)
            .await
            .map_err(|error| ResourcePurgeError::Storage(error.to_string()))?;
        let candidate = FileRecord {
            id: format!("file_{}", uuid::Uuid::new_v4().simple()),
            workspace_id: command.workspace_id.to_string(),
            blob_id,
            filename: command.filename,
            mime_type: command.mime_type,
            size_bytes: command.bytes.len() as u64,
            created_at: now_rfc3339(),
            downloadable: command.downloadable,
            scope_id: command.scope_id,
            logical_path: command.logical_path,
            harvest_key: command.idempotency_key,
            deleted: false,
        };
        let candidate_reference = logical_file_reference(&candidate);
        self.lifecycle
            .add_reference(candidate_reference.clone())
            .await?;
        let outcome = match self.catalog.create_file(candidate).await {
            Ok(outcome) => outcome,
            Err(error) => {
                let _ = self.lifecycle.remove_reference(&candidate_reference).await;
                return Err(file_catalog_error(error));
            }
        };
        match outcome {
            CreateFileRecordOutcome::Inserted(record) => Ok(record),
            CreateFileRecordOutcome::Existing(record) => {
                self.lifecycle
                    .remove_reference(&candidate_reference)
                    .await?;
                self.lifecycle
                    .add_reference(logical_file_reference(&record))
                    .await?;
                Ok(record)
            }
        }
    }

    pub async fn create_uploaded_file(
        &self,
        workspace_id: &str,
        filename: String,
        mime_type: String,
        bytes: &[u8],
    ) -> Result<FileRecord, ResourcePurgeError> {
        self.create(CreateFileCommand {
            workspace_id,
            filename,
            mime_type,
            bytes,
            downloadable: false,
            scope_id: None,
            logical_path: None,
            idempotency_key: None,
        })
        .await
    }

    pub async fn create_generated_file(
        &self,
        workspace_id: &str,
        filename: String,
        mime_type: String,
        bytes: &[u8],
        idempotency_key: String,
    ) -> Result<FileRecord, ResourcePurgeError> {
        self.create(CreateFileCommand {
            workspace_id,
            filename,
            mime_type,
            bytes,
            downloadable: true,
            scope_id: None,
            logical_path: None,
            idempotency_key: Some(idempotency_key),
        })
        .await
    }

    pub async fn bytes(
        &self,
        workspace_id: &str,
        file_id: &str,
    ) -> Result<Option<(FileRecord, Vec<u8>)>, ResourcePurgeError> {
        let Some(record) = self.get(workspace_id, file_id).await? else {
            return Ok(None);
        };
        let bytes = self
            .store
            .get(&record.blob_id)
            .await
            .map_err(|error| ResourcePurgeError::Storage(error.to_string()))?
            .ok_or_else(|| {
                ResourcePurgeError::Storage(format!(
                    "file `{file_id}` references missing blob `{}`",
                    record.blob_id
                ))
            })?;
        Ok(Some((record, bytes)))
    }

    pub async fn delete(
        &self,
        workspace_id: &str,
        file_id: &str,
        requested_at_unix_ms: u64,
    ) -> Result<Option<FileRecord>, ResourcePurgeError> {
        let Some(record) = self.get(workspace_id, file_id).await? else {
            return Ok(None);
        };
        let target = ResourceTarget::new(workspace_id, ResourceKind::File, &record.blob_id);
        let intent = ResourcePurgeIntent::new(
            format!("purge:File:{workspace_id}:{file_id}"),
            format!("file-delete:{workspace_id}:{file_id}"),
            target,
            None,
            requested_at_unix_ms,
            requested_at_unix_ms,
        )?;
        self.lifecycle.put(intent).await?;
        let deleted = self
            .catalog
            .mark_file_deleted(workspace_id, file_id)
            .await
            .map_err(file_catalog_error)?;
        if deleted.is_some() {
            self.lifecycle
                .remove_reference(&logical_file_reference(&record))
                .await?;
        }
        Ok(deleted)
    }
}

#[async_trait::async_trait]
impl FileApplicationService for FileApplication {
    async fn get(
        &self,
        workspace_id: &str,
        file_id: &str,
    ) -> Result<Option<FileRecord>, ResourcePurgeError> {
        FileApplication::get(self, workspace_id, file_id).await
    }

    async fn list(
        &self,
        workspace_id: &str,
        scope_id: Option<&str>,
    ) -> Result<Vec<FileRecord>, ResourcePurgeError> {
        FileApplication::list(self, workspace_id, scope_id).await
    }

    async fn create_uploaded_file(
        &self,
        workspace_id: &str,
        filename: String,
        mime_type: String,
        bytes: &[u8],
    ) -> Result<FileRecord, ResourcePurgeError> {
        FileApplication::create_uploaded_file(self, workspace_id, filename, mime_type, bytes).await
    }

    async fn create_generated_file(
        &self,
        workspace_id: &str,
        filename: String,
        mime_type: String,
        bytes: &[u8],
        idempotency_key: String,
    ) -> Result<FileRecord, ResourcePurgeError> {
        FileApplication::create_generated_file(
            self,
            workspace_id,
            filename,
            mime_type,
            bytes,
            idempotency_key,
        )
        .await
    }

    async fn create_artifact(
        &self,
        workspace_id: &str,
        session_id: &str,
        logical_path: String,
        mime_type: String,
        bytes: &[u8],
        idempotency_key: String,
    ) -> Result<FileRecord, ResourcePurgeError> {
        FileApplication::create(
            self,
            CreateFileCommand {
                workspace_id,
                filename: logical_path.clone(),
                mime_type,
                bytes,
                downloadable: true,
                scope_id: Some(session_id.to_string()),
                logical_path: Some(logical_path),
                idempotency_key: Some(idempotency_key),
            },
        )
        .await
    }

    async fn bytes(
        &self,
        workspace_id: &str,
        file_id: &str,
    ) -> Result<Option<(FileRecord, Vec<u8>)>, ResourcePurgeError> {
        FileApplication::bytes(self, workspace_id, file_id).await
    }

    async fn delete(
        &self,
        workspace_id: &str,
        file_id: &str,
        requested_at_unix_ms: u64,
    ) -> Result<Option<FileRecord>, ResourcePurgeError> {
        FileApplication::delete(self, workspace_id, file_id, requested_at_unix_ms).await
    }
}

fn logical_file_reference(record: &FileRecord) -> ResourceReferenceRecord {
    ResourceReferenceRecord {
        target: ResourceTarget::new(&record.workspace_id, ResourceKind::File, &record.blob_id),
        reference: ResourceReference {
            kind: if record.scope_id.is_some() {
                ResourceReferenceKind::Artifact
            } else {
                ResourceReferenceKind::WorkspaceOwnership
            },
            reference_id: record.id.clone(),
        },
    }
}

fn file_catalog_error(error: FileCatalogError) -> ResourcePurgeError {
    match error {
        FileCatalogError::Invalid(message) => ResourcePurgeError::Invalid(message),
        FileCatalogError::Storage(message) => ResourcePurgeError::Storage(message),
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn validate_file_capacity(file_size: u64, active_size: u64) -> Result<(), ResourcePurgeError> {
    if file_size > MAX_MANAGED_FILE_SIZE_BYTES {
        return Err(ResourcePurgeError::Invalid(format!(
            "file exceeds the {MAX_MANAGED_FILE_SIZE_BYTES} byte limit"
        )));
    }
    if active_size.saturating_add(file_size) > MAX_WORKSPACE_FILE_BYTES {
        return Err(ResourcePurgeError::Invalid(format!(
            "Workspace files exceed the {MAX_WORKSPACE_FILE_BYTES} byte limit"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn application() -> FileApplication {
        let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
        FileApplication::new(
            files.clone(),
            files,
            Arc::new(
                awaken_resource_store::SqliteResourceStore::in_memory()
                    .expect("resource lifecycle"),
            ),
        )
    }

    #[test]
    fn managed_file_capacity_decision_boundaries_are_exact() {
        // Cause/effect decision table: C1 file size at/over 500 MiB and C2
        // Workspace active bytes plus the file at/over 500 GiB. R1/R2 exact
        // inclusive boundaries pass; R3/R4 one byte over fails.
        assert!(
            validate_file_capacity(MAX_MANAGED_FILE_SIZE_BYTES, 0).is_ok(),
            "R1"
        );
        assert!(
            validate_file_capacity(MAX_MANAGED_FILE_SIZE_BYTES + 1, 0).is_err(),
            "R3"
        );
        assert!(
            validate_file_capacity(1, MAX_WORKSPACE_FILE_BYTES - 1).is_ok(),
            "R2"
        );
        assert!(
            validate_file_capacity(1, MAX_WORKSPACE_FILE_BYTES).is_err(),
            "R4"
        );
    }

    #[tokio::test]
    async fn decision_table_keeps_one_logical_file_per_idempotency_key() {
        // Cause/effect design:
        // C1 same Workspace/scope/key retried -> E1 return the original logical File;
        // C2 same bytes with a different key -> E2 create a distinct logical File
        // while the FileStore may still deduplicate its immutable blob.
        // These two rules cover command replay versus content deduplication without
        // creating a second idempotency registry.
        let app = application();
        let command = || CreateFileCommand {
            workspace_id: "workspace-a",
            filename: "report.txt".into(),
            mime_type: "text/plain".into(),
            bytes: b"report",
            downloadable: true,
            scope_id: Some("session-a".into()),
            logical_path: Some("report.txt".into()),
            idempotency_key: Some("session-a/report".into()),
        };
        let first = app.create(command()).await.unwrap();
        let replay = app.create(command()).await.unwrap();
        assert_eq!(replay.id, first.id, "E1");

        let distinct = app
            .create(CreateFileCommand {
                idempotency_key: Some("session-b/report".into()),
                ..command()
            })
            .await
            .unwrap();
        assert_ne!(distinct.id, first.id, "E2");
        assert_eq!(distinct.blob_id, first.blob_id, "E2");
    }

    #[tokio::test]
    async fn decision_table_logical_delete_denies_reads_without_deleting_bytes_inline() {
        // Cause/effect design:
        // C1 active logical File -> E1 metadata and bytes are readable;
        // C2 delete command -> E2 tombstone + purge intent are durable before the
        // ownership reference is removed;
        // C3 post-delete read -> E3 ordinary reads fail closed while physical blob
        // reclamation remains an independent, retryable lifecycle operation.
        let app = application();
        let file = app
            .create_uploaded_file("workspace-a", "input.txt".into(), "text/plain".into(), b"x")
            .await
            .unwrap();
        assert!(
            app.bytes("workspace-a", &file.id).await.unwrap().is_some(),
            "E1"
        );
        assert!(
            app.delete("workspace-a", &file.id, 7)
                .await
                .unwrap()
                .is_some(),
            "E2"
        );
        assert!(
            app.bytes("workspace-a", &file.id).await.unwrap().is_none(),
            "E3"
        );
    }
}
