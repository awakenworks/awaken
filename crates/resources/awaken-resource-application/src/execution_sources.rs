//! In-process adapters from Resources authorities to execution-facing ports.

use std::sync::Arc;

use awaken_agent_contract::AgentSkillKind;
use awaken_resource_contract::{
    ArtifactPublication, ArtifactPublicationError, ArtifactPublicationReceipt, ArtifactPublisher,
    ArtifactRecovery, FileApplicationService, FileContentSource, FileContentSourceError,
    FileReadPurpose, LiveResourceBindingVerifier, RepositoryBindingVerifier,
    RepositoryBindingVerifierError, ResolvedFileContent, SkillStore, SkillVersion, content_id,
};
use awaken_session_contract::{
    ResolvedSkillBinding, SkillBundleSource, SkillBundleSourceError, SkillCatalogApplication,
    validate_skill_bundle,
};

pub struct ApplicationFileContentSource {
    application: Arc<dyn FileApplicationService>,
}

impl ApplicationFileContentSource {
    #[must_use]
    pub fn new(application: Arc<dyn FileApplicationService>) -> Self {
        Self { application }
    }
}

#[async_trait::async_trait]
impl<C: Sync> FileContentSource<C> for ApplicationFileContentSource {
    async fn read(
        &self,
        workspace_id: &str,
        file_id: &str,
        _purpose: &FileReadPurpose,
        _fence: Option<&C>,
    ) -> Result<Option<ResolvedFileContent>, FileContentSourceError> {
        let Some((record, bytes)) = self
            .application
            .bytes(workspace_id, file_id)
            .await
            .map_err(|error| FileContentSourceError::new(error.to_string()))?
        else {
            return Ok(None);
        };
        let actual = content_id(&bytes);
        if actual != record.blob_id {
            return Err(FileContentSourceError::new(format!(
                "File `{file_id}` content digest mismatch: expected {}, received {actual}",
                record.blob_id
            )));
        }
        Ok(Some(ResolvedFileContent {
            file_id: record.id,
            content_id: record.blob_id,
            filename: record.filename,
            media_type: record.mime_type,
            bytes,
        }))
    }
}

pub struct ApplicationArtifactPublisher {
    application: Arc<dyn FileApplicationService>,
}

impl ApplicationArtifactPublisher {
    #[must_use]
    pub fn new(application: Arc<dyn FileApplicationService>) -> Self {
        Self { application }
    }
}

#[async_trait::async_trait]
impl<C: Send + Sync + 'static> ArtifactPublisher<C> for ApplicationArtifactPublisher {
    async fn publish(
        &self,
        publication: ArtifactPublication<C>,
    ) -> Result<ArtifactPublicationReceipt, ArtifactPublicationError> {
        publication.verify()?;
        let file_publication = publication.into_file_application();
        let record = self
            .application
            .create_artifact(&file_publication)
            .await
            .map_err(|error| ArtifactPublicationError::new(error.to_string()))?;
        ArtifactPublicationReceipt::from_publication_record(&file_publication, record)
    }

    async fn recover(
        &self,
        recovery: ArtifactRecovery<C>,
    ) -> Result<Vec<ArtifactPublicationReceipt>, ArtifactPublicationError> {
        recovery.verify()?;
        let records = self
            .application
            .list_including_deleted(&recovery.workspace_id, Some(&recovery.session_id))
            .await
            .map_err(|error| ArtifactPublicationError::new(error.to_string()))?;
        recovery.receipts_from_records(records)
    }
}

pub struct RegistryRepositoryBindingVerifier {
    verifier: Arc<dyn LiveResourceBindingVerifier>,
}

impl RegistryRepositoryBindingVerifier {
    #[must_use]
    pub fn new(verifier: Arc<dyn LiveResourceBindingVerifier>) -> Self {
        Self { verifier }
    }
}

#[async_trait::async_trait]
impl<C: Sync> RepositoryBindingVerifier<C> for RegistryRepositoryBindingVerifier {
    async fn verify(
        &self,
        workspace_id: &str,
        repository_id: &str,
        config_version: awaken_resource_contract::ConfigVersion,
        _fence: Option<&C>,
    ) -> Result<awaken_resource_contract::RepositoryTransport, RepositoryBindingVerifierError> {
        self.verifier
            .verify_repository_binding(workspace_id, repository_id, config_version)
            .map_err(|error| RepositoryBindingVerifierError::new(error.to_string()))?;
        Ok(awaken_resource_contract::RepositoryTransport::Direct)
    }
}

pub struct StoreSkillBundleSource {
    store: Arc<dyn SkillStore>,
}

pub struct StoreSkillCatalogApplication {
    store: Arc<dyn SkillStore>,
}

impl StoreSkillCatalogApplication {
    #[must_use]
    pub fn new(store: Arc<dyn SkillStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl SkillCatalogApplication for StoreSkillCatalogApplication {
    async fn resolve_custom(
        &self,
        workspace_id: &str,
        skill_id: &str,
        selector: &str,
    ) -> Result<ResolvedSkillBinding, awaken_resource_contract::SkillStoreError> {
        let ordinal = if selector == "latest" {
            self.store
                .definition(workspace_id, skill_id)
                .await?
                .ok_or_else(|| {
                    awaken_resource_contract::SkillStoreError::NotFound(skill_id.into())
                })?
                .latest_version
        } else {
            selector.parse::<u64>().map_err(|_| {
                awaken_resource_contract::SkillStoreError::Invalid(
                    "invalid Skill version selector".into(),
                )
            })?
        };
        let version = self
            .store
            .version(workspace_id, skill_id, ordinal)
            .await?
            .ok_or_else(|| awaken_resource_contract::SkillStoreError::NotFound(skill_id.into()))?;
        Ok(ResolvedSkillBinding {
            kind: AgentSkillKind::Custom,
            skill_id: skill_id.to_owned(),
            version: version.version,
            bundle_sha256: version.bundle_sha256,
        })
    }

    async fn snapshot_latest(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<SkillVersion>, awaken_resource_contract::SkillStoreError> {
        self.store.snapshot_latest_versions(workspace_id).await
    }

    async fn publish_authored(
        &self,
        workspace_id: &str,
        raw_id: &str,
        name: &str,
        description: &str,
        content: &str,
    ) -> Result<(), awaken_resource_contract::SkillStoreError> {
        let id = awaken_resource_contract::skill_stem(raw_id);
        let existing = self.store.definition(workspace_id, &id).await?;
        let next = existing
            .as_ref()
            .map_or(1, |definition| definition.latest_version + 1);
        if let Some(definition) = &existing
            && self
                .store
                .version(workspace_id, &id, definition.latest_version)
                .await?
                .is_some_and(|latest| {
                    latest
                        .skill_md()
                        .is_some_and(|bytes| bytes == content.as_bytes())
                })
        {
            return Ok(());
        }
        let files = vec![awaken_resource_contract::SkillBundleFile {
            path: "SKILL.md".into(),
            content: content.as_bytes().to_vec(),
            executable: false,
        }];
        let created_unix_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64)
            .unwrap_or_default();
        let version = SkillVersion {
            id: format!("skver_{id}_{next}").into(),
            skill_id: id.clone().into(),
            version: next,
            name: name.to_owned(),
            description: description.to_owned(),
            directory: format!("/skills/{id}"),
            bundle_sha256: awaken_resource_contract::skill_bundle_sha256(&files),
            files,
            created_unix_nanos,
        };
        if existing.is_some() {
            self.store.append_version(workspace_id, &id, version).await
        } else {
            self.store
                .create(
                    awaken_resource_contract::SkillDefinition {
                        id: id.into(),
                        workspace_id: workspace_id.to_owned(),
                        display_title: None,
                        latest_version: 1,
                        last_version: 1,
                        timestamps: awaken_resource_contract::ResourceTimestamps::created(
                            created_unix_nanos,
                        ),
                    },
                    version,
                )
                .await
        }
    }
}

impl StoreSkillBundleSource {
    #[must_use]
    pub fn new(store: Arc<dyn SkillStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl<C: Sync> SkillBundleSource<C> for StoreSkillBundleSource {
    async fn load(
        &self,
        workspace_id: &str,
        binding: &ResolvedSkillBinding,
        _fence: Option<&C>,
    ) -> Result<Option<SkillVersion>, SkillBundleSourceError> {
        if binding.kind != AgentSkillKind::Custom {
            return Err(SkillBundleSourceError::new(
                "only custom Skills are stored in the Resources context",
            ));
        }
        self.store
            .version(workspace_id, &binding.skill_id, binding.version)
            .await
            .map_err(|error| SkillBundleSourceError::new(error.to_string()))?
            .map(|version| validate_skill_bundle(workspace_id, workspace_id, binding, version))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use awaken_resource_contract::{
        ArtifactPublication, ArtifactPublisher as _, ArtifactRecovery, content_id,
        harvest_idempotency_key,
    };

    #[tokio::test]
    async fn missing_file_application_fails_closed() {
        // Cause/effect graph: C1 no File application was injected; C2 runtime
        // requests immutable bytes. Decision U1: C1+C2 -> explicit error, with
        // no fallback to FileCatalog/FileStore or process-local state.
        let error = <awaken_resource_contract::UnavailableFileContentSource as
            awaken_resource_contract::FileContentSource<()>>::read(
                &awaken_resource_contract::UnavailableFileContentSource,
                "workspace",
                "file",
                &awaken_resource_contract::FileReadPurpose::SessionResource,
                None,
            )
            .await
            .expect_err("U1");
        assert!(error.to_string().contains("not configured"), "U1: {error}");
    }

    #[tokio::test]
    async fn artifact_terminal_association_reuses_one_file_and_recovers_through_delete() {
        // Cause/effect decision table:
        // A1 ordinary canonical File + terminal scope => atomically associate
        // the same row and return the same File id; A2 exact terminal replay =>
        // same receipt; A3 logical delete + response loss => include-deleted
        // readback returns the byte-identical creation receipt; A4 ordinary or
        // foreign terminal scope => filtered from current recovery.
        let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
        let application = Arc::new(crate::FileApplication::new(
            files.clone(),
            files,
            Arc::new(
                awaken_resource_store::SqliteResourceStore::in_memory()
                    .expect("resource lifecycle"),
            ),
        ));
        let publisher = super::ApplicationArtifactPublisher::new(application.clone());
        let bytes = b"durable terminal artifact".to_vec();
        let digest = content_id(&bytes);
        let effect_id = harvest_idempotency_key("session-a", "report.txt", &digest);
        let publication = |idempotency_scope: Option<&str>| ArtifactPublication {
            effect_id: effect_id.clone(),
            workspace_id: "workspace-a".into(),
            session_id: "session-a".into(),
            logical_path: "report.txt".into(),
            mime_type: "text/plain".into(),
            content_id: digest.clone(),
            bytes: bytes.clone(),
            idempotency_scope: idempotency_scope.map(str::to_string),
            fence: None::<()>,
        };

        let ordinary = publisher
            .publish(publication(None))
            .await
            .expect("A1 ordinary");
        let terminal = publisher
            .publish(publication(Some("cleanup-current")))
            .await
            .expect("A1 terminal association");
        assert_eq!(terminal.record.id, ordinary.record.id, "A1 one File row");
        assert_eq!(
            terminal.record.artifact_idempotency_scope.as_deref(),
            Some("cleanup-current"),
            "A1 durable association"
        );
        assert_eq!(
            publisher
                .publish(publication(Some("cleanup-current")))
                .await
                .expect("A2 replay"),
            terminal,
            "A2"
        );

        application
            .delete("workspace-a", &terminal.record.id, 7)
            .await
            .expect("A3 delete");
        let recovered = publisher
            .recover(ArtifactRecovery {
                workspace_id: "workspace-a".into(),
                session_id: "session-a".into(),
                idempotency_scope: "cleanup-current".into(),
                fence: (),
            })
            .await
            .expect("A3 readback");
        assert_eq!(
            recovered,
            vec![terminal],
            "A3 canonical receipt survives delete"
        );
        assert!(
            publisher
                .recover(ArtifactRecovery {
                    workspace_id: "workspace-a".into(),
                    session_id: "session-a".into(),
                    idempotency_scope: "cleanup-foreign".into(),
                    fence: (),
                })
                .await
                .expect("A4 filtered readback")
                .is_empty(),
            "A4"
        );
    }
}
