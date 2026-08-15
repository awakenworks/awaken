//! Session ↔ environment association (Managed Agents contract): the environment is
//! bound at session creation (defaulting to `env_local` when omitted), echoed on the
//! Session, and immutable for the session's lifetime — `POST /v1/sessions/{id}`
//! updates only `title`/`metadata`, so an `environment_id` sent to update rejects
//! the whole mutation and the Session keeps its create-time Environment snapshot.
//!
//! Scope note: this locks the WIRE/record association + immutability. It does not
//! assert Sandbox realization, which is covered by Runtime Host provisioning tests.

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_protocol_managed::{ManagedState, router};
use awaken_session_contract::{
    OutcomeDrive, RunError, SessionInit, SessionRuntime, StepOutcome, ToolPermissionDecision,
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
        _c: Vec<ContentBlock>,
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
    ) -> Result<OutcomeDrive, RunError> {
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

async fn app() -> (Router, String) {
    let (environment_authoring, environment_execution) =
        awaken_protocol_managed::test_support::environment_components();
    let environment = environment_authoring
        .application()
        .create(awaken_environment_contract::CreateEnvironmentCommand {
            command_id: "session-environment-test".into(),
            name: "Custom".into(),
            description: String::new(),
            metadata: Default::default(),
            scope: None,
            config: awaken_environment_contract::EnvironmentConfig::SelfHosted,
        })
        .await
        .expect("publish Environment through the Control application");
    (
        router(std::sync::Arc::new(
            ManagedState::new(AcceptingFake).with_environments(environment_execution),
        )),
        environment.id,
    )
}

#[tokio::test]
async fn environment_is_pinned_at_creation() {
    // Cause/effect rules: R1 a Control-published current Environment is admitted
    // and frozen by id; R2 omission is rejected at the SDK boundary. Managed
    // never turns a missing Environment into ambient local execution.
    let (app, environment_id) = app().await;

    // Explicit environment is echoed on the session.
    let (s, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "a", "environment_id": environment_id })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(session["environment_id"], environment_id);

    // Omitting the SDK-required field fails before Session creation.
    let (s, defaulted) = call(&app, "POST", "/v1/sessions", Some(json!({ "agent": "a" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(defaulted["error"]["type"], "invalid_request_error");
}

#[tokio::test]
async fn environment_is_immutable_across_update() {
    let (app, environment_id) = app().await;

    let (_, session) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "a", "environment_id": environment_id })),
    )
    .await;
    let id = session["id"].as_str().unwrap().to_string();

    // Cause graph: immutable field present -> admission rejects the whole update
    // -> neither environment nor an otherwise-valid title is changed.
    //
    // | environment_id | title | status | environment | title |
    // |----------------|-------|--------|-------------|-------|
    // | absent         | set   | 200    | env_a       | set   |
    // | env_b          | set   | 400    | env_a       | old   |
    let (s, updated) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}"),
        Some(json!({ "title": "renamed", "environment_id": "env_b" })),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(updated["error"]["type"], "invalid_request_error");

    // And a fresh GET still reports the create-time environment.
    let (s, got) = call(&app, "GET", &format!("/v1/sessions/{id}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(got["environment_id"], environment_id);
    assert_eq!(got["title"], serde_json::Value::Null);
}
