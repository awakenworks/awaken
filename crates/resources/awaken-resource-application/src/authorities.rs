//! Canonical Resources authorities selected for one application.
//!
//! File, Memory, Skill, and reclamation retain their type-specific semantics and
//! repositories. This value keeps their selection atomic: callers cannot
//! accidentally pair authorities opened from different backend
//! selections, and Runtime/Coordinator code receives no concrete database type.

use std::sync::Arc;

use awaken_resource_contract::{
    FileCatalog, FileStore, MemoryContentPurge, MemoryCopyConflictPreservation,
    MemoryCopyConflictQueries, MemoryCopyWriteback, MemoryFilesystem, MemoryQueries,
    MemorySnapshotSource, MemoryVersionQueries, MemoryVersionRedaction, MemoryWriter,
    ResourceReclamationRepository, ResourceRegistry, SkillStore,
};

/// One complete set of Resources authorities.
///
/// This is not a universal Resource aggregate or materializer. It preserves the
/// independent per-kind authorities while guaranteeing that one process selects them
/// together exactly once.
#[derive(Clone)]
pub struct ResourceAuthorities {
    resource_registry: Arc<dyn ResourceRegistry>,
    file_store: Arc<dyn FileStore>,
    file_catalog: Arc<dyn FileCatalog>,
    memory_execution_content: awaken_resource_contract::MemoryExecutionContent,
    memory_mount_source: awaken_resource_contract::MemoryMountSource,
    memory_management_content: awaken_resource_contract::MemoryManagementContent,
    memory_queries: Arc<dyn MemoryQueries>,
    memory_snapshot_source: Arc<dyn MemorySnapshotSource>,
    memory_writer: Arc<dyn MemoryWriter>,
    memory_filesystem: Arc<dyn MemoryFilesystem>,
    memory_copy_writeback: Arc<dyn MemoryCopyWriteback>,
    memory_copy_conflict_queries: Arc<dyn MemoryCopyConflictQueries>,
    memory_copy_conflict_preservation: Arc<dyn MemoryCopyConflictPreservation>,
    memory_versions: Arc<dyn MemoryVersionQueries>,
    memory_version_redaction: Arc<dyn MemoryVersionRedaction>,
    memory_content_purge: Arc<dyn MemoryContentPurge>,
    skill_store: Arc<dyn SkillStore>,
    skill_lifecycle: Arc<crate::skill_lifecycle::ReferenceIndexedSkillStore>,
    reclamation: Arc<dyn ResourceReclamationRepository>,
    clock: Arc<dyn awaken_time::WallClock>,
}

impl ResourceAuthorities {
    #[must_use]
    pub fn new<M>(
        resource_registry: Arc<dyn ResourceRegistry>,
        file_store: Arc<dyn FileStore>,
        file_catalog: Arc<dyn FileCatalog>,
        memory: Arc<crate::memory::InvalidatingMemoryRepository<M>>,
        skill_store: Arc<dyn SkillStore>,
        reclamation: Arc<dyn ResourceReclamationRepository>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self
    where
        M: MemoryQueries
            + MemorySnapshotSource
            + MemoryWriter
            + MemoryFilesystem
            + MemoryCopyWriteback
            + MemoryCopyConflictQueries
            + MemoryCopyConflictPreservation
            + MemoryVersionQueries
            + MemoryVersionRedaction
            + MemoryContentPurge
            + 'static,
    {
        let skill_lifecycle = Arc::new(crate::skill_lifecycle::ReferenceIndexedSkillStore::new(
            skill_store,
            reclamation.clone(),
        ));
        Self {
            resource_registry,
            file_store,
            file_catalog,
            memory_execution_content: awaken_resource_contract::MemoryExecutionContent::new(
                memory.clone(),
            ),
            memory_mount_source: awaken_resource_contract::MemoryMountSource::new(memory.clone()),
            memory_management_content: awaken_resource_contract::MemoryManagementContent::new(
                memory.clone(),
            ),
            memory_queries: memory.clone(),
            memory_snapshot_source: memory.clone(),
            memory_writer: memory.clone(),
            memory_filesystem: memory.clone(),
            memory_copy_writeback: memory.clone(),
            memory_copy_conflict_queries: memory.clone(),
            memory_copy_conflict_preservation: memory.clone(),
            memory_versions: memory.clone(),
            memory_version_redaction: memory.clone(),
            memory_content_purge: memory.clone(),
            skill_store: skill_lifecycle.clone(),
            skill_lifecycle,
            reclamation,
            clock,
        }
    }

    #[must_use]
    pub fn resource_registry(&self) -> Arc<dyn ResourceRegistry> {
        self.resource_registry.clone()
    }

    #[must_use]
    pub fn file_store(&self) -> Arc<dyn FileStore> {
        self.file_store.clone()
    }

    #[must_use]
    pub fn file_catalog(&self) -> Arc<dyn FileCatalog> {
        self.file_catalog.clone()
    }

    #[must_use]
    pub fn memory_execution_content(&self) -> awaken_resource_contract::MemoryExecutionContent {
        self.memory_execution_content.clone()
    }
    #[must_use]
    pub fn memory_mount_source(&self) -> awaken_resource_contract::MemoryMountSource {
        self.memory_mount_source.clone()
    }

    #[must_use]
    pub fn memory_management_content(&self) -> awaken_resource_contract::MemoryManagementContent {
        self.memory_management_content.clone()
    }

    #[must_use]
    pub fn memory_queries(&self) -> Arc<dyn MemoryQueries> {
        self.memory_queries.clone()
    }

    #[must_use]
    pub fn memory_snapshot_source(&self) -> Arc<dyn MemorySnapshotSource> {
        self.memory_snapshot_source.clone()
    }

    #[must_use]
    pub fn memory_writer(&self) -> Arc<dyn MemoryWriter> {
        self.memory_writer.clone()
    }

    #[must_use]
    pub fn memory_filesystem(&self) -> Arc<dyn MemoryFilesystem> {
        self.memory_filesystem.clone()
    }

    #[must_use]
    pub fn memory_copy_writeback(&self) -> Arc<dyn MemoryCopyWriteback> {
        self.memory_copy_writeback.clone()
    }

    #[must_use]
    pub fn memory_copy_conflict_queries(&self) -> Arc<dyn MemoryCopyConflictQueries> {
        self.memory_copy_conflict_queries.clone()
    }

    #[must_use]
    pub fn memory_copy_conflict_preservation(&self) -> Arc<dyn MemoryCopyConflictPreservation> {
        self.memory_copy_conflict_preservation.clone()
    }

    #[must_use]
    pub fn memory_versions(&self) -> Arc<dyn MemoryVersionQueries> {
        self.memory_versions.clone()
    }

    #[must_use]
    pub fn memory_version_redaction(&self) -> Arc<dyn MemoryVersionRedaction> {
        self.memory_version_redaction.clone()
    }

    #[must_use]
    pub fn memory_content_purge(&self) -> Arc<dyn MemoryContentPurge> {
        self.memory_content_purge.clone()
    }

    #[must_use]
    pub fn skill_store(&self) -> Arc<dyn SkillStore> {
        self.skill_store.clone()
    }

    pub(crate) fn skill_lifecycle(
        &self,
    ) -> Arc<crate::skill_lifecycle::ReferenceIndexedSkillStore> {
        self.skill_lifecycle.clone()
    }

    #[must_use]
    pub fn reclamation(&self) -> Arc<dyn ResourceReclamationRepository> {
        self.reclamation.clone()
    }

    /// The Resources context's one clock, selected with the authorities: the
    /// registry, the File, Memory-store and Repository commands and the Files
    /// API all read it, so no Resource record mixes two timelines.
    #[must_use]
    pub fn clock(&self) -> Arc<dyn awaken_time::WallClock> {
        self.clock.clone()
    }
}
