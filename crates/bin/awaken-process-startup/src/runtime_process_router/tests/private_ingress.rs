// @formal surface exclude=test_only
// @formal reason: Exercises the composed Coordinator listeners without owning production admission.
//! The actual split Coordinator keeps private reads on its addressed public edge.

use axum::{
    Json, Router,
    body::Body,
    http::{Request, StatusCode},
    routing::get,
};
use tower::ServiceExt as _;

use super::*;

#[tokio::test]
async fn configured_private_reads_are_addressed_and_never_mounted_on_the_private_listener() {
    let (stores, mut process, _) = fixture(Role::Coordinator).await;
    process.deployment.as_mut().unwrap().disable_local_pool = true;
    let directory = tempfile::tempdir().unwrap();
    let projection = directory.path().join("coordinator-service");
    std::fs::write(&projection, "fixture-coordinator").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let authority = tokio::spawn(async move {
        let app = Router::new().route(
            "/.well-known/jwks.json",
            get(|| async { Json(serde_json::json!({"keys": []})) }),
        );
        axum::serve(listener, app).await.unwrap();
    });
    let clock = stores.clock.clone();
    let ordinary = Arc::new(
        crate::HttpOrdinaryWorkspaceAdmission::from_projected_service_token(
            &format!("{base}/ordinary"),
            "awaken-runtime",
            projection.clone(),
        )
        .unwrap(),
    );
    let private_context = Arc::new(
        crate::HttpPrivateWorkspaceContext::new(&format!("{base}/context"), &ordinary).unwrap(),
    );
    process.managed_services = process
        .managed_services
        .with_ordinary_workspace_admission(ordinary)
        .with_private_workspace_context(private_context);
    let remote = tokio::task::spawn_blocking(move || {
        crate::RemoteManagementAuthz::connect_with_projected_service_token(
            base,
            "awaken-runtime".into(),
            "https://iam.example.test".into(),
            projection,
            clock,
        )
        .unwrap()
    })
    .await
    .unwrap();
    let routers = super::super::prepare_runtime_routers(
        stores,
        None,
        Some(remote),
        None,
        PublicationModelSupply::PublishedProviders,
        process,
    )
    .await
    .expect("prepare split Coordinator");
    let status = |router: Router, path: &'static str| async move {
        router
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    };
    // These requests have no bearer. Distinct answers identify the actual
    // ingress and its guard without touching downstream data or IAM.
    let addressed = "/v1/workspaces/assistant-a/private-assistant/sessions";
    assert_eq!(
        status(routers.public_router.clone(), addressed).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status(
            routers.public_router.clone(),
            "/v1/private-assistant/sessions"
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        status(routers.private_router.clone(), addressed).await,
        StatusCode::NOT_FOUND
    );
    let addressed_file = "/v1/workspaces/assistant-a/private-assistant/files/file-a/content";
    assert_eq!(
        status(routers.public_router.clone(), addressed_file).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status(routers.private_router.clone(), addressed_file).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status(
            routers.public_router.clone(),
            "/v1/workspaces/assistant-a/private-assistant/files"
        )
        .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        status(
            routers.public_router.clone(),
            "/v1/workspaces/assistant-a/private-assistant/sessions/a/events/stream"
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
    let stop = |router: Router| async move {
        router
            .oneshot(
                Request::post("/v1/workspaces/assistant-a/private-assistant/sessions/a/stop")
                    .header("idempotency-key", "original-interrupt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    };
    assert_eq!(
        stop(routers.public_router.clone()).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        stop(routers.private_router.clone()).await,
        StatusCode::NOT_FOUND
    );
    routers
        .service_lifecycle
        .shutdown(Duration::from_secs(1))
        .await
        .unwrap();
    authority.abort();
}
