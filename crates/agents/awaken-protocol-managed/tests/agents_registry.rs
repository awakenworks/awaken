//! The agent registry over HTTP: create/retrieve/update/list/archive + version
//! history, including model normalization and optimistic-concurrency updates.

use std::sync::Arc;

use awaken_protocol_managed::{AgentRegistryState, agents_router};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

fn app() -> Router {
    agents_router(Arc::new(AgentRegistryState::new()))
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

#[tokio::test]
async fn agent_crud_versions_and_optimistic_concurrency() {
    let app = app();

    // Create — a bare model string normalizes to a ModelConfig object.
    let (s, a) = call(
        &app,
        "POST",
        "/v1/agents",
        Some(json!({
            "name": "assistant",
            "model": "claude-opus-4-8",
            "system": "be helpful",
            "tools": [{ "type": "custom", "name": "echo" }]
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(a["type"], "agent");
    assert_eq!(a["version"], 1);
    assert_eq!(a["model"]["id"], "claude-opus-4-8");
    assert_eq!(a["system"], "be helpful");
    assert!(a["archived_at"].is_null());
    let id = a["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("agent_"));

    // A stale version is rejected (optimistic concurrency).
    let (s, _) = call(
        &app,
        "POST",
        &format!("/v1/agents/{id}"),
        Some(json!({ "version": 99, "name": "renamed" })),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);

    // The correct version updates and bumps to v2.
    let (s, up) = call(
        &app,
        "POST",
        &format!("/v1/agents/{id}"),
        Some(json!({ "version": 1, "name": "renamed", "model": { "id": "claude-sonnet-5", "speed": "fast" } })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(up["version"], 2);
    assert_eq!(up["name"], "renamed");
    assert_eq!(up["model"]["id"], "claude-sonnet-5");
    assert_eq!(up["model"]["speed"], "fast");

    // Version history has both snapshots (v1 then v2).
    let (s, versions) = call(&app, "GET", &format!("/v1/agents/{id}/versions"), None).await;
    assert_eq!(s, StatusCode::OK);
    let vs: Vec<u64> = versions["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["version"].as_u64().unwrap())
        .collect();
    assert_eq!(vs, vec![1, 2]);

    // Archive → v3 with archived_at.
    let (s, arch) = call(&app, "POST", &format!("/v1/agents/{id}/archive"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(arch["archived_at"].is_string());
    assert_eq!(arch["version"], 3);

    // List returns the agent.
    let (s, page) = call(&app, "GET", "/v1/agents", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(page["data"].as_array().unwrap()[0]["id"], id);

    // Missing required fields + unknown id.
    let (s, _) = call(&app, "POST", "/v1/agents", Some(json!({ "name": "x" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "model is required");
    let (s, _) = call(&app, "GET", "/v1/agents/agent_missing", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
