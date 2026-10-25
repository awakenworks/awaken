// @formal surface exclude=test_only
// @formal owner crates/devtools/awaken-scenario-host/src/scenario_platform.rs
// @formal reason: Exercises the actual Scenario composition and HTTP authoring/Worker read handlers.
//! The real Scenario composition must publish the same live Environment catalog
//! to authoring HTTP and the Coordinator's private Worker reads.
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt as _;

async fn request(app: &Router, method: &str, path: &str, body: Value) -> Value {
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        app.clone().oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .header("x-awaken-worker-id", "catalog-worker")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        ),
    )
    .await
    .expect("Scenario HTTP response deadline")
    .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "{method} {path}: {}",
        String::from_utf8_lossy(&body)
    );
    serde_json::from_slice(&body).unwrap()
}

async fn warmups(
    app: &Router,
    identity: &Value,
) -> Vec<awaken_session_configuration_contract::EnvironmentSnapshot> {
    let response = request(
        app,
        "POST",
        "/v1/worker/environment/warmups",
        json!({"identity":identity}),
    )
    .await;
    serde_json::from_value(response["warmups"].clone()).unwrap()
}

#[tokio::test]
async fn authoring_and_worker_warmup_http_share_live_environment_revisions() {
    // W1 before publication -> no warm demand. W2 cloud creation -> exact id.
    // W3 updated definition -> same id and newer revision. W4 archive -> gone.
    // Use the real mount and handlers, not a separately assembled read adapter.
    // No physical Worker or cloud provisioning is claimed by this HTTP test.
    let mut deployment = awaken_deployment_config::DeploymentConfig::ephemeral();
    deployment.disable_local_pool = true;
    let platform = crate::resource_host_with_deployment(
        Arc::new(crate::EchoModel),
        "warmup-catalog",
        deployment,
    )
    .await;
    let app = super::mount_with_environments(platform).await;
    let version = awaken_dispatch::CURRENT_RUNTIME_PROTOCOL_VERSION;
    let mut manifest = awaken_dispatch::WorkerManifest::default();
    manifest.runtime_protocol.min = version;
    manifest.runtime_protocol.max = version;
    let registered = request(
        &app,
        "POST",
        "/v1/worker/register",
        json!({"registration": {
            "worker_id":"catalog-worker", "incarnation_id":"catalog-boot",
            "manifest":manifest, "create_ledger":null
        }}),
    )
    .await;
    let identity = &registered["worker"]["snapshot"]["identity"];
    assert!(
        identity.is_object(),
        "registration must contain the actual identity: {registered}"
    );
    assert!(warmups(&app, identity).await.is_empty(), "W1");
    let created = request(
        &app,
        "POST",
        "/v1/environments",
        json!({"name":"catalog-cloud","config":{"type":"cloud"}}),
    )
    .await;
    let id = created["id"].as_str().unwrap();
    let first = warmups(&app, identity).await;
    assert_eq!(first.len(), 1, "W2");
    assert_eq!(first[0].environment_id, id, "W2");
    request(
        &app,
        "POST",
        &format!("/v1/environments/{id}"),
        json!({"metadata":{"label":"new-revision"}}),
    )
    .await;
    let updated = warmups(&app, identity).await;
    assert_eq!(updated.len(), 1, "W3");
    assert_eq!(updated[0].environment_id, id, "W3");
    assert!(updated[0].revision > first[0].revision, "W3");
    request(
        &app,
        "POST",
        &format!("/v1/environments/{id}/archive"),
        json!({}),
    )
    .await;
    assert!(warmups(&app, identity).await.is_empty(), "W4");
}
