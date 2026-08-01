//! Canonical Resources application assembly.
//!
//! Persistence selection produces one [`ResourceComponent`]. This layer derives
//! the application services shared by HTTP, Runtime artifact harvesting, and
//! lifecycle cleanup exactly once, without teaching those adapters about stores.

use std::sync::Arc;

use awaken_file_application::FileApplication;
use awaken_resource_contract::{
    FileApplicationService, PutResourcePurgeOutcome, ResourceComponent,
    ResourceLifecycleRepository, ResourcePurgeError, ResourcePurgeIntent, ResourcePurgeScheduler,
    ResourceTarget,
};

#[derive(Clone)]
pub struct ResourcesApplication {
    ports: ResourceComponent,
    files: Arc<FileApplication>,
    purge: Arc<RepositoryPurgeScheduler>,
}

impl ResourcesApplication {
    #[must_use]
    pub fn new(ports: ResourceComponent) -> Self {
        let lifecycle = ports.lifecycle();
        Self {
            files: Arc::new(FileApplication::new(
                ports.file_store(),
                ports.file_catalog(),
                lifecycle.clone(),
            )),
            purge: Arc::new(RepositoryPurgeScheduler::new(lifecycle)),
            ports,
        }
    }

    #[must_use]
    pub fn ports(&self) -> ResourceComponent {
        self.ports.clone()
    }

    #[must_use]
    pub fn files(&self) -> Arc<dyn FileApplicationService> {
        self.files.clone()
    }

    #[must_use]
    pub fn purge_scheduler(&self) -> Arc<dyn ResourcePurgeScheduler> {
        self.purge.clone()
    }
}

/// Persist a deterministic, idempotent cleanup intent after logical deletion.
/// Physical reclamation remains the separately supervised state machine.
pub struct RepositoryPurgeScheduler {
    lifecycle: Arc<dyn ResourceLifecycleRepository>,
}

impl RepositoryPurgeScheduler {
    #[must_use]
    pub fn new(lifecycle: Arc<dyn ResourceLifecycleRepository>) -> Self {
        Self { lifecycle }
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
        self.lifecycle
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
    use awaken_resource_contract::{ResourceDependencies, ResourceKind};

    fn application() -> ResourcesApplication {
        let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
        let resources = Arc::new(
            awaken_resource_store::SqliteResourceStore::in_memory()
                .expect("open resource authority"),
        );
        ResourcesApplication::new(awaken_resource_contract::build_resource_component(
            ResourceDependencies {
                resource_catalog: resources.clone(),
                file_store: files.clone(),
                file_catalog: files,
                memory_repository: Arc::new(awaken_memory_store::VolatileMemoryRepository::new()),
                skill_store: Arc::new(awaken_skill_store::InMemorySkillStore::new()),
                lifecycle: resources,
            },
        ))
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
