// @formal entrypoint skill.execution_pin_binds_exact_workspace_revision_and_hash
// @formal entrypoint skill.invalid_workspace_or_hash_fails_closed
// @formal entrypoint skill.loaded_version_equals_session_pin
//! In-process adapters from Resources authorities to execution-facing ports.

use std::sync::Arc;

use awaken_agent_contract::AgentSkillKind;
use awaken_resource_contract::{
    ArtifactPublication, ArtifactPublicationError, ArtifactPublicationReceipt, ArtifactPublisher,
    ArtifactRecovery, FileApplicationService, FileContentSource, FileContentSourceError,
    FileReadPurpose, LiveResourceBindingVerifier, RepositoryBindingVerifier,
    RepositoryBindingVerifierError, ResolvedFileContent, SkillStore, SkillVersion, content_id,
};
use awaken_session_configuration_contract::{
    SkillBundleSource, SkillBundleSourceError, SkillCatalogApplication, validate_skill_bundle,
};
use awaken_session_resource_contract::ResolvedSkillBinding;
use awaken_time::WallClock;

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
            .map_err(FileContentSourceError::from)?
        else {
            return Ok(None);
        };
        let actual = content_id(&bytes);
        if actual != record.blob_id {
            return Err(FileContentSourceError::Integrity {
                file_id: file_id.into(),
                expected: record.blob_id,
                actual,
            });
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
    async fn capture(
        &self,
        workspace_id: &str,
        session_id: &str,
        _fence: Option<&C>,
    ) -> Result<Option<awaken_resource_contract::ArtifactCapture>, ArtifactPublicationError> {
        let capture = self
            .application
            .artifact_capture(workspace_id, session_id)
            .await
            .map_err(ArtifactPublicationError::from)?;
        if let Some(capture) = &capture {
            capture
                .verify_scope(workspace_id, session_id)
                .map_err(ArtifactPublicationError::from)?;
        }
        Ok(capture)
    }

    async fn replace_capture(
        &self,
        request: awaken_resource_contract::ReplaceArtifactCapture,
        _fence: Option<&C>,
    ) -> Result<awaken_resource_contract::ReplaceArtifactCaptureOutcome, ArtifactPublicationError>
    {
        let outcome = self
            .application
            .replace_artifact_capture(request.clone())
            .await
            .map_err(ArtifactPublicationError::from)?;
        request
            .verify_outcome(&outcome)
            .map_err(ArtifactPublicationError::from)?;
        Ok(outcome)
    }

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
            .map_err(ArtifactPublicationError::from)?;
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
            .map_err(ArtifactPublicationError::from)?;
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
            .await
            .map_err(|error| match error {
                awaken_resource_contract::ResourceRegistryError::IdentityRejected { detail }
                | awaken_resource_contract::ResourceRegistryError::ConfigurationRejected {
                    detail,
                } => {
                    RepositoryBindingVerifierError::InvalidRequest {
                        detail,
                    }
                }
                awaken_resource_contract::ResourceRegistryError::AdmissionRejected { detail } => {
                    RepositoryBindingVerifierError::Denied { detail }
                }
                awaken_resource_contract::ResourceRegistryError::VersionExhausted { id } => {
                    RepositoryBindingVerifierError::Conflict {
                        detail: format!("resource config version space is exhausted for `{id}`"),
                    }
                }
                awaken_resource_contract::ResourceRegistryError::EncodingFailure { detail } => {
                    RepositoryBindingVerifierError::CorruptAuthority { detail }
                }
                awaken_resource_contract::ResourceRegistryError::AlreadyRegistered(detail)
                | awaken_resource_contract::ResourceRegistryError::NotFound(detail)
                | awaken_resource_contract::ResourceRegistryError::ConcurrentModification(
                    detail,
                ) => RepositoryBindingVerifierError::Conflict { detail },
                awaken_resource_contract::ResourceRegistryError::NotActive { id, state } => {
                    RepositoryBindingVerifierError::Denied {
                        detail: format!("resource `{id}` is not active ({state:?})"),
                    }
                }
                awaken_resource_contract::ResourceRegistryError::ConfigConflict {
                    id,
                    expected,
                    current,
                } => RepositoryBindingVerifierError::Conflict {
                    detail: format!(
                        "resource `{id}` config changed (expected {expected:?}, current {current:?})"
                    ),
                },
                awaken_resource_contract::ResourceRegistryError::ConfigNotFound {
                    id,
                    version,
                } => RepositoryBindingVerifierError::Denied {
                    detail: format!("resource `{id}` config version {version:?} was not found"),
                },
                awaken_resource_contract::ResourceRegistryError::CorruptData(detail) => {
                    RepositoryBindingVerifierError::CorruptAuthority { detail }
                }
                awaken_resource_contract::ResourceRegistryError::AuthorityUnavailable {
                    detail,
                } => RepositoryBindingVerifierError::AuthorityUnavailable { detail },
                awaken_resource_contract::ResourceRegistryError::Indeterminate { detail } => {
                    RepositoryBindingVerifierError::Indeterminate { detail }
                }
            })?;
        Ok(awaken_resource_contract::RepositoryTransport::Direct)
    }
}

pub struct StoreSkillBundleSource {
    store: Arc<dyn SkillStore>,
}

pub struct StoreSkillCatalogApplication {
    store: Arc<dyn SkillStore>,
    clock: Arc<dyn WallClock>,
}

impl StoreSkillCatalogApplication {
    #[must_use]
    pub fn new(store: Arc<dyn SkillStore>, clock: Arc<dyn WallClock>) -> Self {
        Self { store, clock }
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
                awaken_resource_contract::SkillStoreError::InvalidSelection {
                    detail: "invalid Skill version selector".into(),
                }
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
        let created_unix_nanos = self.clock.now().as_unix_nanos_u64();
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
    async fn authored_skill_versions_use_the_resource_clock() {
        use awaken_resource_contract::SkillStore as _;
        use awaken_session_configuration_contract::SkillCatalogApplication as _;

        // Cause/effect decision table: R1 missing Skill + content A at T1 ->
        // create v1 and its definition with the same T1 stamp; R2 same
        // content at T2 -> no new version or timestamp change; R3 changed
        // content at T2 -> append v2 stamped T2 and update only the
        // definition's updated stamp. The store's typed failures remain the
        // existing SkillStore boundary; this rule changes its time source.
        let store = Arc::new(awaken_skill_store::InMemorySkillStore::new());
        let clock = Arc::new(awaken_lease_testkit::ManualClock::new(1_700_000_000_123));
        let application = super::StoreSkillCatalogApplication::new(store.clone(), clock.clone());
        application
            .publish_authored("workspace", "guide", "Guide", "first", "content A")
            .await
            .expect("R1 create authored Skill");
        let first = store
            .version("workspace", "guide", 1)
            .await
            .expect("R1 read first version")
            .expect("R1 first version exists");
        let first_stamp = 1_700_000_000_123_000_000;
        assert_eq!(first.created_unix_nanos, first_stamp, "R1");
        let definition = store
            .definition("workspace", "guide")
            .await
            .expect("R1 read definition")
            .expect("R1 definition exists");
        assert_eq!(definition.timestamps.created_unix_nanos, first_stamp, "R1");
        assert_eq!(definition.timestamps.updated_unix_nanos, first_stamp, "R1");

        clock.advance(5);
        application
            .publish_authored("workspace", "guide", "Guide", "first", "content A")
            .await
            .expect("R2 identical content is idempotent");
        assert!(
            store
                .version("workspace", "guide", 2)
                .await
                .unwrap()
                .is_none(),
            "R2 no new version"
        );
        let definition = store
            .definition("workspace", "guide")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(definition.latest_version, 1, "R2");
        assert_eq!(definition.timestamps.updated_unix_nanos, first_stamp, "R2");

        application
            .publish_authored("workspace", "guide", "Guide", "second", "content B")
            .await
            .expect("R3 append changed content");
        let second = store
            .version("workspace", "guide", 2)
            .await
            .unwrap()
            .expect("R3 second version exists");
        let second_stamp = 1_700_000_000_128_000_000;
        assert_eq!(second.created_unix_nanos, second_stamp, "R3");
        let definition = store
            .definition("workspace", "guide")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(definition.latest_version, 2, "R3");
        assert_eq!(definition.timestamps.created_unix_nanos, first_stamp, "R3");
        assert_eq!(definition.timestamps.updated_unix_nanos, second_stamp, "R3");
    }

    struct FixedBindingFailure(awaken_resource_contract::ResourceRegistryError);

    #[async_trait::async_trait]
    impl awaken_resource_contract::LiveResourceBindingVerifier for FixedBindingFailure {
        async fn verify_memory_binding(
            &self,
            _workspace_id: &str,
            _id: &str,
            _version: awaken_resource_contract::ConfigVersion,
        ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
            Err(self.0.clone())
        }

        async fn verify_repository_binding(
            &self,
            _workspace_id: &str,
            _id: &str,
            _version: awaken_resource_contract::ConfigVersion,
        ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
            Err(self.0.clone())
        }
    }

    #[tokio::test]
    async fn repository_verifier_preserves_registry_recovery_causes() {
        // Cause/effect table: invalid command -> InvalidRequest; missing,
        // duplicate, CAS/config races -> Conflict; inactive/missing selected
        // version -> Denied; corrupt authority -> CorruptAuthority; authority
        // outage -> AuthorityUnavailable; unknown repository result ->
        // Indeterminate. No registry cause may collapse into a message-only
        // verifier error.
        use awaken_resource_contract::{
            ConfigVersion, RepositoryBindingVerifier as _, RepositoryBindingVerifierError as E,
            ResourceRegistryError as R, ResourceState,
        };
        let cases = [
            (
                R::IdentityRejected {
                    detail: "bad".into(),
                },
                "invalid",
            ),
            (
                R::ConfigurationRejected {
                    detail: "bad".into(),
                },
                "invalid",
            ),
            (
                R::AdmissionRejected {
                    detail: "denied".into(),
                },
                "denied",
            ),
            (R::VersionExhausted { id: "id".into() }, "conflict"),
            (
                R::EncodingFailure {
                    detail: "encode".into(),
                },
                "corrupt",
            ),
            (R::AlreadyRegistered("id".into()), "conflict"),
            (R::NotFound("id".into()), "conflict"),
            (
                R::NotActive {
                    id: "id".into(),
                    state: ResourceState::Archived,
                },
                "denied",
            ),
            (
                R::ConfigConflict {
                    id: "id".into(),
                    expected: ConfigVersion(1),
                    current: ConfigVersion(2),
                },
                "conflict",
            ),
            (
                R::ConfigNotFound {
                    id: "id".into(),
                    version: ConfigVersion(2),
                },
                "denied",
            ),
            (R::ConcurrentModification("id".into()), "conflict"),
            (R::CorruptData("bad row".into()), "corrupt"),
            (
                R::AuthorityUnavailable {
                    detail: "offline".into(),
                },
                "unavailable",
            ),
            (
                R::Indeterminate {
                    detail: "unknown".into(),
                },
                "indeterminate",
            ),
        ];
        for (cause, expected) in cases {
            let verifier =
                super::RegistryRepositoryBindingVerifier::new(Arc::new(FixedBindingFailure(cause)));
            let error = verifier
                .verify("workspace", "repository", ConfigVersion(1), None::<&()>)
                .await
                .expect_err(expected);
            assert!(
                matches!(
                    (&error, expected),
                    (E::InvalidRequest { .. }, "invalid")
                        | (E::Conflict { .. }, "conflict")
                        | (E::Denied { .. }, "denied")
                        | (E::CorruptAuthority { .. }, "corrupt")
                        | (E::AuthorityUnavailable { .. }, "unavailable")
                        | (E::Indeterminate { .. }, "indeterminate")
                ),
                "{expected}: {error}"
            );
        }
    }

    #[tokio::test]
    async fn corrupted_blob_type_survives_application_and_execution_source() {
        // R1 valid bytes -> exact File content. R2 external corruption under
        // the same immutable id -> preserve CorruptContent through FileStore,
        // FileApplication and FileContentSource. R3 foreign Workspace -> absent
        // before blob access. The error cannot turn into absence or an
        // undifferentiated retry signal at either adapter boundary.
        let root = tempfile::tempdir().unwrap();
        let blobs = Arc::new(
            awaken_file_store::FsFileStore::open(root.path())
                .await
                .unwrap(),
        );
        let catalog = Arc::new(awaken_file_store::InMemoryFileStore::new());
        let application = Arc::new(crate::FileApplication::new(
            blobs,
            catalog,
            Arc::new(
                awaken_resource_store::SqliteResourceStore::in_memory()
                    .await
                    .unwrap(),
            ),
            awaken_time::system_clock(),
        ));
        let file = application
            .create(crate::CreateFileCommand {
                workspace_id: "workspace",
                filename: "proof.txt".into(),
                mime_type: "text/plain".into(),
                bytes: b"original",
                downloadable: true,
                expires_at: None,
                scope_id: None,
                logical_path: None,
                idempotency_key: None,
            })
            .await
            .unwrap();
        let source = super::ApplicationFileContentSource::new(application);
        let read =
            |workspace| {
                <super::ApplicationFileContentSource as
            awaken_resource_contract::FileContentSource<()>>::read(
                &source, workspace, &file.id,
                &awaken_resource_contract::FileReadPurpose::SessionResource, None,
            )
            };
        assert_eq!(read("workspace").await.unwrap().unwrap().bytes, b"original");
        std::fs::write(root.path().join(&file.blob_id), b"damaged").unwrap();
        assert!(matches!(read("workspace").await,
            Err(awaken_resource_contract::FileContentSourceError::Resource(
                awaken_resource_contract::ResourcePurgeError::Blob(
                    awaken_resource_contract::FileStoreError::CorruptContent { content_id }
                )
            )) if content_id == file.blob_id));
        assert!(read("foreign").await.unwrap().is_none());
    }

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
                    .await
                    .expect("resource lifecycle"),
            ),
            awaken_time::system_clock(),
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
