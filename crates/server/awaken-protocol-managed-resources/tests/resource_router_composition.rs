//! A composition-root smoke test: the per-plane resource routers a binary merges
//! over ONE canonical Resources component (`/v1/memory_stores`, `/v1/files`, `/v1/skills`) plus the
//! fixed `/v1/models` directory compose into a single service without route
//! conflicts, and each plane's entry route answers on the assembled app. This is the
//! wiring the real composition root does — proven here in miniature so a route that
//! silently drops out of the merge is caught.

use std::sync::Arc;

use awaken_protocol_managed_resources::{
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
async fn the_resource_planes_merge_over_one_host_without_route_conflicts() {
    let resources = support::ephemeral_resources();
    let ports = resources.ports();

    // Merge the same way the assembly binary does: every plane's router over the one
    // shared host, plus the static model directory.
    let app = Router::new()
        .merge(resources_router(ResourcesRouterInput {
            files: resources.files(),
            memories: ports.memory_repository(),
            memory_stores: resources.memory_stores(),
            skills: Some(ports.skill_store()),
            purge: resources.purge_scheduler(),
        }))
        .merge(models_router(Arc::new(default_models())));

    // Each plane's list entrypoint answers on the assembled app (200, not a 404 from a
    // dropped route). The listing bodies themselves are proven per-router elsewhere.
    assert_eq!(status(&app, "/v1/memory_stores").await, StatusCode::OK);
    assert_eq!(status(&app, "/v1/files").await, StatusCode::OK);
    assert_eq!(status(&app, "/v1/skills").await, StatusCode::OK);
    assert_eq!(status(&app, "/v1/models").await, StatusCode::OK);

    // A route no plane owns is a clean 404 on the merged app (the merge did not swallow
    // the fallback).
    assert_eq!(status(&app, "/v1/nonexistent").await, StatusCode::NOT_FOUND);
}
