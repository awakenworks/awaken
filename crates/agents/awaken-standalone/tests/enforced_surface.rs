//! Deployment-form-independent e2e for the open standalone: it drives the
//! assembled, guarded session surface over the built-in `HelloModel` — no
//! external provider, no durable store, no environment. The same assertions hold
//! wherever this router is mounted, which is the point: enforcement is a property
//! of the open runtime, not of a deployment.

use std::sync::Arc;

use awaken_standalone::{HelloModel, build};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

/// Drive one request through a freshly-built standalone, optionally bearing the
/// seeded api key. Returns the HTTP status.
async fn call(method: &str, path: &str, with_key: bool) -> StatusCode {
    let standalone = build(Arc::new(HelloModel));
    let mut builder = Request::builder().method(method).uri(path);
    if with_key {
        builder = builder.header("authorization", format!("Bearer {}", standalone.api_token));
    }
    let request = builder.body(Body::empty()).expect("request");
    standalone
        .router
        .oneshot(request)
        .await
        .expect("router call")
        .status()
}

#[tokio::test]
async fn the_bare_session_surface_requires_a_credential() {
    assert_eq!(
        call("POST", "/v1/sessions", false).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn the_bare_session_surface_admits_the_seeded_api_key() {
    // The guard passed (auth + workspace-scope authorize) — the request reached
    // the handler, so whatever it answers, it is neither 401 nor 403.
    let status = call("POST", "/v1/sessions", true).await;
    assert_ne!(status, StatusCode::UNAUTHORIZED);
    assert_ne!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_project_session_surface_requires_a_credential() {
    assert_eq!(
        call("POST", "/projects/local/v1/sessions", false).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn the_project_session_surface_admits_the_seeded_api_key() {
    let status = call("POST", "/projects/local/v1/sessions", true).await;
    assert_ne!(status, StatusCode::UNAUTHORIZED);
    assert_ne!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn an_unauthored_project_is_not_found() {
    assert_eq!(
        call("POST", "/projects/ghost/v1/sessions", true).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_read_on_the_bare_surface_also_requires_a_credential() {
    assert_eq!(
        call("GET", "/v1/sessions/sesn_1", false).await,
        StatusCode::UNAUTHORIZED
    );
}
