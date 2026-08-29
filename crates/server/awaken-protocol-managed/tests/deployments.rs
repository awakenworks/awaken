//! Deployments + deployment runs over HTTP: CRUD, the pause/unpause/run/archive
//! lifecycle actions, agent-reference normalization, and run listing.

use std::sync::Arc;

use awaken_deployment_application::{
    DeploymentApplication, DeploymentLaunch, DeploymentLaunchOutcome, DeploymentSessionLauncher,
};
use awaken_protocol_managed::deployments_router;
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
            DeploymentLaunchOutcome::Created {
                session_id: format!("sesn_for_{}", request.deployment_id),
            }
        }
    }
    let state = Arc::new(DeploymentApplication::new());
    state.bind_launcher(Arc::new(Launcher));
    deployments_router(state).layer(axum::Extension(awaken_tenancy::WorkspaceScope(
        "default".into(),
    )))
}

#[tokio::test]
async fn deployment_routes_require_the_edge_selected_workspace() {
    // Cause/effect graph: C1=edge workspace stamp is absent; C2=stamp is
    // present but empty; C3=stamp is non-empty. Effects: E1=fail closed with
    // indistinguishable 404; E2=enter the Deployment application. Decision
    // rules: S1 C1=>E1, S2 C2=>E1, S3 C3=>E2 (the lifecycle tests below own S3).
    let state = Arc::new(DeploymentApplication::new());
    let missing = deployments_router(state.clone());
    let empty = deployments_router(state).layer(axum::Extension(awaken_tenancy::WorkspaceScope(
        String::new(),
    )));
    let body = json!({
        "agent": "agent_x",
        "environment_id": "env_1",
        "name": "nightly",
        "initial_events": [{ "type": "user.message", "content": [{ "type": "text", "text": "go" }] }]
    });

    assert_eq!(
        call(&missing, "POST", "/v1/deployments", Some(body.clone()))
            .await
            .0,
        StatusCode::NOT_FOUND,
        "S1"
    );
    assert_eq!(
        call(&empty, "POST", "/v1/deployments", Some(body)).await.0,
        StatusCode::NOT_FOUND,
        "S2"
    );
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
async fn deployment_initial_events_match_the_managed_agents_wire_contract() {
    // API compatibility cause/effect graph:
    // C1 official user.message; C2 official user.define_outcome; C3 paired
    // user.message + system.message; C4 internal snake_case spelling leaks into
    // a request. Effects E1 accept and echo the exact official discriminant;
    // E2 reject the internal spelling. These rules keep protocol projection
    // independent from the Deployment storage codec.
    //
    // | Rule | Input event(s)                           | HTTP | Response type(s) |
    // | A1   | user.message                             | 200  | user.message     |
    // | A2   | user.define_outcome                      | 200  | user.define_outcome |
    // | A3   | user.message, system.message             | 200  | exact ordered pair |
    // | A4   | user_message (internal storage spelling) | 400  | none             |
    let app = app();
    let cases = [
        (
            "A1",
            json!([{"type":"user.message","content":[{"type":"text","text":"go"}]}]),
            vec!["user.message"],
        ),
        (
            "A2",
            json!([{
                "type":"user.define_outcome",
                "description":"Produce the verified release brief",
                "rubric":{"type":"text","content":"All claims cite supplied evidence"},
                "max_iterations":3
            }]),
            vec!["user.define_outcome"],
        ),
        (
            "A3",
            json!([
                {"type":"user.message","content":[{"type":"text","text":"go"}]},
                {"type":"system.message","content":[{"type":"text","text":"Use only supplied facts"}]}
            ]),
            vec!["user.message", "system.message"],
        ),
    ];

    for (rule, initial_events, expected_types) in cases {
        let (status, deployment) = call(
            &app,
            "POST",
            "/v1/deployments",
            Some(json!({
                "agent":"agent_x",
                "environment_id":"env_1",
                "name":format!("wire-{rule}"),
                "initial_events":initial_events
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{rule}");
        assert_eq!(
            deployment["initial_events"]
                .as_array()
                .expect("initial_events response")
                .iter()
                .map(|event| event["type"].as_str().expect("event type"))
                .collect::<Vec<_>>(),
            expected_types,
            "{rule}"
        );
    }

    let (status, _) = call(
        &app,
        "POST",
        "/v1/deployments",
        Some(json!({
            "agent":"agent_x",
            "environment_id":"env_1",
            "name":"wire-A4",
            "initial_events":[{"type":"user_message","content":[{"type":"text","text":"go"}]}]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "A4");
}

// Test design: deployment_lifecycle_and_runs
// Cause/effect graph: C1 active Deployment; C2 manually paused;
// C3 archived. Effects: E1 create/update commit; E2 C2 suppresses only
// scheduled triggers while manual run still creates a Session and preserves
// the manual pause reason; E3 unpause restores Active; E4 C3 is terminal.
// Constraint K1 manual and schedule triggers share the same launcher but
// only the scheduler consults Active. Rules L1=C1=>E1; L2=C2+manual=>E2;
// Decision table: L1=C1=>E1; L2=C2+manual=>E2;
// L3=C2+unpause=>E3; L4=C3=>E4.
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
    assert!(id.starts_with("depl_"));

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

    // Pause suppresses future schedule occurrences, but not an explicit manual
    // run. The Deployment remains paused after the manual run.
    let (_, paused) = call(&app, "POST", &format!("/v1/deployments/{id}/pause"), None).await;
    assert_eq!(paused["status"], "paused");
    assert_eq!(paused["paused_reason"]["type"], "manual");
    let (s, paused_run) = call(&app, "POST", &format!("/v1/deployments/{id}/run"), None).await;
    assert_eq!(s, StatusCode::OK, "L2/E2");
    assert_eq!(paused_run["trigger_context"]["type"], "manual", "L2/E2");
    assert!(paused_run["session_id"].is_string(), "L2/E2");
    let (_, still_paused) = call(&app, "GET", &format!("/v1/deployments/{id}"), None).await;
    assert_eq!(still_paused["status"], "paused", "L2/E2");
    assert_eq!(still_paused["paused_reason"]["type"], "manual", "L2/E2");

    // Unpause → active + null.
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
    assert!(run_id.starts_with("drun_"));

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
    assert_eq!(runs["data"].as_array().unwrap().len(), 2);
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

    // Archived rows are hidden by default and opt-in through the SDK filter.
    let (_, page) = call(&app, "GET", "/v1/deployments", None).await;
    assert!(page["data"].as_array().unwrap().is_empty());
    let (_, page) = call(&app, "GET", "/v1/deployments?include_archived=true", None).await;
    assert_eq!(page["data"].as_array().unwrap()[0]["id"], id);
}

#[tokio::test]
async fn sdk_page_limit_deserializes_in_filtered_deployment_queries() {
    // Cause/effect graph: C1 the official SDK serializes `limit` as a query
    // string; C2 Deployment and DeploymentRun filters flatten the shared page
    // query into a larger DTO. Effects: E1 both filtered endpoints admit the
    // request, E2 the shared paginator applies the numeric limit, E3 malformed
    // values fail at the HTTP boundary.
    //
    // | Rule | SDK string | Flattened query | Value | Outcome |
    // |---|---|---|---|---|
    // | P1 | yes | Deployment | 100 | 200 page |
    // | P2 | yes | DeploymentRun | 100 | 200 page |
    // | P3 | yes | Deployment | invalid | 400 |
    let app = app();
    let id = make_deployment(&app).await;
    let _ = call(&app, "POST", &format!("/v1/deployments/{id}/run"), None).await;

    for (rule, uri) in [
        ("P1", "/v1/deployments?limit=100"),
        (
            "P2",
            "/v1/deployment_runs?limit=100&deployment_id=does-not-match",
        ),
    ] {
        let (status, page) = call(&app, "GET", uri, None).await;
        assert_eq!(status, StatusCode::OK, "{rule}");
        assert!(page["data"].is_array(), "{rule}");
    }

    let (status, _) = call(&app, "GET", "/v1/deployments?limit=invalid", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "P3");
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
            "initial_events": [{ "type":"user.message", "content":[{"type":"text", "text":"go"}] }],
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
            "initial_events": [{ "type":"user.message", "content":[{"type":"text", "text":"go"}] }],
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
            "initial_events": [{ "type":"user.message", "content":[{"type":"text", "text":"go"}] }],
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

#[tokio::test]
async fn deployment_repository_credentials_fail_closed_before_persistence() {
    // Durable Resource cause/effect graph: C1=secret-free File/Memory/Repository
    // input, C2=Repository input carries write-only authorization_token.
    // Effects: E1=application command may persist and later replay exactly;
    // E2=request is rejected before identity/state because serde intentionally
    // redacts the token and accepting it would silently change behavior after a
    // restart. Decision rules: R1 C1->E1 (covered by lifecycle tests),
    // R2 C2 on create/update->E2 until a durable credential-reference provider
    // exists; vault_ids remains the supported durable credential path.
    let app = app();
    let (status, body) = call(
        &app,
        "POST",
        "/v1/deployments",
        Some(json!({
            "agent":"agent_x",
            "environment_id":"env_1",
            "name":"secret-bearing",
            "initial_events":[{"type":"user.message","content":[{"type":"text","text":"go"}]}],
            "resources":[{
                "type":"github_repository",
                "url":"https://github.com/acme/repo.git",
                "authorization_token":"must-not-disappear" // awaken-allow: secret
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "R2");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("cannot be stored durably")),
        "R2 explicit remediation"
    );
    let (_, page) = call(&app, "GET", "/v1/deployments", None).await;
    assert!(page["data"].as_array().unwrap().is_empty(), "R2 no state");
}
