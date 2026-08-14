use super::*;
use axum::http::header::AUTHORIZATION;
use tower::ServiceExt as _;

#[tokio::test]
async fn application_protocols_use_one_application_authentication_edge() {
    // Cause/effect graph: C1 a service/runtime route enters process composition
    // -> E1 the existing service IAM edge authenticates it; C2 an AI SDK or
    // AG-UI route already carries the application guard -> E2 composition
    // merges it after that service edge, preserving its opaque bearer; C3 an
    // application route is accidentally sent through a management guard -> E3
    // fail closed instead of accepting either credential under two meanings.
    //
    // Decision table:
    // | Rule | Route owner | Presented bearer | Effect |
    // | R1 | service IAM | valid service token | 200 through service guard |
    // | R2 | application guard | opaque application token | 200, bearer preserved |
    // | R3 | application family at service guard | any | explicit composition denial |
    // The real application guard's missing/invalid/protocol/thread rules are
    // covered by awaken-authz-enforce's application_access suite; this test owns
    // only the process composition boundary that previously stacked both PEPs.
    async fn ok() -> StatusCode {
        StatusCode::OK
    }
    async fn application_bearer(headers: HeaderMap) -> StatusCode {
        match headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
        {
            Some("Bearer app-capability") => StatusCode::OK,
            _ => StatusCode::UNAUTHORIZED,
        }
    }

    assert_eq!(
        action_for(&Method::POST, "/v1/ai-sdk/chat"),
        Some(RouteAuthz::Application)
    );
    assert_eq!(
        action_for(&Method::POST, "/v1/ag-ui"),
        Some(RouteAuthz::Application)
    );
    assert!(matches!(
        action_for(&Method::POST, "/v1/application-access-tokens"),
        Some(RouteAuthz::HostedRuntime {
            action: RUN_CREATE,
            ..
        })
    ));

    let dir = tempfile::tempdir().unwrap();
    let iam = embedded_iam(dir.path());
    let service_token = std::fs::read_to_string(dir.path().join(ADMIN_TOKEN_FILE)).unwrap();
    let app = crate::protect_runtime_protocol_routers(
        Router::new().route("/v1/sessions", axum::routing::get(ok)),
        Router::new().route("/v1/ai-sdk/chat", axum::routing::post(application_bearer)),
        Some(iam.clone()),
        None,
    );
    assert_eq!(
        app.clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/sessions")
                    .header(AUTHORIZATION, format!("Bearer {}", service_token.trim()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status(),
        StatusCode::OK,
        "R1"
    );
    assert_eq!(
        app.clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/ai-sdk/chat")
                    .header(AUTHORIZATION, "Bearer app-capability")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status(),
        StatusCode::OK,
        "R2"
    );
    let wrongly_layered = Router::new()
        .route("/v1/ai-sdk/chat", axum::routing::post(ok))
        .layer(axum::middleware::from_fn_with_state(iam, management_guard));
    assert_eq!(
        wrongly_layered
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/ai-sdk/chat")
                    .header(AUTHORIZATION, format!("Bearer {}", service_token.trim()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN,
        "R3"
    );
}
