//! One Resources HTTP adapter composition over the canonical application ports.

use std::sync::Arc;

use awaken_resource_contract::{
    FileApplicationService, MemoryRepository, MemoryStoreApplicationService,
    ResourcePurgeScheduler, SkillStore,
};
use axum::Router;

pub use files::files_router;
pub use memory_stores::memory_stores_router;
pub use skills::skills_router;

mod files;
mod memory_stores;
mod skills;

pub struct ResourcesRouterInput {
    pub files: Arc<dyn FileApplicationService>,
    pub memories: Arc<dyn MemoryRepository>,
    pub memory_stores: Arc<dyn MemoryStoreApplicationService>,
    pub skills: Option<Arc<dyn SkillStore>>,
    pub purge: Arc<dyn ResourcePurgeScheduler>,
}

/// Mount all public resource families once. File, Memory, and Skill retain their
/// independent aggregates and routes; this function owns only HTTP composition.
pub fn resources_router(input: ResourcesRouterInput) -> Router {
    files_router(input.files)
        .merge(memory_stores_router(input.memories, input.memory_stores))
        .merge(skills_router(input.skills, input.purge))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_resource_contract::ResourceDependencies;

    #[tokio::test]
    async fn one_resources_router_mounts_each_public_family_once() {
        // Cause/effect route table: with one trusted Workspace stamp, R1 File
        // path -> File adapter; R2 Memory path -> Memory adapter; R3 Skill path ->
        // Skill adapter; R4 unrelated path -> 404. One merge point prevents
        // per-process route-family drift.
        use tower::ServiceExt;

        let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
        let catalog = Arc::new(
            awaken_resource_store::SqliteResourceStore::in_memory().expect("resource catalog"),
        );
        let application = awaken_resource_application::ResourcesApplication::new(
            awaken_resource_contract::build_resource_component(ResourceDependencies {
                resource_catalog: catalog.clone(),
                file_store: files.clone(),
                file_catalog: files,
                memory_repository: Arc::new(awaken_memory_store::VolatileMemoryRepository::new()),
                skill_store: Arc::new(awaken_skill_store::InMemorySkillStore::new()),
                reclamation: catalog,
            }),
        );
        let ports = application.ports();
        let router = resources_router(ResourcesRouterInput {
            files: application.files(),
            memories: ports.memory_repository(),
            memory_stores: application.memory_stores(),
            skills: Some(ports.skill_store()),
            purge: application.purge_scheduler(),
        });
        for (path, rule) in [
            ("/v1/files", "R1"),
            ("/v1/memory_stores", "R2"),
            ("/v1/skills", "R3"),
        ] {
            let mut request = axum::http::Request::builder()
                .uri(path)
                .body(axum::body::Body::empty())
                .unwrap();
            request
                .extensions_mut()
                .insert(awaken_tenancy::WorkspaceScope("workspace".into()));
            let response = router.clone().oneshot(request).await.unwrap();
            assert_ne!(
                response.status(),
                axum::http::StatusCode::NOT_FOUND,
                "{rule}"
            );
        }
        let response = router
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/not-a-resource")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND, "R4");
    }
}
