//! A process-level smoke test: the per-plane resource routers a binary merges
//! over ONE canonical Resources application (`/v1/memory_stores`, `/v1/files`, `/v1/skills`) plus the
//! fixed `/v1/models` response form a single service without route
//! conflicts, and each plane's entry route answers on the same app. This is the
//! route set the real server exposes — proven here in miniature so a route that
//! silently drops out of the merge is caught.

use std::sync::Arc;

use awaken_protocol_managed::{
    ResourcesRouterInput, default_models, models_router, resources_router,
};
use awaken_tenancy::WorkspaceScope;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

mod support;

async fn status(app: &Router, uri: &str) -> StatusCode {
    let mut request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    request
        .extensions_mut()
        .insert(WorkspaceScope("test".into()));
    app.clone().oneshot(request).await.unwrap().status()
}

#[tokio::test]
async fn resource_routes_share_one_application_without_conflicts() {
    let resources = support::resources::ephemeral_resources();
    let authorities = resources.authorities();

    // FMECA: FM1 one route family reopens its own Resource authority; FM2 a
    // route disappears during process startup; FM3 route merging swallows the
    // unknown-path fallback. Causes: C1 one ResourcesApplication supplies all
    // services; C2 File/Memory/Skill routes are mounted; C3 fixed model response
    // is mounted; C4 unknown route requested. Effects: E1 every owned entry path
    // answers, E2 unknown path remains 404, E3 no second Resource application.
    // Cause graph: C1&&C2&&C3 -> E1+E3; C1&&C2&&C3&&C4 -> E2.
    // Decision table: R1 C1+C2+C3 => E1+E3; R2 R1+C4 => E2.
    let app = Router::new()
        .merge(resources_router(ResourcesRouterInput {
            files: resources.files(),
            memories: authorities.memory_repository(),
            memory_stores: resources.memory_stores(),
            skills: Some(authorities.skill_store()),
            purge: resources.purge_scheduler(),
        }))
        .merge(models_router(Arc::new(default_models())));

    // Each plane's list entrypoint answers on the same app (200, not a 404 from a
    // dropped route). The listing bodies themselves are proven per-router elsewhere.
    assert_eq!(status(&app, "/v1/memory_stores").await, StatusCode::OK);
    assert_eq!(status(&app, "/v1/files").await, StatusCode::OK);
    assert_eq!(status(&app, "/v1/skills").await, StatusCode::OK);
    assert_eq!(status(&app, "/v1/models").await, StatusCode::OK);

    // A route no plane owns is a clean 404 on the merged app (the merge did not swallow
    // the fallback).
    assert_eq!(status(&app, "/v1/nonexistent").await, StatusCode::NOT_FOUND);
}
