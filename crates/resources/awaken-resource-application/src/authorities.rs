//! Canonical Resources authorities selected for one application.
//!
//! File, Memory, Skill, and reclamation retain their type-specific semantics and
//! repositories. This value keeps their selection atomic: callers cannot
//! accidentally pair authorities opened from different backend
//! selections, and Runtime/Coordinator code receives no concrete database type.

use std::sync::Arc;

use awaken_resource_contract::{
    FileCatalog, FileStore, MemoryRepository, ResourceReclamationRepository, ResourceRegistry,
    SkillStore,
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
    memory_repository: Arc<dyn MemoryRepository>,
    skill_store: Arc<dyn SkillStore>,
    skill_lifecycle: Arc<crate::skill_lifecycle::ReferenceIndexedSkillStore>,
    reclamation: Arc<dyn ResourceReclamationRepository>,
}

impl ResourceAuthorities {
    #[must_use]
    pub fn new(
        resource_registry: Arc<dyn ResourceRegistry>,
        file_store: Arc<dyn FileStore>,
        file_catalog: Arc<dyn FileCatalog>,
        memory_repository: Arc<dyn MemoryRepository>,
        skill_store: Arc<dyn SkillStore>,
        reclamation: Arc<dyn ResourceReclamationRepository>,
    ) -> Self {
        let skill_lifecycle = Arc::new(crate::skill_lifecycle::ReferenceIndexedSkillStore::new(
            skill_store,
            reclamation.clone(),
        ));
        Self {
            resource_registry,
            file_store,
            file_catalog,
            memory_repository,
            skill_store: skill_lifecycle.clone(),
            skill_lifecycle,
            reclamation,
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
    pub fn memory_repository(&self) -> Arc<dyn MemoryRepository> {
        self.memory_repository.clone()
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
}
