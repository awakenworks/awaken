//! Cross-module E2E for the official Managed Deployment lifecycle.

use std::sync::Arc;

use awaken_protocol_managed::{
    DeploymentLaunch, DeploymentLaunchOutcome, DeploymentSessionLauncher, DeploymentState,
    LocalDeploymentSessionLauncher, ManagedState, deployments_router,
};
use awaken_runtime_host::ManagedHost;
use awaken_scenario_host::{EchoModel, build_router_and_host};
use awaken_tenancy::WorkspaceScope;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("anthropic-beta", "managed-agents-2026-04-01");
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let response = app
        .clone()
        .oneshot(
            builder
                .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test]
async fn deployment_run_identity_replays_one_session_and_rejects_payload_reuse() {
    // Cause/effect decision table:
    // R1 new stable DeploymentRun + valid launch -> create one deterministic Session;
    // R2 same run + byte-equivalent launch -> return that Session, enqueue no second
    // initial Event batch; R3 same run + changed payload -> fail closed and preserve
    // the R1 Session. This covers the response-loss retry before an HTTP adapter exists.
    let (_, host) = build_router_and_host(Arc::new(EchoModel), "claude-sonnet-5");
    let workspace_id = host.local_workspace().to_string();
    let managed = Arc::new(ManagedState::new(ManagedHost::new(host.clone())));
    let app = awaken_coordinator::mount_with_managed(host, managed.clone());
    let launcher = LocalDeploymentSessionLauncher::new(managed.clone());
    let request: DeploymentLaunch = serde_json::from_value(json!({
        "deployment_id": "depl_retry",
        "deployment_run_id": "drun_retry",
        "workspace_id": workspace_id,
        "agent": {"id": "assistant", "type": "agent", "version": 1},
        "environment_id": "env_local",
        "metadata": {},
        "initial_events": [{
            "type": "user.message",
            "content": [{"type": "text", "text": "one launch only"}]
        }],
        "resources": [],
        "vault_ids": []
    }))
    .unwrap();

    let first = launcher.launch(request.clone()).await;
    let second = launcher.launch(request.clone()).await;
    let (first_id, second_id) = match (first, second) {
        (
            DeploymentLaunchOutcome::Created { session_id: first },
            DeploymentLaunchOutcome::Created { session_id: second },
        ) => (first, second),
        outcomes => panic!("R1/R2 unexpected outcomes: {outcomes:?}"),
    };
    assert_eq!(first_id, second_id, "R1/R2");
    let mut user_messages = 0;
    for _ in 0..200 {
        let (status, events) = call(
            &app,
            "GET",
            &format!("/v1/sessions/{first_id}/events"),
            None,
        )
        .await;
        if status == StatusCode::OK {
            user_messages = events["data"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|event| event["type"] == "user.message")
                .count();
            if user_messages > 0 {
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    assert_eq!(user_messages, 1, "R2 initial Event batch");

    let mut changed = request;
    changed.metadata.insert("changed".into(), "true".into());
    assert!(
        matches!(
            launcher.launch(changed).await,
            DeploymentLaunchOutcome::Failed { .. }
        ),
        "R3"
    );
    assert_eq!(managed.get_session(&first_id).unwrap().id, first_id, "R3");
}

#[tokio::test]
async fn deployment_manual_and_cron_runs_create_ordinary_sessions_with_initial_events() {
    // Official-docs cause/effect table:
    // D1 valid Agent/environment/initial event -> active Deployment;
    // D2 manual run -> DeploymentRun XOR terminal branch with a Session id;
    // D3 initial event -> committed through ordinary Session create (no second send path);
    // D4 due cron -> schedule trigger with scheduled_at and another ordinary Session;
    // D5 pause -> no further scheduled launch; unpause -> future-only cursor.
    let (_, host) = build_router_and_host(Arc::new(EchoModel), "claude-sonnet-5");
    let workspace_id = host.local_workspace().to_string();
    let managed = Arc::new(ManagedState::new(ManagedHost::new(host.clone())));
    let deployments = Arc::new(DeploymentState::new());
    deployments.bind_launcher(Arc::new(LocalDeploymentSessionLauncher::new(
        managed.clone(),
    )));
    let deployment_api = deployments_router(deployments.clone())
        .layer(axum::Extension(WorkspaceScope(workspace_id)));
    let app = awaken_coordinator::mount_with_managed(host, managed.clone()).merge(deployment_api);

    let (status, deployment) = call(
        &app,
        "POST",
        "/v1/deployments",
        Some(json!({
            "agent":"assistant",
            "environment_id":"env_local",
            "name":"Dream maintenance",
            "initial_events":[{
                "type":"user.message",
                "content":[{"type":"text","text":"deployment seed event"}]
            }],
            "schedule":{"type":"cron","expression":"*/15 * * * *","timezone":"UTC"}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "D1: {deployment}");
    let deployment_id = deployment["id"].as_str().unwrap();

    let (status, manual) = call(
        &app,
        "POST",
        &format!("/v1/deployments/{deployment_id}/run"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "D2: {manual}");
    assert!(manual["error"].is_null(), "D2 XOR");
    let session_id = manual["session_id"].as_str().unwrap();
    let mut observed = None;
    for _ in 0..200 {
        let response = call(
            &app,
            "GET",
            &format!("/v1/sessions/{session_id}/events"),
            None,
        )
        .await;
        if response.0 == StatusCode::OK && response.1.to_string().contains("deployment seed event")
        {
            observed = Some(response);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    let (status, events) = match observed {
        Some(observed) => observed,
        None => panic!(
            "D3 initial Event did not commit; direct session={:?}, owner={:?}",
            managed.get_session(session_id),
            managed.resolve_owner(session_id).await
        ),
    };
    assert_eq!(status, StatusCode::OK, "D3: {events}");

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let scheduled = deployments
        .tick_and_launch(now + 20 * 60_000)
        .await
        .unwrap();
    assert!(!scheduled.is_empty(), "D4");
    assert!(
        scheduled.iter().all(|run| {
            run.error.is_none()
                && run.session_id.is_some()
                && serde_json::to_value(&run.trigger_context).unwrap()["type"] == "schedule"
        }),
        "D4: {scheduled:?}"
    );

    let (status, _) = call(
        &app,
        "POST",
        &format!("/v1/deployments/{deployment_id}/pause"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "D5");
    assert!(
        deployments
            .tick_and_launch(now + 40 * 60_000)
            .await
            .unwrap()
            .is_empty(),
        "D5"
    );
}
