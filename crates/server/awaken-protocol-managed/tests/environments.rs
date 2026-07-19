//! Environments + the self-hosted work queue over HTTP: env CRUD + archive, and
//! the work lifecycle (list / poll / ack / heartbeat / stop / stats) including the
//! single-active-lease (open-tier single-worker) cap.

use std::sync::Arc;

use awaken_protocol_managed::{EnvironmentState, environments_router};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

fn app() -> Router {
    environments_router(Arc::new(EnvironmentState::new()))
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

async fn make_env(app: &Router) -> String {
    let (s, e) = call(
        app,
        "POST",
        "/v1/environments",
        Some(json!({ "name": "prod" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    e["id"].as_str().unwrap().to_string()
}

async fn call_with_worker(
    app: &Router,
    method: &str,
    uri: &str,
    worker_id: &str,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("anthropic-worker-id", worker_id)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

/// The environment projects to the official `BetaEnvironment` shape: ownership is
/// credential-implicit, so the object carries NO `scope` — and a `scope` sent in
/// the create body is ignored (non-official field), keeping the response byte-
/// compatible with the Anthropic SDK's decoder.
#[tokio::test]
async fn environment_carries_no_scope_and_ignores_a_body_scope() {
    let app = app();

    // A plain create: exactly the official field set, no `scope`.
    let (s, env) = call(
        &app,
        "POST",
        "/v1/environments",
        Some(json!({ "name": "prod" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(env.get("scope").is_none(), "no scope on the wire: {env}");
    let expected: std::collections::BTreeSet<&str> = [
        "id",
        "type",
        "archived_at",
        "created_at",
        "updated_at",
        "name",
        "description",
        "metadata",
        "config",
    ]
    .into_iter()
    .collect();
    let got: std::collections::BTreeSet<&str> = env
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(got, expected, "only the official BetaEnvironment fields");

    // A caller sending `scope` gets it ignored, not echoed.
    let (s, scoped) = call(
        &app,
        "POST",
        "/v1/environments",
        Some(json!({ "name": "prod2", "scope": "org_acme/ws_eng/proj_x" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        scoped.get("scope").is_none(),
        "a body scope is ignored, not echoed"
    );
}

#[tokio::test]
async fn official_worker_header_and_heartbeat_cas_are_wired() {
    let app = app();
    let id = make_env(&app).await;
    let (status, work) = call_with_worker(
        &app,
        "GET",
        &format!("/v1/environments/{id}/work/poll"),
        "worker-cas",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let wid = work["id"].as_str().unwrap();

    let (_, stats) = call(
        &app,
        "GET",
        &format!("/v1/environments/{id}/work/stats"),
        None,
    )
    .await;
    assert_eq!(stats["workers_polling"], 1);

    // Before any heartbeat receipt exists, the claim owner is still authority:
    // another worker cannot win the otherwise-shared NO_HEARTBEAT condition.
    let (status, _) = call_with_worker(
        &app,
        "POST",
        &format!("/v1/environments/{id}/work/{wid}/heartbeat?expected_last_heartbeat=NO_HEARTBEAT"),
        "worker-other",
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);

    let (status, first) = call_with_worker(
        &app,
        "POST",
        &format!(
            "/v1/environments/{id}/work/{wid}/heartbeat?expected_last_heartbeat=NO_HEARTBEAT&desired_ttl_seconds=7"
        ),
        "worker-cas",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["ttl_seconds"], 7);
    let first_token = first["last_heartbeat"].as_str().unwrap();

    let (status, _) = call_with_worker(
        &app,
        "POST",
        &format!("/v1/environments/{id}/work/{wid}/heartbeat?expected_last_heartbeat=NO_HEARTBEAT"),
        "worker-cas",
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);

    let (status, second) = call_with_worker(
        &app,
        "POST",
        &format!(
            "/v1/environments/{id}/work/{wid}/heartbeat?expected_last_heartbeat={first_token}"
        ),
        "worker-cas",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(second["last_heartbeat"], first["last_heartbeat"]);

    let (status, _) = call_with_worker(
        &app,
        "POST",
        &format!(
            "/v1/environments/{id}/work/{wid}/heartbeat?expected_last_heartbeat={first_token}"
        ),
        "worker-cas",
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
}

/// `deny_egress` reflects the environment's networking policy: a `limited` or `none`
/// policy denies egress (bwrap enforces on/off only — an allowlist is not honorable
/// locally, so it fails closed), while `unrestricted`, absent networking, and an
/// unknown environment share the host network.
#[tokio::test]
async fn deny_egress_reflects_the_networking_policy() {
    let state = Arc::new(EnvironmentState::new());
    let app = environments_router(state.clone());
    async fn make(app: &Router, cfg: Value) -> String {
        let (s, e) = call(
            app,
            "POST",
            "/v1/environments",
            Some(json!({ "name": "e", "config": cfg })),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        e["id"].as_str().unwrap().to_string()
    }

    let limited = make(
        &app,
        json!({ "type": "cloud", "networking": { "type": "limited" } }),
    )
    .await;
    assert!(state.deny_egress(&limited).await, "limited → egress denied");

    let unrestricted = make(
        &app,
        json!({ "type": "cloud", "networking": { "type": "unrestricted" } }),
    )
    .await;
    assert!(
        !state.deny_egress(&unrestricted).await,
        "unrestricted → host network"
    );

    let self_hosted = make(&app, json!({ "type": "self_hosted" })).await;
    assert!(
        !state.deny_egress(&self_hosted).await,
        "no networking → host network"
    );

    assert!(
        !state.deny_egress("env_nonexistent").await,
        "unknown env → host network"
    );
}

#[tokio::test]
async fn environment_crud_and_work_lifecycle() {
    let app = app();

    // Create — config defaults to self_hosted; a healthcheck work item is seeded.
    let (s, env) = call(
        &app,
        "POST",
        "/v1/environments",
        Some(json!({ "name": "prod" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(env["type"], "environment");
    assert_eq!(env["config"]["type"], "self_hosted");
    let id = env["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("env_"));

    // Work list shows the seeded queued item.
    let (_, list) = call(&app, "GET", &format!("/v1/environments/{id}/work"), None).await;
    let works = list["data"].as_array().unwrap();
    assert_eq!(works.len(), 1);
    assert_eq!(works[0]["state"], "queued");
    assert_eq!(works[0]["data"]["type"], "healthcheck");

    // Stats: one queued, depth 1.
    let (_, stats) = call(
        &app,
        "GET",
        &format!("/v1/environments/{id}/work/stats"),
        None,
    )
    .await;
    assert_eq!(stats["type"], "work_queue_stats");
    assert_eq!(stats["depth"], 1);
    // Nothing claimed yet: the seeded healthcheck is queued, so `pending` (claimed &
    // processing) is 0, not the queue depth.
    assert_eq!(stats["pending"], 0);

    // Poll leases the item -> active.
    let (s, leased) = call(
        &app,
        "GET",
        &format!("/v1/environments/{id}/work/poll"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(leased["state"], "active");
    assert!(leased["started_at"].is_string());
    let wid = leased["id"].as_str().unwrap().to_string();

    // A second poll returns null (single active lease == the single-worker cap).
    let (_, again) = call(
        &app,
        "GET",
        &format!("/v1/environments/{id}/work/poll"),
        None,
    )
    .await;
    assert!(again.is_null(), "only one active lease at a time");

    // Ack + heartbeat + stop.
    let (_, acked) = call(
        &app,
        "POST",
        &format!("/v1/environments/{id}/work/{wid}/ack"),
        None,
    )
    .await;
    assert!(acked["acknowledged_at"].is_string());
    let (_, hb) = call(
        &app,
        "POST",
        &format!("/v1/environments/{id}/work/{wid}/heartbeat"),
        None,
    )
    .await;
    assert_eq!(hb["type"], "work_heartbeat");
    assert_eq!(hb["lease_extended"], true);
    assert_eq!(hb["ttl_seconds"], 60);
    let (_, stopped) = call(
        &app,
        "POST",
        &format!("/v1/environments/{id}/work/{wid}/stop"),
        None,
    )
    .await;
    assert_eq!(stopped["state"], "stopped");
    assert!(stopped["stop_requested_at"].is_string());

    // Retrieve + update work (metadata).
    let (_, upd) = call(
        &app,
        "POST",
        &format!("/v1/environments/{id}/work/{wid}"),
        Some(json!({ "metadata": { "run": "1" } })),
    )
    .await;
    assert_eq!(upd["metadata"]["run"], "1");

    // Env retrieve / update / list / archive.
    let (_, upenv) = call(
        &app,
        "POST",
        &format!("/v1/environments/{id}"),
        Some(json!({ "description": "the prod env" })),
    )
    .await;
    assert_eq!(upenv["description"], "the prod env");
    let (_, page) = call(&app, "GET", "/v1/environments", None).await;
    assert_eq!(page["data"].as_array().unwrap()[0]["id"], id);
    let (_, arch) = call(
        &app,
        "POST",
        &format!("/v1/environments/{id}/archive"),
        None,
    )
    .await;
    assert!(arch["archived_at"].is_string());
    let (s, del) = call(&app, "DELETE", &format!("/v1/environments/{id}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(del["type"], "environment_deleted");
}

#[tokio::test]
async fn work_and_env_not_found_paths() {
    let app = app();
    let id = make_env(&app).await;
    // Work under the wrong env 404s.
    let (s, _) = call(&app, "GET", "/v1/environments/env_missing/work/poll", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(
        &app,
        "GET",
        &format!("/v1/environments/{id}/work/work_missing"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = call(&app, "GET", "/v1/environments/env_missing", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
