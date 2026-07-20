//! Deployments + deployment runs over HTTP: CRUD, the pause/unpause/run/archive
//! lifecycle actions, agent-reference normalization, and run listing.

use std::sync::Arc;

use awaken_protocol_managed::{
    DeploymentLaunch, DeploymentLaunchOutcome, DeploymentSessionLauncher, DeploymentState,
    deployments_router,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

fn app() -> Router {
    struct Launcher;
    #[async_trait::async_trait]
    impl DeploymentSessionLauncher for Launcher {
        async fn launch(&self, request: DeploymentLaunch) -> DeploymentLaunchOutcome {
            DeploymentLaunchOutcome {
                session_id: Some(format!("sesn_for_{}", request.deployment_id)),
                error: None,
            }
        }
    }
    let state = Arc::new(DeploymentState::new());
    state.bind_launcher(Arc::new(Launcher));
    deployments_router(state)
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

async fn make_deployment(app: &Router) -> String {
    let (s, d) = call(
        app,
        "POST",
        "/v1/deployments",
        Some(json!({
            "agent": "agent_x",
            "environment_id": "env_1",
            "name": "nightly",
            "initial_events": [{ "type": "user.message", "content": [{ "type": "text", "text": "go" }] }]
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    d["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn deployment_lifecycle_and_runs() {
    let app = app();

    // Create — agent string normalizes to a reference; status defaults active.
    let (s, d) = call(
        &app,
        "POST",
        "/v1/deployments",
        Some(json!({
            "agent": "agent_x",
            "environment_id": "env_1",
            "name": "nightly",
            "initial_events": [{ "type": "user.message", "content": [{ "type": "text", "text": "go" }] }],
            "vault_ids": ["vlt_1"]
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(d["type"], "deployment");
    assert_eq!(d["status"], "active");
    assert!(d["paused_reason"].is_null());
    assert_eq!(d["agent"]["type"], "agent");
    assert_eq!(d["agent"]["id"], "agent_x");
    assert_eq!(d["agent"]["version"], 1);
    assert_eq!(d["vault_ids"][0], "vlt_1");
    let id = d["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("deploy_"));

    // Update name.
    let (s, up) = call(
        &app,
        "POST",
        &format!("/v1/deployments/{id}"),
        Some(json!({ "name": "hourly" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(up["name"], "hourly");

    // Pause → paused + manual reason; unpause → active + null.
    let (_, paused) = call(&app, "POST", &format!("/v1/deployments/{id}/pause"), None).await;
    assert_eq!(paused["status"], "paused");
    assert_eq!(paused["paused_reason"]["type"], "manual");
    let (_, unp) = call(&app, "POST", &format!("/v1/deployments/{id}/unpause"), None).await;
    assert_eq!(unp["status"], "active");
    assert!(unp["paused_reason"].is_null());

    // Run → a deployment_run bound to the deployment + its agent.
    let (s, run) = call(&app, "POST", &format!("/v1/deployments/{id}/run"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(run["type"], "deployment_run");
    assert_eq!(run["deployment_id"], id);
    assert_eq!(run["trigger_context"]["type"], "manual");
    assert!(run["error"].is_null());
    assert_eq!(run["session_id"], format!("sesn_for_{id}"));
    let run_id = run["id"].as_str().unwrap().to_string();
    assert!(run_id.starts_with("deprun_"));

    // Retrieve + list runs (filtered by deployment).
    let (s, got_run) = call(&app, "GET", &format!("/v1/deployment_runs/{run_id}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(got_run["id"], run_id);
    let (_, runs) = call(
        &app,
        "GET",
        &format!("/v1/deployment_runs?deployment_id={id}"),
        None,
    )
    .await;
    assert_eq!(runs["data"].as_array().unwrap().len(), 1);
    // A filter that matches nothing yields an empty page.
    let (_, none) = call(
        &app,
        "GET",
        "/v1/deployment_runs?deployment_id=deploy_other",
        None,
    )
    .await;
    assert_eq!(none["data"].as_array().unwrap().len(), 0);

    // Archive.
    let (_, arch) = call(&app, "POST", &format!("/v1/deployments/{id}/archive"), None).await;
    assert!(arch["archived_at"].is_string());

    // List deployments.
    let (_, page) = call(&app, "GET", "/v1/deployments", None).await;
    assert_eq!(page["data"].as_array().unwrap()[0]["id"], id);
}

/// The cron-schedule wire path (`cron.rs` reached through the deployments router):
/// a create carrying a well-formed 5-field cron schedule is accepted and the
/// schedule object (expression + timezone) is echoed back verbatim; a malformed
/// expression, and a schedule missing its `expression`, are each fail-closed with a
/// 400 `invalid_request_error` at write time (not silently stored). An update that
/// swaps in a bad cron is rejected the same way and leaves the stored schedule intact.
#[tokio::test]
async fn schedule_cron_is_validated_and_echoed_at_the_wire() {
    let app = app();

    // A valid weekday-9am cron is accepted and echoed.
    let (s, d) = call(
        &app,
        "POST",
        "/v1/deployments",
        Some(json!({
            "agent": "agent_x",
            "environment_id": "env_1",
            "name": "nightly",
            "schedule": { "type": "cron", "expression": "0 9 * * 1-5", "timezone": "UTC" }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(d["schedule"]["expression"], "0 9 * * 1-5");
    assert_eq!(d["schedule"]["timezone"], "UTC");
    let id = d["id"].as_str().unwrap().to_string();

    // A malformed cron is rejected at create time (fail-closed).
    let (s, body) = call(
        &app,
        "POST",
        "/v1/deployments",
        Some(json!({
            "agent": "agent_x",
            "environment_id": "env_1",
            "name": "bad",
            "schedule": { "type": "cron", "expression": "0 99 * * *", "timezone": "UTC" }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "hour out of range is rejected");
    assert_eq!(body["error"]["type"], "invalid_request_error");

    // A schedule object without an `expression` is rejected.
    let (s, _) = call(
        &app,
        "POST",
        "/v1/deployments",
        Some(json!({
            "agent": "agent_x",
            "environment_id": "env_1",
            "name": "noexpr",
            "schedule": { "type": "cron", "timezone": "UTC" }
        })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a schedule needs an expression");

    // Updating the good deployment with a malformed cron is rejected, and its stored
    // schedule is unchanged.
    let (s, _) = call(
        &app,
        "POST",
        &format!("/v1/deployments/{id}"),
        Some(json!({ "schedule": { "type": "cron", "expression": "not a cron", "timezone": "UTC" } })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (_, still) = call(&app, "GET", &format!("/v1/deployments/{id}"), None).await;
    assert_eq!(
        still["schedule"]["expression"], "0 9 * * 1-5",
        "the rejected update did not corrupt the stored schedule"
    );
}

#[tokio::test]
async fn missing_required_fields_and_unknown_ids() {
    let app = app();
    let (s, _) = call(
        &app,
        "POST",
        "/v1/deployments",
        Some(json!({ "agent": "agent_x", "name": "n" })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "environment_id required");

    let _ = make_deployment(&app).await;
    let (s, _) = call(&app, "GET", "/v1/deployments/deploy_missing", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(&app, "POST", "/v1/deployments/deploy_missing/run", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(&app, "GET", "/v1/deployment_runs/deprun_missing", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
