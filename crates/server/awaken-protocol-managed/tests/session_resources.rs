//! Post-creation session-resource CRUD (`/v1/sessions/{id}/resources`) mirrors the
//! Managed Agents contract: `file` and `github_repository` attach to a live
//! session, but a `memory_store` binds at session-create time only — attaching one
//! to a running session fails closed with a 400 (`invalid_request_error`).

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_protocol_managed::{
    AgentConfigSource, AgentConfigView, ManagedState, OutcomeReport, RunError, SessionInit,
    SessionResource, SessionRuntime, StepOutcome, ToolPermissionDecision, router,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

/// A runtime that accepts every `prepare_session` — the session record exists, so
/// the resource routes can be exercised. Turn methods are unused here.
struct AcceptingFake;

struct AgentWithResources;

impl AgentConfigSource for AgentWithResources {
    fn agent_view(&self, agent_id: &str) -> Option<AgentConfigView> {
        (agent_id == "a").then(|| AgentConfigView {
            model: None,
            system: None,
            tool_ids: Vec::new(),
            resources: vec![SessionResource {
                kind: "skill".into(),
                id: "skill_release".into(),
                mount_path: "/mnt/skills/release".into(),
                instructions: None,
                auth_token: None,
                git_ref: None,
            }],
        })
    }
}

#[async_trait::async_trait]
impl SessionRuntime for AcceptingFake {
    async fn prepare_session(&self, _thread: &str, _init: SessionInit) -> Result<(), RunError> {
        Ok(())
    }
    async fn run(
        &self,
        _a: &str,
        _t: &str,
        _c: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn resume(
        &self,
        _t: &str,
        _tid: &str,
        _d: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn resume_custom(
        &self,
        _t: &str,
        _tid: &str,
        _c: &str,
        _e: bool,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn add_system(&self, _t: &str, _x: &str) -> Result<(), RunError> {
        Ok(())
    }
    async fn define_outcome(
        &self,
        _t: &str,
        _d: &str,
        _r: &str,
        _m: u32,
    ) -> Result<OutcomeReport, RunError> {
        Err(RunError::internal("unused"))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
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

async fn app_with_session() -> (Router, String) {
    let app = router(std::sync::Arc::new(ManagedState::new(AcceptingFake)));
    let (s, session) = call(&app, "POST", "/v1/sessions", Some(json!({ "agent": "a" }))).await;
    assert_eq!(s, StatusCode::OK);
    let id = session["id"].as_str().unwrap().to_string();
    (app, id)
}

#[tokio::test]
async fn create_time_resources_are_backfilled_and_addressable() {
    let app = router(std::sync::Arc::new(ManagedState::new(AcceptingFake)));

    let (s, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({
            "agent": "a",
            "resources": [
                { "type": "file", "file_id": "file_1", "mount_path": "/w/data.csv" },
                { "type": "memory_store", "memory_store_id": "mem_1", "instructions": "notes" }
            ]
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let id = session["id"].as_str().unwrap().to_string();

    // The created session echoes its create-time mounts in SDK-decodable shape.
    let res = session["resources"].as_array().unwrap();
    assert_eq!(res.len(), 2, "both create-time resources are backfilled");
    assert_eq!(res[0]["type"], "file");
    assert_eq!(res[0]["file_id"], "file_1");
    assert_eq!(res[0]["mount_path"], "/w/data.csv");
    assert!(res[0]["id"].is_string() && res[0]["created_at"].is_string());
    assert_eq!(res[1]["type"], "memory_store");
    assert_eq!(res[1]["memory_store_id"], "mem_1");
    assert_eq!(res[1]["instructions"], "notes");

    // They are addressable via list/get, uniformly with any later-attached ones.
    let (s, listed) = call(&app, "GET", &format!("/v1/sessions/{id}/resources"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(listed["data"].as_array().unwrap().len(), 2);
    let rid = res[0]["id"].as_str().unwrap();
    let (s, got) = call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/resources/{rid}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(got["type"], "file");
}

#[tokio::test]
async fn published_agent_resources_are_visible_as_effective_session_inputs() {
    let state = ManagedState::new(AcceptingFake)
        .with_config_source(std::sync::Arc::new(AgentWithResources));
    let app = router(std::sync::Arc::new(state));

    let (status, session) = call(&app, "POST", "/v1/sessions", Some(json!({ "agent": "a" }))).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(session["resources"][0]["type"], "skill");
    assert_eq!(session["resources"][0]["resource_id"], "skill_release");
    assert_eq!(session["resources"][0]["mount_path"], "/mnt/skills/release");
}

#[tokio::test]
async fn file_resource_attaches_to_a_live_session() {
    let (app, id) = app_with_session().await;

    let (s, resource) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/resources"),
        Some(json!({ "type": "file", "file_id": "file_1", "mount_path": "/workspace/data.csv" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(resource["type"], "file");
    assert!(resource["id"].is_string(), "the mount is assigned an id");

    // It now shows up in the session's resource listing.
    let (s, listed) = call(&app, "GET", &format!("/v1/sessions/{id}/resources"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn github_repository_attaches_to_a_live_session() {
    let (app, id) = app_with_session().await;

    let (s, resource) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/resources"),
        Some(json!({
            "type": "github_repository",
            "url": "https://github.com/owner/repo",
            "authorization_token": "ghp_x", // awaken-allow: secret
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(resource["type"], "github_repository");
    assert!(resource["id"].is_string());

    let (s, listed) = call(&app, "GET", &format!("/v1/sessions/{id}/resources"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn a_file_resource_can_be_detached_from_a_live_session() {
    let (app, id) = app_with_session().await;

    let (s, resource) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/resources"),
        Some(json!({ "type": "file", "file_id": "file_1" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let rid = resource["id"].as_str().unwrap().to_string();

    let (s, _) = call(
        &app,
        "DELETE",
        &format!("/v1/sessions/{id}/resources/{rid}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    let (s, listed) = call(&app, "GET", &format!("/v1/sessions/{id}/resources"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(listed["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn memory_store_cannot_attach_to_a_running_session() {
    let (app, id) = app_with_session().await;

    let (s, err) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/resources"),
        Some(json!({ "type": "memory_store", "memory_store_id": "memstore_1" })),
    )
    .await;
    // Managed Agents contract: memory stores are create-time only.
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(err["error"]["type"], "invalid_request_error");

    // Nothing was mounted — the listing stays empty.
    let (s, listed) = call(&app, "GET", &format!("/v1/sessions/{id}/resources"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(listed["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn getting_or_deleting_an_unknown_session_resource_is_404() {
    let (app, id) = app_with_session().await;
    let uri = format!("/v1/sessions/{id}/resources/res_does_not_exist");
    let (get_status, _) = call(&app, "GET", &uri, None).await;
    assert_eq!(get_status, StatusCode::NOT_FOUND);
    let (del_status, _) = call(&app, "DELETE", &uri, None).await;
    assert_eq!(del_status, StatusCode::NOT_FOUND);
}
