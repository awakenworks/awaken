// @formal surface requirement=resource.files.resource_lifecycle
// @formal reason: This source implements the repository-controlled files product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Logical File commands owned by the canonical Resources application.
//!
//! This is the sole command path which coordinates immutable bytes, public File
//! metadata, Workspace/reference ownership, quotas, and logical deletion. HTTP,
//! Runtime artifact harvesting, and other driving adapters reuse this service;
//! none of them may reproduce this ordering over the repositories.

use std::sync::Arc;

mod deletion;

use awaken_resource_contract::{
    ArtifactCapture, ArtifactPublication, CreateFileRecordOutcome, FileApplicationService,
    FileCatalog, FileCatalogError, FileRecord, FileStore, MAX_MANAGED_FILE_SIZE_BYTES,
    MAX_WORKSPACE_FILE_BYTES, ReplaceArtifactCapture, ReplaceArtifactCaptureOutcome, ResourceKind,
    ResourcePurgeError, ResourceReclamationRepository, ResourceReference, ResourceReferenceKind,
    ResourceReferenceRecord, ResourceTarget, content_id,
};

/// Complete logical-File creation command. Public uploads, generated outputs,
/// and Session artifact harvesting differ only in these declared facts.
pub struct CreateFileCommand<'a> {
    pub workspace_id: &'a str,
    pub filename: String,
    pub mime_type: String,
    pub bytes: &'a [u8],
    pub downloadable: bool,
    pub expires_at: Option<String>,
    pub scope_id: Option<String>,
    pub logical_path: Option<String>,
    pub idempotency_key: Option<String>,
}

#[derive(Clone)]
pub struct FileApplication {
    store: Arc<dyn FileStore>,
    catalog: Arc<dyn FileCatalog>,
    reclamation: Arc<dyn ResourceReclamationRepository>,
    /// Stamps `created_at`; the Files API computes `expires_at` from the same
    /// Resources clock, so one File record never mixes two timelines.
    clock: Arc<dyn awaken_time::WallClock>,
}

impl FileApplication {
    #[must_use]
    pub fn new(
        store: Arc<dyn FileStore>,
        catalog: Arc<dyn FileCatalog>,
        reclamation: Arc<dyn ResourceReclamationRepository>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self {
            store,
            catalog,
            reclamation,
            clock,
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

    pub async fn list_including_deleted(
        &self,
        workspace_id: &str,
        scope_id: Option<&str>,
    ) -> Result<Vec<FileRecord>, ResourcePurgeError> {
        self.catalog
            .list_files_including_deleted(workspace_id, scope_id)
            .await
            .map_err(file_catalog_error)
    }

    pub async fn create(
        &self,
        command: CreateFileCommand<'_>,
    ) -> Result<FileRecord, ResourcePurgeError> {
        self.create_with_artifact_scope(command, None).await
    }

    async fn create_with_artifact_scope(
        &self,
        command: CreateFileCommand<'_>,
        artifact_idempotency_scope: Option<String>,
    ) -> Result<FileRecord, ResourcePurgeError> {
        if let Some(key) = command.idempotency_key.as_deref() {
            let records = if artifact_idempotency_scope.is_some() {
                self.catalog
                    .list_files_including_deleted(command.workspace_id, command.scope_id.as_deref())
                    .await
            } else {
                self.catalog
                    .list_files(command.workspace_id, command.scope_id.as_deref())
                    .await
            }
            .map_err(file_catalog_error)?;
            if let Some(mut existing) = records
                .into_iter()
                .find(|record| record.harvest_key.as_deref() == Some(key))
            {
                if let Some(scope) = artifact_idempotency_scope.as_deref() {
                    if !same_file_effect_ignoring_lifecycle(&existing, &command) {
                        return Err(ResourcePurgeError::IdempotencyConflict(key.to_owned()));
                    }
                    // `create_file` is the single CAS owner for terminal
                    // association. Reusing its existing row here keeps the
                    // decision ahead of quota/blob/reference effects, including
                    // when the only durable evidence is a tombstone.
                    existing.artifact_idempotency_scope = Some(scope.to_string());
                    return self
                        .catalog
                        .create_file(existing)
                        .await
                        .map(|outcome| outcome.record().clone())
                        .map_err(file_catalog_error);
                }
                return if same_file_effect(&existing, &command) {
                    Ok(existing)
                } else {
                    Err(ResourcePurgeError::IdempotencyConflict(key.to_owned()))
                };
            }
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
            .map_err(ResourcePurgeError::from)?;
        let candidate = FileRecord {
            id: format!("file_{}", uuid::Uuid::new_v4().simple()),
            workspace_id: command.workspace_id.to_string(),
            blob_id,
            filename: command.filename,
            mime_type: command.mime_type,
            size_bytes: command.bytes.len() as u64,
            created_at: self.clock.now().to_rfc3339_secs(),
            expires_at: command.expires_at,
            downloadable: command.downloadable,
            scope_id: command.scope_id,
            logical_path: command.logical_path,
            harvest_key: command.idempotency_key,
            artifact_idempotency_scope,
            deleted: false,
        };
        let candidate_reference = logical_file_reference(&candidate);
        self.reclamation
            .add_reference(candidate_reference.clone())
            .await?;
        let outcome = match self.catalog.create_file(candidate.clone()).await {
            Ok(outcome) => outcome,
            Err(error) => {
                let _ = self
                    .reclamation
                    .remove_reference(&candidate_reference)
                    .await;
                return Err(file_catalog_error(error));
            }
        };
        match outcome {
            CreateFileRecordOutcome::Inserted(record) => Ok(record),
            CreateFileRecordOutcome::Existing(record) => {
                self.reclamation
                    .remove_reference(&candidate_reference)
                    .await?;
                let same_effect = if candidate.artifact_idempotency_scope.is_some() {
                    same_file_record_effect_ignoring_lifecycle(&record, &candidate)
                } else {
                    same_file_record_effect(&record, &candidate)
                };
                if !same_effect {
                    return Err(ResourcePurgeError::IdempotencyConflict(
                        candidate.harvest_key.unwrap_or_default(),
                    ));
                }
                if !record.deleted {
                    self.reclamation
                        .add_reference(logical_file_reference(&record))
                        .await?;
                }
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
        self.create_uploaded_file_with_expiry(workspace_id, filename, mime_type, bytes, None)
            .await
    }

    pub async fn create_uploaded_file_with_expiry(
        &self,
        workspace_id: &str,
        filename: String,
        mime_type: String,
        bytes: &[u8],
        expires_at: Option<String>,
    ) -> Result<FileRecord, ResourcePurgeError> {
        self.create_uploaded_file_with_expiry_and_idempotency(
            workspace_id,
            filename,
            mime_type,
            bytes,
            expires_at,
            None,
        )
        .await
    }

    pub async fn create_uploaded_file_with_expiry_and_idempotency(
        &self,
        workspace_id: &str,
        filename: String,
        mime_type: String,
        bytes: &[u8],
        expires_at: Option<String>,
        idempotency_key: Option<String>,
    ) -> Result<FileRecord, ResourcePurgeError> {
        self.create(CreateFileCommand {
            workspace_id,
            filename,
            mime_type,
            bytes,
            downloadable: false,
            expires_at,
            scope_id: None,
            logical_path: None,
            idempotency_key,
        })
        .await
    }

    pub async fn create_artifact(
        &self,
        publication: &ArtifactPublication<()>,
    ) -> Result<FileRecord, ResourcePurgeError> {
        publication
            .verify()
            .map_err(|error| ResourcePurgeError::InvalidIntent {
                detail: error.to_string(),
            })?;
        self.create_with_artifact_scope(
            CreateFileCommand {
                workspace_id: &publication.workspace_id,
                filename: publication.logical_path.clone(),
                mime_type: publication.mime_type.clone(),
                bytes: &publication.bytes,
                downloadable: true,
                expires_at: None,
                scope_id: Some(publication.session_id.clone()),
                logical_path: Some(publication.logical_path.clone()),
                idempotency_key: Some(publication.effect_id.clone()),
            },
            publication.idempotency_scope.clone(),
        )
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
            .map_err(ResourcePurgeError::from)?
            .ok_or_else(|| ResourcePurgeError::CorruptData {
                detail: format!(
                    "file `{file_id}` references missing blob `{}`",
                    record.blob_id
                ),
            })?;
        Ok(Some((record, bytes)))
    }
}

#[async_trait::async_trait]
impl FileApplicationService for FileApplication {
    async fn artifact_capture(
        &self,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<Option<ArtifactCapture>, ResourcePurgeError> {
        self.catalog
            .artifact_capture(workspace_id, session_id)
            .await
            .map_err(file_catalog_error)
    }

    async fn replace_artifact_capture(
        &self,
        request: ReplaceArtifactCapture,
    ) -> Result<ReplaceArtifactCaptureOutcome, ResourcePurgeError> {
        self.catalog
            .replace_artifact_capture(request)
            .await
            .map_err(file_catalog_error)
    }

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

    async fn list_including_deleted(
        &self,
        workspace_id: &str,
        scope_id: Option<&str>,
    ) -> Result<Vec<FileRecord>, ResourcePurgeError> {
        FileApplication::list_including_deleted(self, workspace_id, scope_id).await
    }

    async fn create_uploaded_file_with_expiry_and_idempotency(
        &self,
        workspace_id: &str,
        filename: String,
        mime_type: String,
        bytes: &[u8],
        expires_at: Option<String>,
        idempotency_key: Option<String>,
    ) -> Result<FileRecord, ResourcePurgeError> {
        FileApplication::create_uploaded_file_with_expiry_and_idempotency(
            self,
            workspace_id,
            filename,
            mime_type,
            bytes,
            expires_at,
            idempotency_key,
        )
        .await
    }

    async fn create_artifact(
        &self,
        publication: &ArtifactPublication<()>,
    ) -> Result<FileRecord, ResourcePurgeError> {
        FileApplication::create_artifact(self, publication).await
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

fn same_file_effect(existing: &FileRecord, command: &CreateFileCommand<'_>) -> bool {
    !existing.deleted && same_file_effect_ignoring_lifecycle(existing, command)
}

fn same_file_effect_ignoring_lifecycle(
    existing: &FileRecord,
    command: &CreateFileCommand<'_>,
) -> bool {
    existing.workspace_id == command.workspace_id
        && existing.blob_id == content_id(command.bytes)
        && existing.filename == command.filename
        && existing.mime_type == command.mime_type
        && existing.size_bytes == command.bytes.len() as u64
        && existing.downloadable == command.downloadable
        && existing.expires_at == command.expires_at
        && existing.scope_id == command.scope_id
        && existing.logical_path == command.logical_path
        && existing.harvest_key == command.idempotency_key
}

fn same_file_record_effect(existing: &FileRecord, candidate: &FileRecord) -> bool {
    !existing.deleted && same_file_record_effect_ignoring_lifecycle(existing, candidate)
}

fn same_file_record_effect_ignoring_lifecycle(
    existing: &FileRecord,
    candidate: &FileRecord,
) -> bool {
    existing.workspace_id == candidate.workspace_id
        && existing.blob_id == candidate.blob_id
        && existing.filename == candidate.filename
        && existing.mime_type == candidate.mime_type
        && existing.size_bytes == candidate.size_bytes
        && existing.downloadable == candidate.downloadable
        && existing.expires_at == candidate.expires_at
        && existing.scope_id == candidate.scope_id
        && existing.logical_path == candidate.logical_path
        && existing.harvest_key == candidate.harvest_key
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
        FileCatalogError::InvalidRecord { detail }
        | FileCatalogError::InvalidCapture { detail } => {
            ResourcePurgeError::InvalidIntent { detail }
        }
        FileCatalogError::IdentityConflict { detail }
        | FileCatalogError::AssociationConflict { detail } => {
            ResourcePurgeError::IdempotencyConflict(detail)
        }
        error => ResourcePurgeError::Catalog(error),
    }
}

fn validate_file_capacity(file_size: u64, active_size: u64) -> Result<(), ResourcePurgeError> {
    if file_size > MAX_MANAGED_FILE_SIZE_BYTES {
        return Err(ResourcePurgeError::LimitExceeded {
            detail: format!("file exceeds the {MAX_MANAGED_FILE_SIZE_BYTES} byte limit"),
        });
    }
    if active_size.saturating_add(file_size) > MAX_WORKSPACE_FILE_BYTES {
        return Err(ResourcePurgeError::LimitExceeded {
            detail: format!("Workspace files exceed the {MAX_WORKSPACE_FILE_BYTES} byte limit"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_resource_contract::{content_id, harvest_idempotency_key};

    pub(super) fn application() -> FileApplication {
        let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
        FileApplication::new(
            files.clone(),
            files,
            Arc::new(
                awaken_resource_store::SqliteResourceStore::in_memory()
                    .expect("resource reclamation"),
            ),
            awaken_time::system_clock(),
        )
    }

    pub(super) fn artifact_publication(
        logical_path: &str,
        bytes: &[u8],
        idempotency_scope: Option<&str>,
    ) -> ArtifactPublication<()> {
        let content_id = content_id(bytes);
        ArtifactPublication {
            effect_id: harvest_idempotency_key("session-a", logical_path, &content_id),
            workspace_id: "workspace-a".into(),
            session_id: "session-a".into(),
            logical_path: logical_path.into(),
            mime_type: "text/plain".into(),
            content_id,
            bytes: bytes.to_vec(),
            idempotency_scope: idempotency_scope.map(str::to_owned),
            fence: None,
        }
    }

    #[tokio::test]
    async fn capture_selection_reuses_file_identity_bytes_and_deletion_authority() {
        // A1 publish A/B/A at the same path -> two immutable Files, latest
        // capture selects reused A id. A2 losing the replacement response ->
        // exact replay, no extra File/blob/reference. A3 deleting selected A ->
        // capture keeps the exact id but File download is denied; B is never
        // substituted. Application delegates CAS to its existing catalog.
        let app = application();
        let a = app
            .create_artifact(&artifact_publication("report.txt", b"A", None))
            .await
            .unwrap();
        let b = app
            .create_artifact(&artifact_publication("report.txt", b"B", None))
            .await
            .unwrap();
        let reused = app
            .create_artifact(&artifact_publication("report.txt", b"A", None))
            .await
            .unwrap();
        assert_eq!(a.id, reused.id, "A1");
        for (revision, id) in [a.id.clone(), b.id.clone(), reused.id.clone()]
            .into_iter()
            .enumerate()
        {
            let command = ReplaceArtifactCapture {
                workspace_id: "workspace-a".into(),
                session_id: "session-a".into(),
                expected_revision: revision as u64,
                file_ids: vec![id],
            };
            assert!(
                matches!(
                    app.replace_artifact_capture(command.clone()).await.unwrap(),
                    ReplaceArtifactCaptureOutcome::Applied(_)
                ),
                "A1"
            );
            assert!(
                matches!(
                    app.replace_artifact_capture(command).await.unwrap(),
                    ReplaceArtifactCaptureOutcome::Replayed(_)
                ),
                "A2"
            );
        }
        assert_eq!(
            app.list("workspace-a", Some("session-a"))
                .await
                .unwrap()
                .len(),
            2,
            "A1/A2"
        );
        assert_eq!(
            app.bytes("workspace-a", &a.id).await.unwrap().unwrap().1,
            b"A",
            "A1"
        );
        app.delete("workspace-a", &a.id, 1).await.unwrap();
        assert_eq!(
            app.artifact_capture("workspace-a", "session-a")
                .await
                .unwrap()
                .unwrap()
                .file_ids
                .as_slice(),
            std::slice::from_ref(&a.id),
            "A3"
        );
        assert!(
            app.bytes("workspace-a", &a.id).await.unwrap().is_none(),
            "A3"
        );
        assert_eq!(
            app.bytes("workspace-a", &b.id).await.unwrap().unwrap().1,
            b"B",
            "A3 history remains"
        );
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
        // C2 same bytes with a different key -> E2 create a distinct logical File;
        // C3 same key with changed effect facts -> E3 fail with an idempotency conflict
        // while the FileStore may still deduplicate its immutable blob.
        // These three rules cover command replay versus content deduplication without
        // creating a second idempotency registry.
        let app = application();
        let command = || CreateFileCommand {
            workspace_id: "workspace-a",
            filename: "report.txt".into(),
            mime_type: "text/plain".into(),
            bytes: b"report",
            downloadable: true,
            expires_at: None,
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

        let conflict = app
            .create(CreateFileCommand {
                bytes: b"changed report",
                ..command()
            })
            .await
            .unwrap_err();
        assert!(
            matches!(conflict, ResourcePurgeError::IdempotencyConflict(_)),
            "E3 {conflict}"
        );
    }

    #[tokio::test]
    async fn terminal_scope_associates_a_tombstone_before_capacity_and_blob_effects() {
        // Cause/effect decision table:
        // C1 canonical ordinary artifact is already tombstoned; C2 Workspace is
        // now at its active-byte limit; C3 terminal request has the same v1
        // harvest key and a new cleanup scope; C4 a caller substitutes a
        // noncanonical key. R1 => FileCatalog atomically
        // associates the existing tombstone before capacity/blob/reference
        // effects, preserves `deleted`, and creates no active replacement. R2
        // => reject C4 before touching the catalog or blob store.
        let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
        let app = FileApplication::new(
            files.clone(),
            files.clone(),
            Arc::new(
                awaken_resource_store::SqliteResourceStore::in_memory()
                    .expect("resource reclamation"),
            ),
            awaken_time::system_clock(),
        );
        let ordinary_publication = artifact_publication("report.txt", b"report", None);
        let ordinary = app
            .create_artifact(&ordinary_publication)
            .await
            .expect("ordinary artifact");
        app.delete("workspace-a", &ordinary.id, 7)
            .await
            .expect("tombstone ordinary artifact");
        files
            .create_file(FileRecord {
                id: "file_capacity_filler".into(),
                workspace_id: "workspace-a".into(),
                blob_id: "capacity-filler".into(),
                filename: "capacity.bin".into(),
                mime_type: "application/octet-stream".into(),
                size_bytes: MAX_WORKSPACE_FILE_BYTES,
                created_at: "2026-01-02T00:00:00Z".into(),
                expires_at: None,
                downloadable: false,
                scope_id: None,
                logical_path: None,
                harvest_key: None,
                artifact_idempotency_scope: None,
                deleted: false,
            })
            .await
            .expect("fill active-byte capacity");

        let terminal_publication =
            artifact_publication("report.txt", b"report", Some("cleanup-current"));
        let terminal = app
            .create_artifact(&terminal_publication)
            .await
            .expect("R1 associates before capacity validation");
        assert_eq!(terminal.id, ordinary.id, "R1 preserves File identity");
        assert!(terminal.deleted, "R1 does not resurrect the tombstone");
        assert_eq!(
            terminal.artifact_idempotency_scope.as_deref(),
            Some("cleanup-current"),
            "R1 persists the exact terminal association"
        );
        assert_eq!(
            app.list("workspace-a", Some("session-a"))
                .await
                .unwrap()
                .len(),
            0,
            "R1 creates no active replacement"
        );
        let mut substituted = terminal_publication;
        substituted.effect_id = "substituted-key".into();
        assert!(
            app.create_artifact(&substituted).await.is_err(),
            "R2 rejects a substituted harvest identity"
        );
    }
}
