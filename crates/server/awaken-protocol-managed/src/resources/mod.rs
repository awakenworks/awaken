// @formal surface requirement=product.public_dto.anti_corruption
// @formal reason: Static owner: the shared Managed Resource HTTP composition and official error
// @formal reason: discriminator projection. Dynamic path: File, Memory, and Skill application failures are
// @formal reason: mapped to the Anthropic wire error vocabulary without authenticating, authorizing, or
// @formal reason: introducing another Resource lifecycle authority. Evidence:
// @formal reason: product.public_dto.anti_corruption and the adjacent exhaustive status-discriminator
// @formal reason: test; this is a product-requirement classification, not a direct proof claim.
//! Resources HTTP routes over the canonical application services.

use std::sync::Arc;

use awaken_resource_contract::{
    FileApplicationService, MemoryStoreApplicationService, ResourcePurgeScheduler, SkillStore,
};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, Router};

use crate::types::ErrorResponse;

pub use files::files_router;
pub use memory_stores::memory_stores_router;
pub use repositories::repositories_router;
pub use skills::skills_router;

mod files;
pub(crate) mod flavor;
pub(crate) mod memory_stores;
mod repositories;
mod skills;

fn managed_resource_error_type(status: StatusCode) -> &'static str {
    match status {
        StatusCode::UNAUTHORIZED => "authentication_error",
        StatusCode::FORBIDDEN => "permission_error",
        StatusCode::NOT_FOUND => "not_found_error",
        StatusCode::CONFLICT => "conflict_error",
        StatusCode::TOO_MANY_REQUESTS => "rate_limit_error",
        status if status.is_server_error() => "api_error",
        _ => "invalid_request_error",
    }
}

pub(super) fn managed_resource_error(
    status: StatusCode,
    message: impl Into<String>,
) -> axum::response::Response {
    (
        status,
        Json(ErrorResponse::new(
            managed_resource_error_type(status),
            message,
        )),
    )
        .into_response()
}

pub struct ResourcesRouterInput {
    pub repositories: Arc<dyn awaken_resource_contract::RepositoryApplicationService>,
    pub files: Arc<dyn FileApplicationService>,
    pub memory: awaken_resource_contract::MemoryManagementContent,
    pub memory_stores: Arc<dyn MemoryStoreApplicationService>,
    pub skills: Option<Arc<dyn SkillStore>>,
    pub purge: Arc<dyn ResourcePurgeScheduler>,
    /// The Resources context's clock, the one its File application stamps with.
    pub clock: Arc<dyn awaken_time::WallClock>,
}

/// Mount all public resource families once. File, Memory, and Skill retain their
/// independent aggregates and routes; this function only joins their HTTP routes.
pub fn resources_router(input: ResourcesRouterInput) -> Router {
    files_router(input.files.clone(), input.clock.clone())
        .merge(memory_stores_router(
            input.memory,
            input.memory_stores,
            input.clock.clone(),
        ))
        .merge(repositories_router(input.repositories))
        .merge(skills_router(input.skills, input.purge, input.clock))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_resource_application::ResourceAuthorities;

    #[test]
    fn resource_errors_follow_the_official_status_discriminators() {
        // Cause/effect table: C1 client syntax, C2 authentication, C3 policy,
        // C4 absent identity, C5 aggregate conflict, C6 rate limit, C7 server
        // fault. Effects are the exact official SDK inner error discriminators.
        // One mapper prevents File, Skill, and Memory adapters from returning
        // the same HTTP status with incompatible Anthropic error bodies.
        for (status, expected) in [
            (StatusCode::BAD_REQUEST, "invalid_request_error"),
            (StatusCode::UNAUTHORIZED, "authentication_error"),
            (StatusCode::FORBIDDEN, "permission_error"),
            (StatusCode::NOT_FOUND, "not_found_error"),
            (StatusCode::CONFLICT, "conflict_error"),
            (StatusCode::TOO_MANY_REQUESTS, "rate_limit_error"),
            (StatusCode::INTERNAL_SERVER_ERROR, "api_error"),
        ] {
            assert_eq!(managed_resource_error_type(status), expected);
        }
    }

    #[tokio::test]
    async fn one_resources_router_mounts_each_public_family_once() {
        // Cause/effect route table: with one trusted Workspace stamp, R1 File
        // path -> File adapter; R2 Memory path -> Memory adapter; R3 Skill path ->
        // Skill adapter; R4 removed extension -> 404. One merge point prevents
        // per-process route-family drift.
        use tower::ServiceExt;

        let files = Arc::new(awaken_file_store::InMemoryFileStore::new());
        let clock = awaken_time::system_clock();
        let storage = Arc::new(
            awaken_resource_store::SqliteResourceStore::in_memory()
                .await
                .expect("resource registry"),
        );
        let registry = Arc::new(awaken_resource_application::RegistryApplication::new(
            storage.clone(),
            clock.clone(),
        ));
        let application =
            awaken_resource_application::ResourcesApplication::new(ResourceAuthorities::new(
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
                    Arc::new(
                        awaken_resource_application::memory::InvalidatingMemoryRepository::new(
                            Arc::new(awaken_memory_store::VolatileMemoryRepository::new(
                                clock.clone(),
                            )),
                            Arc::new(MemoryEvents::default()),
                        ),
                    )
                },
                Arc::new(awaken_skill_store::InMemorySkillStore::new()),
                storage,
                clock,
            ));
        let authorities = application.authorities();
        let router = resources_router(ResourcesRouterInput {
            repositories: application.repositories(Arc::new(
                awaken_resource_contract::UnavailableRepositoryCredentialValidator,
            )),
            files: application.files(),
            memory: authorities.memory_management_content(),
            memory_stores: application.memory_stores(),
            skills: Some(authorities.skill_store()),
            purge: application.purge_scheduler(),
            clock: authorities.clock(),
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
                    .uri("/v1/awaken/files/output-capture?scope_id=session")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND, "R4");
    }
}
