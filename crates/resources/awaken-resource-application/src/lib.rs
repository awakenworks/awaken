//! Canonical Resources application services.
//!
//! Persistence selection produces one [`ResourceAuthorities`]. This layer derives
//! the application services shared by HTTP, Runtime artifact harvesting, and
//! reclamation cleanup exactly once, without teaching those adapters about stores.

use std::sync::Arc;

mod authorities;
mod execution_sources;
mod files;
mod reclamation;
mod skill_ingest;
pub use authorities::ResourceAuthorities;
mod skill_lifecycle;
use awaken_resource_contract::{
    ConfigVersion, CreateMemoryStoreCommand, FileApplicationService, MemoryStoreApplicationError,
    MemoryStoreApplicationService, MemoryStoreConfigVersion, MemoryStoreDefinition,
    PutResourcePurgeOutcome, ResourceCatalog, ResourceKind, ResourcePurgeError,
    ResourcePurgeIntent, ResourcePurgeScheduler, ResourceReclamationRepository, ResourceState,
    ResourceTarget, ResourceTimestamps, UpdateMemoryStoreCommand,
};
pub use awaken_resource_contract::{MAX_MANAGED_FILE_SIZE_BYTES, MAX_WORKSPACE_FILE_BYTES};
pub use execution_sources::{
    ApplicationArtifactPublisher, ApplicationFileContentSource, CatalogRepositoryBindingVerifier,
    StoreSkillBundleSource, StoreSkillCatalogApplication,
};
pub use files::{CreateFileCommand, FileApplication};
pub use reclamation::{ResourceLifecycleGuard, ResourcePhysicalCleanup};
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
            )),
            memories: Arc::new(MemoryStoreApplication::new(
                authorities.resource_catalog(),
                purge.clone(),
            )),
            lifecycle_guard: Arc::new(ResourceLifecycleGuard::new(authorities.resource_catalog())),
            physical_cleanup: Arc::new(ResourcePhysicalCleanup::new(
                authorities.file_store(),
                authorities.memory_repository(),
                authorities.skill_store(),
            )),
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

    #[must_use]
    pub fn file_content_source<C: Sync + 'static>(
        &self,
    ) -> Arc<dyn awaken_resource_contract::FileContentSource<C>> {
        Arc::new(ApplicationFileContentSource::new(self.files.clone()))
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
    ) -> Arc<dyn awaken_session_contract::SkillBundleSource<C>> {
        Arc::new(StoreSkillBundleSource::new(self.authorities.skill_store()))
    }

    #[must_use]
    pub fn skill_catalog_application(
        &self,
    ) -> Arc<dyn awaken_session_contract::SkillCatalogApplication> {
        Arc::new(StoreSkillCatalogApplication::new(
            self.authorities.skill_store(),
        ))
    }

    #[must_use]
    pub fn memory_stores(&self) -> Arc<dyn MemoryStoreApplicationService> {
        self.memories.clone()
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
    pub fn physical_cleanup(&self) -> Arc<dyn awaken_resource_contract::ResourcePhysicalReclaimer> {
        self.physical_cleanup.clone()
    }
}

/// The sole MemoryStore identity/reclamation command path. Content mutations stay
/// in the independent path-addressed MemoryRepository aggregate.
pub struct MemoryStoreApplication {
    catalog: Arc<dyn ResourceCatalog>,
    purge: Arc<dyn ResourcePurgeScheduler>,
}

impl MemoryStoreApplication {
    #[must_use]
    pub fn new(catalog: Arc<dyn ResourceCatalog>, purge: Arc<dyn ResourcePurgeScheduler>) -> Self {
        Self { catalog, purge }
    }
}

fn now_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or_default()
}

fn mint_memory_store_id() -> String {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("memstore_{:016x}_{sequence:016x}", now_nanos())
}

fn required_store(
    value: Option<MemoryStoreDefinition>,
    id: &str,
) -> Result<MemoryStoreDefinition, MemoryStoreApplicationError> {
    value.ok_or_else(|| {
        awaken_resource_contract::ResourceCatalogError::NotFound(id.to_string()).into()
    })
}

#[async_trait::async_trait]
impl MemoryStoreApplicationService for MemoryStoreApplication {
    async fn create(
        &self,
        command: CreateMemoryStoreCommand,
    ) -> Result<MemoryStoreDefinition, MemoryStoreApplicationError> {
        let id = command.id.unwrap_or_else(|| mint_memory_store_id().into());
        let definition = MemoryStoreDefinition {
            id: id.clone(),
            workspace_id: command.workspace_id,
            name: command.name,
            description: command.description,
            metadata: command.metadata,
            state: command.initial_state,
            current_config_version: ConfigVersion::INITIAL,
            timestamps: ResourceTimestamps::created(now_nanos()),
        };
        self.catalog.create_memory_store(
            definition.clone(),
            MemoryStoreConfigVersion {
                memory_store_id: id,
                version: ConfigVersion::INITIAL,
                retention_policy: command.retention_policy,
            },
        )?;
        Ok(definition)
    }

    async fn get(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Option<MemoryStoreDefinition>, MemoryStoreApplicationError> {
        Ok(self.catalog.memory_store(workspace_id, id)?)
    }

    async fn list(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<MemoryStoreDefinition>, MemoryStoreApplicationError> {
        Ok(self.catalog.list_memory_stores(workspace_id)?)
    }

    async fn update(
        &self,
        command: UpdateMemoryStoreCommand,
    ) -> Result<MemoryStoreDefinition, MemoryStoreApplicationError> {
        let mut definition = required_store(
            self.catalog
                .memory_store(&command.workspace_id, command.id.as_ref())?,
            command.id.as_ref(),
        )?;
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
        definition.timestamps.touch(now_nanos());
        self.catalog.update_memory_store(definition.clone())?;
        Ok(definition)
    }

    async fn set_state(
        &self,
        workspace_id: &str,
        id: &str,
        state: ResourceState,
    ) -> Result<MemoryStoreDefinition, MemoryStoreApplicationError> {
        self.catalog.set_memory_state(workspace_id, id, state)?;
        required_store(self.catalog.memory_store(workspace_id, id)?, id)
    }

    async fn delete(
        &self,
        workspace_id: &str,
        id: &str,
        requested_at_unix_ms: u64,
    ) -> Result<MemoryStoreDefinition, MemoryStoreApplicationError> {
        let definition = required_store(self.catalog.memory_store(workspace_id, id)?, id)?;
        let config = self
            .catalog
            .memory_config(workspace_id, id, definition.current_config_version)?
            .ok_or_else(
                || awaken_resource_contract::ResourceCatalogError::ConfigNotFound {
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

    fn application() -> ResourcesApplication {
        let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
        let resources = Arc::new(
            awaken_resource_store::SqliteResourceStore::in_memory()
                .expect("open resource authority"),
        );
        ResourcesApplication::new(ResourceAuthorities::new(
            resources.clone(),
            files.clone(),
            files,
            Arc::new(awaken_memory_store::VolatileMemoryRepository::new()),
            Arc::new(awaken_skill_store::InMemorySkillStore::new()),
            resources,
        ))
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
        let application = application();
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
                description: Some("curated".into()),
                metadata_patch: [("source".into(), Some("dream".into()))].into(),
            })
            .await
            .expect("R3");
        assert_eq!(updated.description, "curated", "R3");
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
        let application = application();
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
