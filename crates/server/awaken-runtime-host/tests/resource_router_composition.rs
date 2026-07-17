//! A composition-root smoke test: the per-plane resource routers a binary merges
//! over ONE `SharedHost` (`/v1/memory_stores`, `/v1/files`, `/v1/skills`) plus the
//! fixed `/v1/models` directory compose into a single service without route
//! conflicts, and each plane's entry route answers on the assembled app. This is the
//! wiring the real composition root does — proven here in miniature so a route that
//! silently drops out of the merge is caught.

use std::sync::Arc;

use awaken_managed_routers::{default_models, files_router, models_router};
use awaken_runtime_contract::llm::{ChatRequest, ChatResponse, LlmExecutor, Result as LlmResult};
use awaken_runtime_host::{SharedHost, memory_stores_router, skills_router};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

struct NoLlm;
#[async_trait::async_trait]
impl LlmExecutor for NoLlm {
    async fn infer(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
        unreachable!("the resource planes never infer")
    }
}

async fn status(app: &Router, uri: &str) -> StatusCode {
    app.clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn the_resource_planes_merge_over_one_host_without_route_conflicts() {
    let host = Arc::new(SharedHost::new(Arc::new(NoLlm), "test"));

    // Merge the same way the assembly binary does: every plane's router over the one
    // shared host, plus the static model directory.
    let app = Router::new()
        .merge(memory_stores_router(host.clone()))
        .merge(files_router(host.clone()))
        .merge(skills_router(host.clone()))
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
