//! Canonical Resources application component.
//!
//! File, Memory, Skill, and lifecycle retain their type-specific semantics and
//! repositories. This component is only their atomic process-composition value:
//! callers cannot accidentally pair ports opened from different backend
//! selections, and Runtime/Coordinator code receives no concrete database type.

use std::sync::Arc;

use crate::{
    FileCatalog, FileStore, MemoryRepository, ResourceCatalog, ResourceLifecycleRepository,
    SkillStore,
};

/// Resources-owned ports selected by an outer process adapter.
pub struct ResourceDependencies {
    /// Secret-free definitions and immutable configuration versions used by
    /// Session resolution. The catalog belongs to Resources even when a legacy
    /// concrete adapter also implements unrelated admin-store interfaces.
    pub resource_catalog: Arc<dyn ResourceCatalog>,
    pub file_store: Arc<dyn FileStore>,
    pub file_catalog: Arc<dyn FileCatalog>,
    pub memory_repository: Arc<dyn MemoryRepository>,
    pub skill_store: Arc<dyn SkillStore>,
    pub lifecycle: Arc<dyn ResourceLifecycleRepository>,
}

/// One complete Resources component.
///
/// This is not a universal Resource aggregate or materializer. It preserves the
/// independent per-kind ports while guaranteeing that one process selects them
/// together exactly once.
#[derive(Clone)]
pub struct ResourceComponent {
    resource_catalog: Arc<dyn ResourceCatalog>,
    file_store: Arc<dyn FileStore>,
    file_catalog: Arc<dyn FileCatalog>,
    memory_repository: Arc<dyn MemoryRepository>,
    skill_store: Arc<dyn SkillStore>,
    lifecycle: Arc<dyn ResourceLifecycleRepository>,
}

#[must_use]
pub fn build_resource_component(dependencies: ResourceDependencies) -> ResourceComponent {
    ResourceComponent {
        resource_catalog: dependencies.resource_catalog,
        file_store: dependencies.file_store,
        file_catalog: dependencies.file_catalog,
        memory_repository: dependencies.memory_repository,
        skill_store: dependencies.skill_store,
        lifecycle: dependencies.lifecycle,
    }
}

impl ResourceComponent {
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
    pub fn lifecycle(&self) -> Arc<dyn ResourceLifecycleRepository> {
        self.lifecycle.clone()
    }
}
