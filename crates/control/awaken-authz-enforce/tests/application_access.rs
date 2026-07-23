use std::collections::HashSet;
use std::sync::Arc;

use awaken_authz_enforce::{ApplicationAccessStore, ApplicationGrant, application_guard};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Json, Path};
use axum::http::{Request, StatusCode};
use axum::routing::{get, post};
use serde_json::{Value, json};
use tower::ServiceExt;

fn grant(scope: &str) -> ApplicationGrant {
    ApplicationGrant {
        authority_id: "customer-app".to_string(),
        application_scope: scope.to_string(),
        thread_namespace: "browser".to_string(),
        actor_key: Some("opaque-user-7".to_string()),
        operations: HashSet::from(["thread.run".to_string(), "thread.read".to_string()]),
        agent_ids: HashSet::from(["support".to_string()]),
        default_agent_id: Some("support".to_string()),
    }
}

fn app(store: Arc<ApplicationAccessStore>) -> Router {
    async fn echo(
        Path(thread): Path<String>,
        resolved: Option<axum::Extension<awaken_tenancy::ResolvedResourceId>>,
        agent: Option<axum::Extension<awaken_tenancy::ResolvedAgentId>>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        let resolved = resolved.map(|axum::Extension(thread)| thread.0);
        let agent = agent.map(|axum::Extension(agent)| agent.0);
        Json(json!({ "thread": thread, "resolved": resolved, "agent": agent, "body": body }))
    }
    Router::new()
        .route("/v1/ai-sdk/threads/{thread}/runs", post(echo))
        .route(
            "/v1/ai-sdk/threads/{thread}/messages",
            get(|| async { StatusCode::NO_CONTENT }),
        )
        .layer(axum::middleware::from_fn_with_state(
            store,
            application_guard,
        ))
}

fn request(method: &str, path: &str, token: Option<&str>, body: Value) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

async fn json_body(response: axum::response::Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn missing_and_revoked_application_tokens_are_rejected() {
    let store = Arc::new(ApplicationAccessStore::new());
    let router = app(store.clone());
    let missing = router
        .clone()
        .oneshot(request(
            "POST",
            "/v1/ai-sdk/threads/t1/runs",
            None,
            json!({ "messages": [] }),
        ))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

    let token = store
        .mint("one".into(), "ws-1".into(), None, grant("project-a"))
        .unwrap();
    store.revoke("one").unwrap();
    let revoked = router
        .oneshot(request(
            "POST",
            "/v1/ai-sdk/threads/t1/runs",
            Some(&token),
            json!({ "messages": [] }),
        ))
        .await
        .unwrap();
    assert_eq!(revoked.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn thread_ids_are_stable_inside_a_scope_and_isolated_between_scopes() {
    let store = Arc::new(ApplicationAccessStore::new());
    let token_a = store
        .mint("a".into(), "ws-1".into(), None, grant("project-a"))
        .unwrap();
    let token_a2 = store
        .mint("a2".into(), "ws-1".into(), None, grant("project-a"))
        .unwrap();
    let token_b = store
        .mint("b".into(), "ws-1".into(), None, grant("project-b"))
        .unwrap();
    let router = app(store);

    let mut internal = Vec::new();
    for token in [&token_a, &token_a2, &token_b] {
        let response = router
            .clone()
            .oneshot(request(
                "POST",
                "/v1/ai-sdk/threads/customer-thread/runs",
                Some(token),
                json!({ "messages": [] }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        internal.push(body["resolved"].as_str().unwrap().to_string());
        assert_eq!(body["body"]["agentId"], "support");
        assert_eq!(body["agent"], "support");
    }
    assert_eq!(internal[0], internal[1]);
    assert_ne!(internal[0], internal[2]);
    assert!(internal[0].starts_with("app_"));
}

#[tokio::test]
async fn operation_and_agent_allow_lists_fail_closed() {
    let store = Arc::new(ApplicationAccessStore::new());
    let mut run_only = grant("project-a");
    run_only.operations = HashSet::from(["thread.run".to_string()]);
    let token = store
        .mint("limited".into(), "ws-1".into(), None, run_only)
        .unwrap();
    let router = app(store);

    let read = router
        .clone()
        .oneshot(request(
            "GET",
            "/v1/ai-sdk/threads/t1/messages",
            Some(&token),
            Value::Null,
        ))
        .await
        .unwrap();
    assert_eq!(read.status(), StatusCode::FORBIDDEN);

    let other_agent = router
        .oneshot(request(
            "POST",
            "/v1/ai-sdk/threads/t1/runs",
            Some(&token),
            json!({ "agentId": "billing", "messages": [] }),
        ))
        .await
        .unwrap();
    assert_eq!(other_agent.status(), StatusCode::FORBIDDEN);
}
