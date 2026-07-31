use std::collections::HashMap;
use std::sync::Arc;

use awaken_authz_enforce::{
    ApplicationAccessStore, ApplicationGrant, ApplicationThreadBinding, application_guard,
};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Json, Path};
use axum::http::{Request, StatusCode};
use axum::routing::{get, post};
use serde_json::{Value, json};
use tower::ServiceExt;

fn grant(protocols: &[&str], operations: &[&str]) -> ApplicationGrant {
    ApplicationGrant {
        authority_id: "customer-app".to_string(),
        application_scope: "project-a".to_string(),
        actor_key: Some("opaque-user-7".to_string()),
        protocols: protocols.iter().map(|value| (*value).to_string()).collect(),
        operations: operations
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
        thread_bindings: HashMap::from([(
            "customer-thread".to_string(),
            ApplicationThreadBinding {
                managed_session_id: "sesn_existing".to_string(),
                agent_id: "support".to_string(),
            },
        )]),
    }
}

fn app(store: Arc<ApplicationAccessStore>) -> Router {
    async fn echo_run(
        Path(thread): Path<String>,
        resolved: Option<axum::Extension<awaken_tenancy::ResolvedResourceId>>,
        agent: Option<axum::Extension<awaken_tenancy::ResolvedAgentId>>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        Json(json!({
            "thread": thread,
            "resolved": resolved.map(|axum::Extension(value)| value.0),
            "agent": agent.map(|axum::Extension(value)| value.0),
            "body": body,
        }))
    }
    async fn echo_history(
        Path(thread): Path<String>,
        resolved: Option<axum::Extension<awaken_tenancy::ResolvedResourceId>>,
    ) -> Json<Value> {
        Json(json!({
            "thread": thread,
            "resolved": resolved.map(|axum::Extension(value)| value.0),
        }))
    }
    async fn echo_ag_ui(
        resolved: Option<axum::Extension<awaken_tenancy::ResolvedResourceId>>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        Json(json!({
            "resolved": resolved.map(|axum::Extension(value)| value.0),
            "body": body,
        }))
    }

    Router::new()
        .route("/v1/ai-sdk/threads/{thread}/runs", post(echo_run))
        .route("/v1/ai-sdk/threads/{thread}/messages", get(echo_history))
        .route("/v1/ag-ui", post(echo_ag_ui))
        .route("/v1/ag-ui/threads/{thread}/messages", get(echo_history))
        .layer(axum::middleware::from_fn_with_state(
            store,
            application_guard,
        ))
}

fn request(method: &str, path: &str, token: Option<&str>, body: Value) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    if method == "POST" {
        builder = builder.header("content-type", "application/json");
    }
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let body = if method == "POST" {
        Body::from(body.to_string())
    } else {
        Body::empty()
    };
    builder.body(body).unwrap()
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn mint(store: &ApplicationAccessStore, id: &str, grant: ApplicationGrant) -> String {
    store.mint(id.into(), "ws-1".into(), None, grant).unwrap()
}

/// Cause-effect graph: authenticated token (C1), protocol permission (C2),
/// operation permission (C3), exact external-thread binding (C4), matching
/// path/body thread (C5), and matching frozen Agent (C6) gate the sole effects:
/// dispatch to the bound Managed Session (E1) or fail before dispatch (E2).
/// Decision rules covered here: R1 all causes true -> E1 with exact rewrite;
/// R2 C1 false -> 401/E2. Later tests cover R3-R7 for each other false cause.
#[tokio::test]
async fn exact_binding_rewrites_to_the_existing_managed_session() {
    let store = Arc::new(ApplicationAccessStore::new());
    let token = mint(
        &store,
        "valid",
        grant(&["ai-sdk"], &["thread.run", "thread.messages.read"]),
    );
    let router = app(store.clone());

    let response = router
        .clone()
        .oneshot(request(
            "POST",
            "/v1/ai-sdk/threads/customer-thread/runs",
            Some(&token),
            json!({ "threadId": "customer-thread", "messages": [] }),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["thread"], "customer-thread");
    assert_eq!(body["resolved"], "sesn_existing");
    assert_eq!(body["body"]["threadId"], "sesn_existing");
    assert_eq!(body["body"]["agentId"], "support");
    assert_eq!(body["agent"], "support");
    assert!(!body["resolved"].as_str().unwrap().starts_with("app_"));

    let missing = router
        .clone()
        .oneshot(request(
            "GET",
            "/v1/ai-sdk/threads/customer-thread/messages",
            None,
            Value::Null,
        ))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
    store.revoke("valid").unwrap();
    let revoked = router
        .oneshot(request(
            "GET",
            "/v1/ai-sdk/threads/customer-thread/messages",
            Some(&token),
            Value::Null,
        ))
        .await
        .unwrap();
    assert_eq!(revoked.status(), StatusCode::UNAUTHORIZED);
}

/// Decision rules from the graph above: R3 protocol false, R4 operation false,
/// and R5 binding false each produce 403/E2. The protocol rule proves an AI SDK
/// credential cannot become an AG-UI credential even for the same Session.
#[tokio::test]
async fn protocol_operation_and_binding_permissions_fail_closed() {
    let store = Arc::new(ApplicationAccessStore::new());
    let run_only = mint(&store, "limited", grant(&["ai-sdk"], &["thread.run"]));
    let router = app(store);

    let ag_ui = router
        .clone()
        .oneshot(request(
            "POST",
            "/v1/ag-ui",
            Some(&run_only),
            json!({ "threadId": "customer-thread", "messages": [] }),
        ))
        .await
        .unwrap();
    assert_eq!(ag_ui.status(), StatusCode::FORBIDDEN);

    let history = router
        .clone()
        .oneshot(request(
            "GET",
            "/v1/ai-sdk/threads/customer-thread/messages",
            Some(&run_only),
            Value::Null,
        ))
        .await
        .unwrap();
    assert_eq!(history.status(), StatusCode::FORBIDDEN);

    let unbound = router
        .oneshot(request(
            "POST",
            "/v1/ai-sdk/threads/another-thread/runs",
            Some(&run_only),
            json!({ "messages": [] }),
        ))
        .await
        .unwrap();
    assert_eq!(unbound.status(), StatusCode::FORBIDDEN);
}

/// Decision rules from the graph above: R6 path/body mismatch -> 400/E2 and
/// R7 requested Agent differs from the bound Session baseline -> 403/E2.
#[tokio::test]
async fn conflicting_request_identity_cannot_override_the_binding() {
    let store = Arc::new(ApplicationAccessStore::new());
    let token = mint(&store, "identity", grant(&["ai-sdk"], &["thread.run"]));
    let router = app(store);

    let thread_mismatch = router
        .clone()
        .oneshot(request(
            "POST",
            "/v1/ai-sdk/threads/customer-thread/runs",
            Some(&token),
            json!({ "threadId": "another-thread", "messages": [] }),
        ))
        .await
        .unwrap();
    assert_eq!(thread_mismatch.status(), StatusCode::BAD_REQUEST);

    let agent_mismatch = router
        .oneshot(request(
            "POST",
            "/v1/ai-sdk/threads/customer-thread/runs",
            Some(&token),
            json!({ "agentId": "billing", "messages": [] }),
        ))
        .await
        .unwrap();
    assert_eq!(agent_mismatch.status(), StatusCode::FORBIDDEN);
}

/// Route classification is an independent default-deny cause: no known exact
/// route means no operation exists to authorize, so the terminal effect is 403.
#[tokio::test]
async fn unknown_routes_are_not_inferred_from_the_http_method() {
    let store = Arc::new(ApplicationAccessStore::new());
    let token = mint(
        &store,
        "unknown",
        grant(&["ai-sdk"], &["thread.messages.read"]),
    );
    let response = app(store)
        .oneshot(request(
            "GET",
            "/v1/ai-sdk/threads/customer-thread/export",
            Some(&token),
            Value::Null,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
