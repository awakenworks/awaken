//! Production composition closure: a Deployment run creates and drives a real
//! Session through the same application service as `/v1/sessions`.

use std::sync::Arc;

use awaken_cli::build_management_router_with_model;
use awaken_scenario_host::EchoModel;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(value) => {
            request = request.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&value).unwrap())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test(flavor = "multi_thread")]
async fn deployment_run_creates_session_and_executes_initial_events() {
    // Production-composition cause/effect rules:
    // C1 published Agent + valid Deployment -> Agent latest version is frozen;
    // C2 manual run -> ordinary Session with Deployment id;
    // C3 initial user Event -> ordinary Event executor commits the model response.
    let app = build_management_router_with_model(Arc::new(EchoModel), "echo").await;
    let (status, agent) = call(
        &app,
        "POST",
        "/v1/agents",
        Some(json!({"name":"release-agent", "model":"echo"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "C1: {agent}");
    let agent_id = agent["id"].as_str().unwrap();
    let (status, deployment) = call(
        &app,
        "POST",
        "/v1/deployments",
        Some(json!({
            "agent": agent_id,
            "environment_id": "env_local",
            "name": "release-check",
            "initial_events": [{
                "type": "user.message",
                "content": [{ "type": "text", "text": "verify release" }]
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "C1: {deployment}");
    assert_eq!(deployment["agent"]["version"], agent["version"], "C1");

    let deployment_id = deployment["id"].as_str().unwrap();
    let (status, run) = call(
        &app,
        "POST",
        &format!("/v1/deployments/{deployment_id}/run"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "C2");
    assert!(run["error"].is_null(), "launch failed: {run}");
    let session_id = run["session_id"]
        .as_str()
        .expect("a successful deployment run returns its Session");

    let (status, session) = call(&app, "GET", &format!("/v1/sessions/{session_id}"), None).await;
    assert_eq!(status, StatusCode::OK, "C2");
    assert_eq!(session["deployment_id"], deployment_id);

    let mut observed = None;
    for _ in 0..200 {
        let (status, events) = call(
            &app,
            "GET",
            &format!("/v1/sessions/{session_id}/events"),
            None,
        )
        .await;
        if status == StatusCode::OK
            && events["data"].as_array().unwrap().iter().any(|event| {
                event["type"] == "agent.message"
                    && event["content"][0]["text"] == "Echo: verify release"
            })
        {
            observed = Some(events);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    assert!(observed.is_some(), "C3 initial Event eventually completes");
}
