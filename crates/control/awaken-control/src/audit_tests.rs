use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use awaken_config_service::{ConfigService, StaticToolCatalog};
use awaken_config_store::SqliteConfigStore;
use awaken_tenancy::{ScopeId, WorkspaceScope};
use axum::middleware::Next;
use axum::routing::post;
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;

use super::*;

fn audit_plane() -> ConfigPlane {
    ConfigPlane::new(
        Arc::new(ConfigService::new()),
        Arc::new(SqliteConfigStore::open_in_memory().unwrap()),
        Arc::new(StaticToolCatalog(vec![])),
    )
}

fn mutation(call_id: &str, body: impl Into<Body>) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri("/v1/mutate")
        .header("idempotency-key", call_id)
        .body(body.into())
        .unwrap()
}

async fn stamp_workspace(mut request: Request<Body>, next: Next) -> Response {
    request
        .extensions_mut()
        .insert(WorkspaceScope("ws_authenticated".into()));
    next.run(request).await
}

#[tokio::test]
async fn authenticated_workspace_selects_the_durable_audit_partition() {
    let plane = audit_plane();
    let app = Router::new()
        .route("/v1/mutate", post(|| async { StatusCode::NO_CONTENT }))
        .layer(axum::middleware::from_fn_with_state(
            plane.clone(),
            durable_management_audit,
        ))
        .layer(axum::middleware::from_fn(stamp_workspace));

    let response = app.oneshot(mutation("scope-1", "{}")).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let entry = plane
        .get_management_audit(
            &ScopeId::from("ws_authenticated"),
            "http:POST:/v1/mutate",
            "scope-1",
        )
        .await
        .unwrap()
        .expect("authenticated workspace audit");
    assert!(entry.business_committed);
    assert!(
        plane
            .get_management_audit(
                &ScopeId::from(DEFAULT_SCOPE),
                "http:POST:/v1/mutate",
                "scope-1",
            )
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn rejected_outer_authentication_cannot_create_an_audit_intent() {
    async fn reject(_request: Request<Body>, _next: Next) -> Response {
        StatusCode::UNAUTHORIZED.into_response()
    }

    let plane = audit_plane();
    let app = Router::new()
        .route("/v1/mutate", post(|| async { StatusCode::NO_CONTENT }))
        .layer(axum::middleware::from_fn_with_state(
            plane.clone(),
            durable_management_audit,
        ))
        .layer(axum::middleware::from_fn(reject));

    let response = app.oneshot(mutation("rejected-1", "{}")).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(
        plane
            .get_management_audit(
                &ScopeId::from(DEFAULT_SCOPE),
                "http:POST:/v1/mutate",
                "rejected-1",
            )
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn replayed_or_conflicting_call_id_fails_closed_without_repeating_business_work() {
    let plane = audit_plane();
    let business_calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route(
            "/v1/mutate",
            post({
                let business_calls = business_calls.clone();
                move || {
                    let business_calls = business_calls.clone();
                    async move {
                        business_calls.fetch_add(1, Ordering::SeqCst);
                        StatusCode::NO_CONTENT
                    }
                }
            }),
        )
        .layer(axum::middleware::from_fn_with_state(
            plane,
            durable_management_audit,
        ));

    let first = app
        .clone()
        .oneshot(mutation("stable-1", "one"))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::NO_CONTENT);
    let replay = app
        .clone()
        .oneshot(mutation("stable-1", "one"))
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::CONFLICT);
    assert_eq!(business_calls.load(Ordering::SeqCst), 1);

    let conflict = app.oneshot(mutation("stable-1", "two")).await.unwrap();
    assert_eq!(conflict.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(business_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn oversized_audit_body_is_rejected_before_business_or_audit() {
    let plane = audit_plane();
    let business_calls = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route(
            "/v1/mutate",
            post({
                let business_calls = business_calls.clone();
                move || {
                    let business_calls = business_calls.clone();
                    async move {
                        business_calls.fetch_add(1, Ordering::SeqCst);
                        StatusCode::NO_CONTENT
                    }
                }
            }),
        )
        .layer(axum::middleware::from_fn_with_state(
            plane.clone(),
            durable_management_audit,
        ));

    let response = app
        .oneshot(mutation("large-1", "x".repeat(MAX_AUDITED_BODY + 1)))
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body:?}");
    assert_eq!(business_calls.load(Ordering::SeqCst), 0);
    assert!(
        plane
            .get_management_audit(
                &ScopeId::from(DEFAULT_SCOPE),
                "http:POST:/v1/mutate",
                "large-1",
            )
            .await
            .unwrap()
            .is_none()
    );
}
