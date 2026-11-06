//! The Managed protocol entry admitted by Org bucket: the access edge's
//! decision with the Managed classification and Anthropic encoding mounted
//! together, as `protect_management_router` mounts them.

use std::sync::Arc;

use awaken_authz_enforce::{OrgAdmissionEdge, OrgRateLimiter, admit_by_org_bucket};
use awaken_tenancy::org_admission::{OrgRateLimits, OrgRequestAdmission};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::{delete, post};
use http_body_util::BodyExt;
use tower::ServiceExt;

use crate::MANAGED_ORG_ADMISSION;

#[tokio::test]
async fn classified_request_without_trusted_workspace_fails_before_admission() {
    // Causes: C1 request classifies as Managed Create; C2 no edge-authored
    // WorkspaceScope exists. Effects: E1 503; E2 handler does not run; E3
    // limiter is not given a caller-invented fallback tenant. Decision rule
    // A1=C1+C2 -> E1+E2+E3. The scoped success and shared-bucket rules are
    // exercised by the following compatibility test.
    let limiter: Arc<dyn OrgRequestAdmission> = Arc::new(OrgRateLimiter::for_organization(
        "org",
        awaken_time::system_clock(),
    ));
    let app = Router::new()
        .route("/v1/sessions", post(|| async { StatusCode::CREATED }))
        .layer(axum::middleware::from_fn_with_state(
            OrgAdmissionEdge::new(limiter, MANAGED_ORG_ADMISSION),
            admit_by_org_bucket,
        ));
    let response = app
        .oneshot(Request::post("/v1/sessions").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "A1");
}

#[tokio::test]
async fn native_and_acp_session_creates_share_the_organization_bucket() {
    // Runtime-axis cause graph: Native and ACP differ only after Session
    // creation. Rate limiting is an organization ingress effect, so N1 Native
    // create and N2 ACP create consume the same capacity; N3 any third create
    // is rejected before its handler. The read bucket remains independently
    // available and every metered response exposes limit state.
    async fn accepted(body: String) -> String {
        body
    }
    let limiter: Arc<dyn OrgRequestAdmission> = Arc::new(OrgRateLimiter::with_limits(
        "org_shared",
        OrgRateLimits {
            create_per_minute: 2,
            read_per_minute: 1,
        },
        awaken_time::system_clock(),
    ));
    let app = Router::new()
        .route("/v1/sessions", post(accepted).get(|| async { "sessions" }))
        .layer(axum::middleware::from_fn_with_state(
            OrgAdmissionEdge::new(limiter, MANAGED_ORG_ADMISSION),
            admit_by_org_bucket,
        ))
        .layer(axum::Extension(
            awaken_runtime_contract::runs::scope::WorkspaceScope("workspace_shared".into()),
        ));
    for (rule, agent) in [("N1", "native:claude"), ("N2", "acp:claude-code")] {
        let response = app
            .clone()
            .oneshot(
                Request::post("/v1/sessions")
                    .body(Body::from(agent))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{rule}");
    }
    let response = app
        .clone()
        .oneshot(
            Request::post("/v1/sessions")
                .body(Body::from("native:other"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS, "N3");
    assert_eq!(response.headers()["retry-after"], "30", "N3");
    assert_eq!(
        response.headers()["anthropic-ratelimit-requests-limit"],
        "2",
        "N3"
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["error"]["type"], "rate_limit_error", "N3");

    let read = app
        .oneshot(Request::get("/v1/sessions").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(read.status(), StatusCode::OK, "independent read bucket");
    assert_eq!(read.headers()["anthropic-ratelimit-requests-limit"], "1");
}

/// Application-token admission table: M1 a POST to the exact collection
/// consumes the existing organization Create bucket; M2 another Managed
/// Create then returns 429 before its handler; M3 management DELETE is not
/// a mint and passes through. The application surface introduces no second
/// limiter, bucket, or bypass classification.
#[tokio::test]
async fn application_access_mint_uses_the_existing_create_bucket() {
    let limiter: Arc<dyn OrgRequestAdmission> = Arc::new(OrgRateLimiter::with_limits(
        "org_application",
        OrgRateLimits {
            create_per_minute: 1,
            read_per_minute: 1,
        },
        awaken_time::system_clock(),
    ));
    let app = Router::new()
        .route(
            "/v1/application-access-tokens",
            post(|| async { StatusCode::CREATED }),
        )
        .route(
            "/v1/application-access-tokens/{id}",
            delete(|| async { StatusCode::NO_CONTENT }),
        )
        .route("/v1/sessions", post(|| async { StatusCode::CREATED }))
        .layer(axum::middleware::from_fn_with_state(
            OrgAdmissionEdge::new(limiter, MANAGED_ORG_ADMISSION),
            admit_by_org_bucket,
        ))
        .layer(axum::Extension(
            awaken_runtime_contract::runs::scope::WorkspaceScope("workspace_application".into()),
        ));

    let mint = app
        .clone()
        .oneshot(
            Request::post("/v1/application-access-tokens")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(mint.status(), StatusCode::CREATED, "M1");

    let another_create = app
        .clone()
        .oneshot(Request::post("/v1/sessions").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(another_create.status(), StatusCode::TOO_MANY_REQUESTS, "M2");

    let revoke = app
        .oneshot(
            Request::delete("/v1/application-access-tokens/aat_1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(revoke.status(), StatusCode::NO_CONTENT, "M3");
}
