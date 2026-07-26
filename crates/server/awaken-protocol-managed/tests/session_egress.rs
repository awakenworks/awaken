//! `POST /v1/sessions` resolves the session's `environment_id` to its networking
//! policy and stages the exact `EnvironmentSnapshot`: a session on a `limited`
//! environment denies egress, one on `unrestricted` (or an unknown env) keeps the
//! host network. No coarse boolean becomes a second policy authority.

use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_protocol_managed::{
    EnvironmentState, ManagedState, OutcomeReport, RunError, SessionInit, SessionRuntime,
    StepOutcome, ToolPermissionDecision, environments_router, router,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

/// Records each `prepare_session`'s exact frozen network policy.
struct CapturingFake {
    egress: Arc<Mutex<Vec<awaken_protocol_managed::SessionNetworkPolicy>>>,
}

#[async_trait::async_trait]
impl SessionRuntime for CapturingFake {
    async fn prepare_session(&self, _thread: &str, init: SessionInit) -> Result<(), RunError> {
        self.egress.lock().unwrap().push(init.environment.network);
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
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test]
async fn session_carries_the_exact_frozen_environment_network() {
    let env_state = Arc::new(EnvironmentState::new());
    let egress = Arc::new(Mutex::new(Vec::new()));
    let managed = Arc::new(
        ManagedState::new(CapturingFake {
            egress: egress.clone(),
        })
        .with_environments(env_state.clone()),
    );
    let app = router(managed).merge(environments_router(env_state));

    // A limited-networking environment.
    let (_, limited) = call(
        &app,
        "POST",
        "/v1/environments",
        Some(json!({ "name": "l", "config": { "type": "cloud", "networking": { "type": "limited" } } })),
    )
    .await;
    let limited_id = limited["id"].as_str().unwrap().to_string();

    // A session on it stages the exact closed policy.
    let (s, _) = call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "a", "environment_id": limited_id })),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        egress.lock().unwrap().last(),
        Some(&awaken_protocol_managed::SessionNetworkPolicy::None)
    );

    // An unrestricted environment keeps the exact open policy.
    let (_, open) = call(
        &app,
        "POST",
        "/v1/environments",
        Some(json!({ "name": "o", "config": { "type": "cloud", "networking": { "type": "unrestricted" } } })),
    )
    .await;
    let open_id = open["id"].as_str().unwrap().to_string();
    call(
        &app,
        "POST",
        "/v1/sessions",
        Some(json!({ "agent": "a", "environment_id": open_id })),
    )
    .await;
    assert_eq!(
        egress.lock().unwrap().last(),
        Some(&awaken_protocol_managed::SessionNetworkPolicy::Unrestricted)
    );

    // An omitted/unknown environment defaults to host network.
    call(&app, "POST", "/v1/sessions", Some(json!({ "agent": "a" }))).await;
    assert_eq!(
        egress.lock().unwrap().last(),
        Some(&awaken_protocol_managed::SessionNetworkPolicy::Unrestricted)
    );
}
