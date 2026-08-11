#![allow(dead_code)]

use std::sync::Arc;

use awaken_resource_application::{ResourceAuthorities, ResourcesApplication};
use awaken_resource_contract::SkillStore;

/// Build the canonical Resources application consumed during process startup,
/// with an injectable Skill repository for durability-specific protocol cases.
pub fn resources(skill_store: Arc<dyn SkillStore>) -> ResourcesApplication {
    let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
    let catalog = Arc::new(
        awaken_resource_store::SqliteResourceStore::in_memory()
            .expect("open ephemeral Resource Catalog"),
    );
    ResourcesApplication::new(ResourceAuthorities::new(
        catalog.clone(),
        files.clone(),
        files,
        Arc::new(awaken_memory_store::VolatileMemoryRepository::new()),
        skill_store,
        catalog,
    ))
}

pub fn ephemeral_resources() -> ResourcesApplication {
    resources(Arc::new(awaken_skill_store::InMemorySkillStore::new()))
}

pub fn filesystem_skill_store(path: impl Into<std::path::PathBuf>) -> Arc<dyn SkillStore> {
    Arc::new(
        awaken_skill_store::FsSkillStore::open(path.into()).expect("open filesystem Skill store"),
    )
}
