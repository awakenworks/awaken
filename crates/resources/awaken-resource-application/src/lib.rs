// @formal surface requirement=resource.files.resource_lifecycle
// @formal reason: This source implements the repository-controlled files product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Canonical Resources application services.
//!
//! Persistence selection produces one [`ResourceAuthorities`]. This layer derives
//! the application services shared by HTTP, Runtime artifact harvesting, and
//! reclamation cleanup exactly once, without teaching those adapters about stores.

use std::sync::Arc;

mod authorities;
mod execution_sources;
mod files;
pub mod memory;
mod reclamation;
mod registry;
mod repositories;
pub use repositories::RepositoryApplication;
mod skill_ingest;
pub use authorities::ResourceAuthorities;
mod skill_lifecycle;
use awaken_resource_contract::{
    ChangeMemoryStoreState, ConfigVersion, CreateMemoryStoreCommand, FileApplicationService,
    MemoryStoreApplicationError, MemoryStoreApplicationService, MemoryStoreConfigVersion,
    MemoryStoreDefinition, PutResourcePurgeOutcome, RegisterMemoryStore, ResourceKind,
    ResourcePurgeError, ResourcePurgeIntent, ResourcePurgeScheduler, ResourceReclamationRepository,
    ResourceRegistry, ResourceState, ResourceTarget, ResourceTimestamps, UpdateMemoryStoreCommand,
    UpdateMemoryStoreProfile,
};
pub use awaken_resource_contract::{MAX_MANAGED_FILE_SIZE_BYTES, MAX_WORKSPACE_FILE_BYTES};
pub use execution_sources::{
    ApplicationArtifactPublisher, ApplicationFileContentSource, RegistryRepositoryBindingVerifier,
    StoreSkillBundleSource, StoreSkillCatalogApplication,
};
pub use files::{
    CreateFileCommand, FileApplication, PrivateFileViewFactory, admitted_private_file_view,
};
pub use reclamation::{ResourceLifecycleGuard, ResourcePhysicalCleanup};
pub use registry::RegistryApplication;
pub use skill_ingest::{
    CanonicalSkillBundle, MAX_SKILL_ARCHIVE_BYTES, MAX_SKILL_BUNDLE_BYTES, MAX_SKILL_FILE_BYTES,
    MAX_SKILL_FILES, UploadedSkillBundleFile, canonicalize_skill_bundle, normalize_bundle_path,
};

#[derive(Clone)]
pub struct ResourcesApplication {
    authorities: ResourceAuthorities,
    files: Arc<FileApplication>,
    memories: Arc<MemoryStoreApplication>,
    purge: Arc<RepositoryPurgeScheduler>,
    lifecycle_guard: Arc<ResourceLifecycleGuard>,
    physical_cleanup: Arc<ResourcePhysicalCleanup>,
}

impl ResourcesApplication {
    #[must_use]
    pub fn new(authorities: ResourceAuthorities) -> Self {
        let reclamation = authorities.reclamation();
        let purge = Arc::new(RepositoryPurgeScheduler::new(reclamation.clone()));
        Self {
            files: Arc::new(FileApplication::new(
                authorities.file_store(),
                authorities.file_catalog(),
                reclamation.clone(),
                authorities.clock(),
            )),
            memories: Arc::new(MemoryStoreApplication::new(
                authorities.resource_registry(),
                purge.clone(),
                authorities.clock(),
            )),
            lifecycle_guard: Arc::new(ResourceLifecycleGuard::new(authorities.resource_registry())),
            physical_cleanup: Arc::new(ResourcePhysicalCleanup::new(&authorities)),
            purge,
            authorities,
        }
    }

    #[must_use]
    pub fn authorities(&self) -> ResourceAuthorities {
        self.authorities.clone()
    }

    #[must_use]
    pub fn files(&self) -> Arc<dyn FileApplicationService> {
        self.files.clone()
    }

    /// Request-bound private views share this exact File application.
    #[must_use]
    pub fn private_file_views(&self) -> PrivateFileViewFactory {
        PrivateFileViewFactory::new(self.files.clone())
    }

    #[must_use]
    pub fn file_content_source<C: Sync + 'static>(
        &self,
    ) -> Arc<dyn awaken_resource_contract::FileContentSource<C>> {
        Arc::new(ApplicationFileContentSource::new(self.files.clone()))
    }

    #[must_use]
    pub fn file_content_source_with_visibility<C: Sync + 'static>(
        &self,
        visibility: Arc<dyn awaken_resource_contract::FileContentVisibility>,
    ) -> Arc<dyn awaken_resource_contract::FileContentSource<C>> {
        Arc::new(ApplicationFileContentSource::new(self.files.clone()).with_visibility(visibility))
    }

    #[must_use]
    pub fn artifact_publisher<C: Send + Sync + 'static>(
        &self,
    ) -> Arc<dyn awaken_resource_contract::ArtifactPublisher<C>> {
        Arc::new(ApplicationArtifactPublisher::new(self.files.clone()))
    }

    #[must_use]
    pub fn skill_bundle_source<C: Sync + 'static>(
        &self,
    ) -> Arc<dyn awaken_session_configuration_contract::SkillBundleSource<C>> {
        Arc::new(StoreSkillBundleSource::new(self.authorities.skill_store()))
    }

    #[must_use]
    pub fn skill_catalog_application(
        &self,
    ) -> Arc<dyn awaken_session_configuration_contract::SkillCatalogApplication> {
        Arc::new(StoreSkillCatalogApplication::new(
            self.authorities.skill_store(),
            self.authorities.clock(),
        ))
    }

    #[must_use]
    pub fn memory_stores(&self) -> Arc<dyn MemoryStoreApplicationService> {
        self.memories.clone()
    }

    pub fn repositories(
        &self,
        credentials: Arc<dyn awaken_resource_contract::RepositoryCredentialValidator>,
    ) -> Arc<dyn awaken_resource_contract::RepositoryApplicationService> {
        Arc::new(RepositoryApplication::new(
            self.authorities.resource_registry(),
            credentials,
            self.authorities.clock(),
        ))
    }

    #[must_use]
    pub fn purge_scheduler(&self) -> Arc<dyn ResourcePurgeScheduler> {
        self.purge.clone()
    }

    pub async fn synchronize_skill_references(
        &self,
    ) -> Result<(), awaken_resource_contract::SkillStoreError> {
        self.authorities.skill_lifecycle().synchronize_all().await
    }

    #[must_use]
    pub fn lifecycle_guard(&self) -> Arc<dyn awaken_resource_contract::ResourcePurgeGuard> {
        self.lifecycle_guard.clone()
    }

    #[must_use]
    pub fn purge_preparation(&self) -> Arc<dyn awaken_resource_contract::ResourcePurgePreparation> {
        self.files.clone()
    }

    #[must_use]
    pub fn physical_cleanup(&self) -> Arc<dyn awaken_resource_contract::ResourcePhysicalReclaimer> {
        self.physical_cleanup.clone()
    }
}

/// The sole MemoryStore identity/reclamation command path. Content mutations stay
/// in the independent path-addressed MemoryRepository aggregate.
pub struct MemoryStoreApplication {
    registry: Arc<dyn ResourceRegistry>,
    purge: Arc<dyn ResourcePurgeScheduler>,
    /// Stamps a registration's creation instant. It is the clock the registry
    /// application stamps every later profile and state change with, so a
    /// store's `created` and `updated` come from one timeline.
    clock: Arc<dyn awaken_time::WallClock>,
}

impl MemoryStoreApplication {
    #[must_use]
    pub fn new(
        registry: Arc<dyn ResourceRegistry>,
        purge: Arc<dyn ResourcePurgeScheduler>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self {
            registry,
            purge,
            clock,
        }
    }
}

fn mint_memory_store_id(now: awaken_time::UtcTime) -> String {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("memstore_{:016x}_{sequence:016x}", now.as_unix_nanos_u64())
}

fn required_store(
    value: Option<MemoryStoreDefinition>,
    id: &str,
) -> Result<MemoryStoreDefinition, MemoryStoreApplicationError> {
    value.ok_or_else(|| {
        awaken_resource_contract::ResourceRegistryError::NotFound(id.to_string()).into()
    })
}

fn memory_store_registration(
    command: CreateMemoryStoreCommand,
    now: awaken_time::UtcTime,
) -> RegisterMemoryStore {
    let id = command
        .id
        .unwrap_or_else(|| mint_memory_store_id(now).into());
    RegisterMemoryStore {
        definition: MemoryStoreDefinition {
            id: id.clone(),
            workspace_id: command.workspace_id,
            name: command.name,
            description: command.description,
            metadata: command.metadata,
            state: command.initial_state,
            current_config_version: ConfigVersion::INITIAL,
            timestamps: ResourceTimestamps::created(now.as_unix_nanos_u64()),
        },
        initial_config: MemoryStoreConfigVersion {
            memory_store_id: id,
            version: ConfigVersion::INITIAL,
            retention_policy: command.retention_policy,
        },
    }
}

#[async_trait::async_trait]
impl MemoryStoreApplicationService for MemoryStoreApplication {
    async fn create_idempotent(
        &self,
        mut command: CreateMemoryStoreCommand,
        request_key: &str,
    ) -> Result<MemoryStoreDefinition, MemoryStoreApplicationError> {
        if command.id.is_some()
            || request_key.trim().is_empty()
            || request_key.len() > 255
            || !request_key
                .bytes()
                .all(|byte| (0x20..=0x7e).contains(&byte))
        {
            return Err(awaken_resource_contract::ResourceRegistryError::AdmissionRejected {
                detail: "idempotent MemoryStore creation requires a visible request key and no explicit id"
                    .into(),
            }
            .into());
        }
        // Length framing keeps Workspace/key boundaries unambiguous. The
        // opaque identity contains neither the raw key nor editable metadata.
        let identity = format!(
            "memory-store-create:{}:{}{}",
            command.workspace_id.len(),
            command.workspace_id,
            request_key
        );
        command.id = Some(
            format!(
                "memstore_request_{}",
                awaken_resource_contract::content_id(identity.as_bytes())
            )
            .into(),
        );
        Ok(self
            .registry
            .register_memory_store_idempotent(memory_store_registration(command, self.clock.now()))
            .await?)
    }

    async fn create(
        &self,
        command: CreateMemoryStoreCommand,
    ) -> Result<MemoryStoreDefinition, MemoryStoreApplicationError> {
        let registration = memory_store_registration(command, self.clock.now());
        let definition = registration.definition.clone();
        self.registry.register_memory_store(registration).await?;
        Ok(definition)
    }

    async fn get(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Option<MemoryStoreDefinition>, MemoryStoreApplicationError> {
        Ok(self.registry.find_memory_store(workspace_id, id).await?)
    }

    async fn list(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<MemoryStoreDefinition>, MemoryStoreApplicationError> {
        Ok(self.registry.list_memory_stores(workspace_id).await?)
    }

    async fn update(
        &self,
        command: UpdateMemoryStoreCommand,
    ) -> Result<MemoryStoreDefinition, MemoryStoreApplicationError> {
        let mut definition = required_store(
            self.registry
                .find_memory_store(&command.workspace_id, command.id.as_ref())
                .await?,
            command.id.as_ref(),
        )?;
        if let Some(name) = command.name {
            definition.name = name;
        }
        if let Some(description) = command.description {
            definition.description = description;
        }
        for (key, value) in command.metadata_patch {
            match value {
                Some(value) => {
                    definition.metadata.insert(key, value);
                }
                None => {
                    definition.metadata.remove(&key);
                }
            }
        }
        self.registry
            .update_memory_store_profile(UpdateMemoryStoreProfile {
                workspace_id: command.workspace_id,
                id: definition.id.clone(),
                name: definition.name,
                description: definition.description,
                metadata: definition.metadata,
            })
            .await?;
        required_store(
            self.registry
                .find_memory_store(&definition.workspace_id, definition.id.as_ref())
                .await?,
            definition.id.as_ref(),
        )
    }

    async fn set_state(
        &self,
        workspace_id: &str,
        id: &str,
        state: ResourceState,
    ) -> Result<MemoryStoreDefinition, MemoryStoreApplicationError> {
        self.registry
            .change_memory_store_state(ChangeMemoryStoreState {
                workspace_id: workspace_id.into(),
                id: id.into(),
                state,
            })
            .await?;
        required_store(self.registry.find_memory_store(workspace_id, id).await?, id)
    }

    async fn delete(
        &self,
        workspace_id: &str,
        id: &str,
        requested_at_unix_ms: u64,
    ) -> Result<MemoryStoreDefinition, MemoryStoreApplicationError> {
        let definition =
            required_store(self.registry.find_memory_store(workspace_id, id).await?, id)?;
        let config = self
            .registry
            .find_memory_store_config(workspace_id, id, definition.current_config_version)
            .await?
            .ok_or_else(
                || awaken_resource_contract::ResourceRegistryError::ConfigNotFound {
                    id: id.to_string(),
                    version: definition.current_config_version,
                },
            )?;
        let retention_ms = config
            .retention_policy
            .retention_days
            .map_or(0, |days| u64::from(days).saturating_mul(86_400_000));
        self.purge
            .schedule_purge(
                ResourceTarget::new(workspace_id, ResourceKind::MemoryStore, id),
                Some(definition.current_config_version.0),
                requested_at_unix_ms,
                requested_at_unix_ms.saturating_add(retention_ms),
            )
            .await?;
        self.set_state(workspace_id, id, ResourceState::Deleted)
            .await
    }
}

/// Persist a deterministic, idempotent cleanup intent after logical deletion.
/// Physical reclamation remains the separately supervised state machine.
pub struct RepositoryPurgeScheduler {
    reclamation: Arc<dyn ResourceReclamationRepository>,
}

impl RepositoryPurgeScheduler {
    #[must_use]
    pub fn new(reclamation: Arc<dyn ResourceReclamationRepository>) -> Self {
        Self { reclamation }
    }
}

#[async_trait::async_trait]
impl ResourcePurgeScheduler for RepositoryPurgeScheduler {
    async fn schedule_purge(
        &self,
        target: ResourceTarget,
        config_version: Option<u64>,
        requested_at_unix_ms: u64,
        not_before_unix_ms: u64,
    ) -> Result<PutResourcePurgeOutcome, ResourcePurgeError> {
        let key = format!(
            "{}:{:?}:{}:{:?}",
            target.workspace_id, target.kind, target.resource_id, config_version
        );
        self.reclamation
            .put(ResourcePurgeIntent::new(
                format!("purge:{:?}:{key}", target.kind),
                key,
                target,
                config_version,
                requested_at_unix_ms,
                not_before_unix_ms,
            )?)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_resource_contract::ResourceKind;

    async fn application() -> ResourcesApplication {
        let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
        let clock = awaken_time::system_clock();
        let resources = Arc::new(
            awaken_resource_store::SqliteResourceStore::in_memory()
                .await
                .expect("open resource authority"),
        );
        let registry = Arc::new(RegistryApplication::new(resources.clone(), clock.clone()));
        ResourcesApplication::new(ResourceAuthorities::new(
            registry,
            files.clone(),
            files,
            {
                #[derive(Default)]
                struct MemoryEvents(std::sync::Mutex<Vec<(String, String)>>);
                #[cfg(test)]
                impl awaken_resource_contract::MemoryInvalidator for MemoryEvents {
                    fn publish(&self, store: &str, path: &str) {
                        self.0.lock().unwrap().push((store.into(), path.into()));
                    }
                }
                Arc::new(crate::memory::InvalidatingMemoryRepository::new(
                    Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
                        clock.clone(),
                    )),
                    Arc::new(MemoryEvents::default()),
                ))
            },
            Arc::new(awaken_skill_store::InMemorySkillStore::new()),
            resources,
            clock,
        ))
    }

    #[tokio::test]
    async fn memory_store_creation_keys_are_workspace_scoped_and_optional() {
        let application = application().await;
        let stores = application.memory_stores();
        let command = CreateMemoryStoreCommand {
            workspace_id: "workspace".into(),
            id: None,
            name: "Workshop".into(),
            description: String::new(),
            metadata: Default::default(),
            initial_state: ResourceState::Active,
            retention_policy: Default::default(),
        };
        let first = stores
            .create_idempotent(command.clone(), "same-key")
            .await
            .unwrap();
        let retry = stores
            .create_idempotent(command.clone(), "same-key")
            .await
            .unwrap();
        assert_eq!(first, retry);
        let mut foreign = command.clone();
        foreign.workspace_id = "another-workspace".into();
        let other = stores.create_idempotent(foreign, "same-key").await.unwrap();
        assert_ne!(first.id, other.id);
        assert_ne!(
            first.id,
            stores
                .create_idempotent(command.clone(), "new-key")
                .await
                .unwrap()
                .id
        );
        let unkeyed = stores.create(command.clone()).await.unwrap();
        assert_ne!(unkeyed.id, stores.create(command.clone()).await.unwrap().id);
        let mut explicit = command.clone();
        explicit.id = Some("must-not-be-used".into());
        assert!(stores.create_idempotent(explicit, "key").await.is_err());
        for key in ["", " ", "line\nbreak", &"x".repeat(256)] {
            assert!(
                stores
                    .create_idempotent(command.clone(), key)
                    .await
                    .is_err()
            );
        }
        assert_eq!(stores.list("workspace").await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn memory_store_commands_share_one_lifecycle_owner() {
        // FMECA: FM1 HTTP and runtime receive different MemoryStore services;
        // FM2 repeated create overwrites an existing aggregate; FM3 lifecycle
        // transitions lose version order. Cause/effect graph and decision table:
        // C0 one Resources application -> E0 every consumer receives the exact
        // same MemoryStore and purge service allocations; C1 absent id + valid
        // create -> E1 one Suspended aggregate at v1; C2 same deterministic id
        // again -> E2 conflict and original unchanged; C3 existing id +
        // metadata/state commands -> E3 one updated aggregate; C4 delete with
        // zero retention -> E4 Deleted plus one durable purge; C5 unknown id ->
        // E5 NotFound and no aggregate.
        //
        // | Rule | application | command/state | Effect |
        // | R0   | one         | none          | E0     |
        // | R1   | one         | create/new    | E1     |
        // | R2   | one         | create/exists | E2     |
        // | R3   | one         | update/exists | E3     |
        // | R4   | one         | delete/exists | E4     |
        // | R5   | one         | any/missing   | E5     |
        // HTTP and Dream both drive the same application ports proven by R0.
        let application = application().await;
        let stores = application.memory_stores();
        assert!(
            Arc::ptr_eq(&stores, &application.memory_stores()),
            "R0: MemoryStore service must not be reconstructed per consumer"
        );
        assert!(
            Arc::ptr_eq(
                &application.purge_scheduler(),
                &application.purge_scheduler()
            ),
            "R0: reclamation scheduler must not be reconstructed per consumer"
        );
        let id: awaken_resource_contract::MemoryStoreId = "memory-result".into();
        let created = stores
            .create(CreateMemoryStoreCommand {
                workspace_id: "workspace".into(),
                id: Some(id.clone()),
                name: "Dream result".into(),
                description: String::new(),
                metadata: Default::default(),
                initial_state: ResourceState::Suspended,
                retention_policy: Default::default(),
            })
            .await
            .expect("R1");
        assert_eq!(created.state, ResourceState::Suspended, "R1");
        assert!(
            stores
                .create(CreateMemoryStoreCommand {
                    workspace_id: "workspace".into(),
                    id: Some(id.clone()),
                    name: "duplicate".into(),
                    description: String::new(),
                    metadata: Default::default(),
                    initial_state: ResourceState::Active,
                    retention_policy: Default::default(),
                })
                .await
                .is_err(),
            "R2"
        );
        let updated = stores
            .update(UpdateMemoryStoreCommand {
                workspace_id: "workspace".into(),
                id: id.clone(),
                name: Some("renamed-memory".into()),
                description: Some("curated".into()),
                metadata_patch: [("source".into(), Some("dream".into()))].into(),
            })
            .await
            .expect("R3");
        assert_eq!(updated.description, "curated", "R3");
        assert_eq!(updated.name, "renamed-memory", "R3");
        assert_eq!(
            updated.metadata.get("source").map(String::as_str),
            Some("dream"),
            "R3"
        );
        assert_eq!(
            stores
                .set_state("workspace", id.as_ref(), ResourceState::Active)
                .await
                .expect("R3")
                .state,
            ResourceState::Active,
            "R3"
        );
        assert_eq!(
            stores
                .delete("workspace", id.as_ref(), 100)
                .await
                .expect("R4")
                .state,
            ResourceState::Deleted,
            "R4"
        );
        assert!(
            stores
                .get("workspace", "unknown")
                .await
                .expect("R5")
                .is_none(),
            "R5"
        );
    }

    #[tokio::test]
    async fn cleanup_schedule_is_one_idempotent_resources_command() {
        // Cause/effect decision table:
        // R1 a valid logical-delete fact -> one durable pending intent; R2 the
        // same target/config fact is retried -> Existing and no parallel intent;
        // R3 an invalid empty coordinate -> Invalid and no intent.
        let application = application().await;
        let target = ResourceTarget::new("workspace", ResourceKind::MemoryStore, "memory");
        assert_eq!(
            application
                .purge_scheduler()
                .schedule_purge(target.clone(), Some(3), 10, 20)
                .await
                .expect("R1"),
            PutResourcePurgeOutcome::Inserted
        );
        assert_eq!(
            application
                .purge_scheduler()
                .schedule_purge(target, Some(3), 99, 100)
                .await
                .expect("R2"),
            PutResourcePurgeOutcome::Existing
        );
        assert!(
            application
                .purge_scheduler()
                .schedule_purge(
                    ResourceTarget::new("", ResourceKind::File, "file"),
                    None,
                    10,
                    10,
                )
                .await
                .is_err(),
            "R3"
        );
    }
}
