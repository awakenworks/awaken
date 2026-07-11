//! Session ↔ environment association (Managed Agents contract): the environment is
//! bound at session creation (defaulting to `env_local` when omitted), echoed on the
//! Session, and immutable for the session's lifetime — `POST /v1/sessions/{id}`
//! updates only `title`/`metadata`, so an `environment_id` sent to update is ignored
//! and the session keeps its create-time environment.
//!
//! Scope note: this locks the WIRE/record association + immutability. It does not
//! assert sandbox realization — `environment_id` does not yet parameterize the local
//! `SandboxSpec` (networking/packages), so two sessions in different environments get
//! byte-identical local sandboxes today.

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_protocol_managed::{
    Decision, ManagedState, OutcomeReport, RunError, SessionInit, SessionRuntime, TurnOutcome,
    router,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

struct AcceptingFake;

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
    ) -> Result<TurnOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn resume(&self, _t: &str, _tid: &str, _d: Decision) -> Result<TurnOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn resume_custom(
        &self,
        _t: &str,
        _tid: &str,
        _c: &str,
        _e: bool,
    ) -> Result<TurnOutcome, RunError> {
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

fn app() -> Router {
    router(std::sync::Arc::new(ManagedState::new(AcceptingFake)))
}

#[tokio::test]
async fn environment_is_pinned_at_creation() {
    let app = app();

    // Explicit environment is echoed on the session.
    let (s, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "a", "environment_id": "env_custom" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(session["environment_id"], "env_custom");

    // Omitting it defaults to the local environment.
    let (s, defaulted) = call(&app, "POST", "/v1/sessions", Some(json!({ "agent": "a" }))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(defaulted["environment_id"], "env_local");
}

#[tokio::test]
async fn environment_is_immutable_across_update() {
    let app = app();

    let (_, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "a", "environment_id": "env_a" })),
    )
    .await;
    let id = session["id"].as_str().unwrap().to_string();

    // Update sends a new environment_id alongside a title change. The contract
    // pins the environment at creation, so the update changes the title but the
    // environment stays put.
    let (s, updated) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({ "title": "renamed", "environment_id": "env_b" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(updated["title"], "renamed", "title is updatable");
    assert_eq!(
        updated["environment_id"], "env_a",
        "environment is immutable — the update's env is ignored"
    );

    // And a fresh GET still reports the create-time environment.
    let (s, got) = call(&app, "GET", &format!("/v1/sessions/{id}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(got["environment_id"], "env_a");
}
