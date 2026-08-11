//! Canonical Resources authorities selected for one application.
//!
//! File, Memory, Skill, and reclamation retain their type-specific semantics and
//! repositories. This value keeps their selection atomic: callers cannot
//! accidentally pair authorities opened from different backend
//! selections, and Runtime/Coordinator code receives no concrete database type.

use std::sync::Arc;

use awaken_resource_contract::{
    FileCatalog, FileStore, MemoryRepository, ResourceCatalog, ResourceReclamationRepository,
    SkillStore,
};

/// One complete set of Resources authorities.
///
/// This is not a universal Resource aggregate or materializer. It preserves the
/// independent per-kind authorities while guaranteeing that one process selects them
/// together exactly once.
#[derive(Clone)]
pub struct ResourceAuthorities {
    resource_catalog: Arc<dyn ResourceCatalog>,
    file_store: Arc<dyn FileStore>,
    file_catalog: Arc<dyn FileCatalog>,
    memory_repository: Arc<dyn MemoryRepository>,
    skill_store: Arc<dyn SkillStore>,
    reclamation: Arc<dyn ResourceReclamationRepository>,
}

impl ResourceAuthorities {
    #[must_use]
    pub fn new(
        resource_catalog: Arc<dyn ResourceCatalog>,
        file_store: Arc<dyn FileStore>,
        file_catalog: Arc<dyn FileCatalog>,
        memory_repository: Arc<dyn MemoryRepository>,
        skill_store: Arc<dyn SkillStore>,
        reclamation: Arc<dyn ResourceReclamationRepository>,
    ) -> Self {
        Self {
            resource_catalog,
            file_store,
            file_catalog,
            memory_repository,
            skill_store,
            reclamation,
        }
    }

    #[must_use]
    pub fn resource_catalog(&self) -> Arc<dyn ResourceCatalog> {
        self.resource_catalog.clone()
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
    pub fn memory_repository(&self) -> Arc<dyn MemoryRepository> {
        self.memory_repository.clone()
    }

    #[must_use]
    pub fn skill_store(&self) -> Arc<dyn SkillStore> {
        self.skill_store.clone()
    }

    #[must_use]
    pub fn reclamation(&self) -> Arc<dyn ResourceReclamationRepository> {
        self.reclamation.clone()
    }
}
