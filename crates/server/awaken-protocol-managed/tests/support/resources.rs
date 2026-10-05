#![allow(dead_code)]

use std::sync::Arc;

use awaken_resource_application::{RegistryApplication, ResourceAuthorities, ResourcesApplication};
use awaken_resource_contract::SkillStore;

/// Build the canonical Resources application consumed during process startup,
/// with an injectable Skill repository for durability-specific protocol cases.
pub fn resources(skill_store: Arc<dyn SkillStore>) -> ResourcesApplication {
    resources_at(skill_store, awaken_time::system_clock())
}

/// The same application over an explicit Resources clock, shared by the
/// registry and every Resources command exactly as process startup shares it.
pub fn resources_at(
    skill_store: Arc<dyn SkillStore>,
    clock: Arc<dyn awaken_time::WallClock>,
) -> ResourcesApplication {
    let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
    let storage = Arc::new(
        awaken_resource_store::SqliteResourceStore::in_memory()
            .expect("open ephemeral Resource Registry"),
    );
    let registry = Arc::new(RegistryApplication::new(storage.clone(), clock.clone()));
    ResourcesApplication::new(ResourceAuthorities::new(
        registry,
        files.clone(),
        files,
        Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
            clock.clone(),
        )),
        skill_store,
        storage,
        clock,
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
