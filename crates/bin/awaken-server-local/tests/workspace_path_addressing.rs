//! ADR-0048 D3 / ADR-0051 end-to-end: the management plane addresses a workspace
//! resource by path (`/v1/workspaces/{ws}/…`), and that path scope both routes to
//! the flat handler AND fences cross-tenant access through the resource's
//! ownership guard. Driven over the real assembled management router.

use awaken_server_local::build_management_router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(
            body.map(|b| Body::from(serde_json::to_vec(&b).unwrap()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test]
async fn workspace_path_addresses_and_isolates_the_agent_registry() {
    let app = build_management_router().await;

    // Author an agent under ws_a via the D3 path form: the middleware rewrites
    // `/v1/workspaces/ws_a/agents` → `/v1/agents` and stamps the scope ws_a, so the
    // agent is owned by ws_a.
    let (status, agent) = call(
        &app,
        "POST",
        "/v1/workspaces/ws_a/agents",
        Some(json!({ "name": "a", "model": "kimi" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{agent:?}");
    let id = agent["id"].as_str().unwrap().to_string();

    // ws_a retrieves it via its own path.
    let (status, _) = call(
        &app,
        "GET",
        &format!("/v1/workspaces/ws_a/agents/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // ws_b is fenced (404, never 403) via its path — the registry ownership guard
    // reads the D3-stamped scope and refuses to disclose another tenant's agent.
    let (status, _) = call(
        &app,
        "GET",
        &format!("/v1/workspaces/ws_b/agents/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A cross-tenant write via the path is fenced too.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/v1/workspaces/ws_b/agents/{id}/archive"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The flat form resolves the seeded default scope, so it also cannot see the
    // ws_a-owned agent (the flat data plane is workspace-from-key elsewhere).
    let (status, _) = call(&app, "GET", &format!("/v1/agents/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_flat_management_request_is_untouched_by_the_path_middleware() {
    let app = build_management_router().await;
    // A flat create (no workspace path) owns under the seeded default and lists there.
    let (status, agent) = call(
        &app,
        "POST",
        "/v1/agents",
        Some(json!({ "name": "flat", "model": "kimi" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{agent:?}");
    let id = agent["id"].as_str().unwrap().to_string();
    let (status, _) = call(&app, "GET", &format!("/v1/agents/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn workspace_path_isolates_inference_profiles() {
    let app = build_management_router().await;
    let body = json!({
        "model_id": "kimi",
        "credential_binding": { "type": "none" }
    });

    // Author profile `prof1` under ws_a via the path form.
    let (status, _) = call(
        &app,
        "PUT",
        "/v1/workspaces/ws_a/config/inference-profiles/prof1",
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // ws_a reads its own profile.
    let (status, _) = call(
        &app,
        "GET",
        "/v1/workspaces/ws_a/config/inference-profiles/prof1",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // ws_b is fenced (404) on read of ws_a's profile…
    let (status, _) = call(
        &app,
        "GET",
        "/v1/workspaces/ws_b/config/inference-profiles/prof1",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // …and cannot overwrite it (cross-tenant author is fenced before the handler).
    let (status, _) = call(
        &app,
        "PUT",
        "/v1/workspaces/ws_b/config/inference-profiles/prof1",
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
