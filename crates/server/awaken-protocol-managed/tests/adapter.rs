//! Adapter integration tests with fake runtimes: the happy path, and the HITL
//! await -> `requires_action` -> `user.tool_confirmation` -> resume round-trip.

mod support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::content::{ContentBlock, extract_text};
use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_agent_contract::agent::run::EndCause;
use awaken_protocol_managed::{ManagedState, router};
use awaken_session_contract::{
    AgentCapabilities, BuiltinTool, CustomTool, OutcomeDrive, OutcomeIteration, OutcomeReport,
    Pending, RunError, RunErrorKind, SessionRuntime, StepOutcome, ToolPermissionDecision,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

async fn json_call(
    app: &Router,
    method: &str,
    uri: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let (status, json) = json_response(app, method, uri, body).await;
    assert_eq!(status, StatusCode::OK, "{method} {uri}");
    json
}

async fn json_response(
    app: &Router,
    method: &str,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(if body.is_null() {
            Body::empty()
        } else {
            Body::from(serde_json::to_vec(&body).unwrap())
        })
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

fn types(list: &serde_json::Value) -> Vec<String> {
    list["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap().to_string())
        .collect()
}

/// Add the SDK-required Environment to an exact wire fixture. Call sites remain
/// explicit about constructing a Session request; the test harness never grants
/// the production-forbidden implicit local fallback.
fn session_request(mut body: serde_json::Value) -> serde_json::Value {
    body.as_object_mut()
        .expect("Session request fixture is an object")
        .insert(
            "environment_id".into(),
            serde_json::json!(awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID),
        );
    body
}

async fn create(app: &Router) -> String {
    let s = json_call(
        app,
        "POST",
        "/v1/sessions",
        session_request(serde_json::json!({ "agent": "coder" })),
    )
    .await;
    s["id"].as_str().unwrap().to_string()
}

/// Session-create idempotency cause/effect decision table.
///
/// | Rule | Key | Payload | Effect |
/// |---|---|---|---|
/// | I1 | absent | same | ordinary independent Sessions |
/// | I2 | valid, repeated | same | one Session id and one durable aggregate |
/// | I3 | valid, repeated | changed | 409 idempotency mismatch |
/// | I4 | empty/overlong | any | 400 before Session creation |
/// | I5 | same key, different owner | same | distinct owner-scoped Sessions |
/// | I6 | valid | non-empty initial events | 400; never replay an event batch |
#[tokio::test]
async fn session_create_idempotency_replays_one_canonical_session() {
    async fn post(app: &Router, key: Option<&str>, title: &str) -> (StatusCode, serde_json::Value) {
        let mut request = Request::builder()
            .method("POST")
            .uri("/v1/sessions")
            .header("content-type", "application/json");
        if let Some(key) = key {
            request = request.header("idempotency-key", key);
        }
        let response = app
            .clone()
            .oneshot(
                request
                    .body(Body::from(
                        serde_json::to_vec(&session_request(serde_json::json!({
                            "agent": "coder",
                            "title": title,
                        })))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, body)
    }

    let app = router(Arc::new(ManagedState::new(EchoFake)));
    let first = post(&app, Some("design-project-a"), "Project A").await;
    let replay = post(&app, Some("design-project-a"), "Project A").await;
    assert_eq!(first.0, StatusCode::OK, "I2 first create succeeds");
    assert_eq!(replay.0, StatusCode::OK, "I2 replay succeeds");
    assert_eq!(first.1["id"], replay.1["id"], "I2 identity is stable");

    let mismatch = post(&app, Some("design-project-a"), "Changed").await;
    assert_eq!(
        mismatch.0,
        StatusCode::CONFLICT,
        "I3 changed payload conflicts"
    );

    let independent_a = post(&app, None, "ordinary").await;
    let independent_b = post(&app, None, "ordinary").await;
    assert_ne!(
        independent_a.1["id"], independent_b.1["id"],
        "I1 preserves ordinary create"
    );

    let invalid = post(&app, Some(""), "invalid").await;
    assert_eq!(
        invalid.0,
        StatusCode::BAD_REQUEST,
        "I4 rejects an empty key"
    );
    let overlong = "x".repeat(256);
    let invalid = post(&app, Some(&overlong), "invalid").await;
    assert_eq!(
        invalid.0,
        StatusCode::BAD_REQUEST,
        "I4 rejects an overlong key"
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/sessions")
                .header("content-type", "application/json")
                .header("idempotency-key", "seeded-session")
                .body(Body::from(
                    serde_json::to_vec(&session_request(serde_json::json!({
                        "agent": "coder",
                        "initial_events": [{
                            "type": "user.message",
                            "content": [{"type": "text", "text": "once"}]
                        }]
                    })))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "I6 rejects a batch whose replay cannot yet be atomic"
    );

    let sessions = json_call(&app, "GET", "/v1/sessions", serde_json::Value::Null).await;
    assert_eq!(
        sessions["data"].as_array().unwrap().len(),
        3,
        "I2/I3/I4 add no duplicate"
    );

    let state = Arc::new(ManagedState::new(EchoFake));
    let request = || {
        serde_json::from_value(session_request(serde_json::json!({ "agent": "coder" }))).unwrap()
    };
    let owner_a = state
        .create_session_with_initial_events_idempotent(
            request(),
            Some("owner-a".into()),
            "shared-key",
        )
        .await
        .unwrap();
    let owner_b = state
        .create_session_with_initial_events_idempotent(
            request(),
            Some("owner-b".into()),
            "shared-key",
        )
        .await
        .unwrap();
    assert_ne!(owner_a.id, owner_b.id, "I5 keys are owner-scoped");
}

fn ended(messages: Vec<Message>) -> StepOutcome {
    StepOutcome::ended(messages, EndCause::NaturalEnd, false, false)
}

/// The happy path: one assistant text reply, no tools.
struct EchoFake;

static ECHO_MESSAGE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[async_trait::async_trait]
impl SessionRuntime for EchoFake {
    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        let user_text = Message::new(Id("u".into()), Role::User, content).text_content();
        Ok(ended(vec![Message::text(
            // Runtime message identity is the active-active projection fence; a
            // test double must obey the production contract that every committed
            // message has a distinct id, including across turns.
            Id(format!(
                "a-{}",
                ECHO_MESSAGE_SEQUENCE.fetch_add(1, Ordering::SeqCst)
            )),
            Role::Assistant,
            format!("echo: {user_text}"),
        )]))
    }
    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("no awaiting run"))
    }
    async fn add_system(&self, _thread: &str, _text: &str) -> Result<(), RunError> {
        Ok(())
    }
    async fn define_outcome(
        &self,
        _t: &str,
        _d: &str,
        _r: &str,
        _m: u32,
    ) -> Result<OutcomeDrive, RunError> {
        Err(RunError::internal("no outcome"))
    }
    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<ContentBlock>,
        _is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("no custom"))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

/// A runtime whose turn fails; the `kind` selects the HTTP status.
struct FailingFake {
    kind: RunErrorKind,
}

impl FailingFake {
    fn with_kind(kind: RunErrorKind) -> Self {
        Self { kind }
    }
}

#[async_trait::async_trait]
impl SessionRuntime for FailingFake {
    async fn run(
        &self,
        _a: &str,
        _t: &str,
        _c: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        Err(match self.kind {
            RunErrorKind::BadRequest => RunError::bad_request("nope"),
            RunErrorKind::Internal => RunError::internal("boom"),
            RunErrorKind::Unavailable => RunError::unavailable("not ready"),
        })
    }
    async fn resume(
        &self,
        _t: &str,
        _tid: &str,
        _d: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("no resume"))
    }
    async fn resume_custom(
        &self,
        _t: &str,
        _tid: &str,
        _c: Vec<ContentBlock>,
        _e: bool,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("no custom"))
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
        Err(RunError::internal("no outcome"))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

#[tokio::test]
async fn run_error_kind_maps_to_http_status() {
    // Cause/effect decision table: R1 caller-invalid runtime failures map to
    // 400; R2 permanent internal failures map to 500; R3 temporarily unavailable
    // runtime dependencies map to retryable 503.
    for (kind, want) in [
        (RunErrorKind::BadRequest, StatusCode::BAD_REQUEST),
        (RunErrorKind::Internal, StatusCode::INTERNAL_SERVER_ERROR),
        (RunErrorKind::Unavailable, StatusCode::SERVICE_UNAVAILABLE),
    ] {
        let app = router(Arc::new(ManagedState::new(FailingFake::with_kind(kind))));
        let id = create(&app).await;
        let req = Request::builder()
            .method("POST")
            .uri(format!("/v1/sessions/{id}/events"))
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "hi" }] }] }))
                    .unwrap(),
            ))
            .unwrap();
        let status = app.clone().oneshot(req).await.unwrap().status();
        assert_eq!(status, want, "{kind:?}");
    }
}

#[tokio::test]
async fn an_unknown_inbound_event_type_is_rejected() {
    // A well-formed body carrying an unknown event `type` fails the tagged-enum
    // decode → 400, not a silently-ignored event.
    let app = router(Arc::new(ManagedState::new(EchoFake)));
    let id = create(&app).await;
    let req = Request::builder()
        .method("POST")
        .uri(format!("/v1/sessions/{id}/events"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({ "events": [{ "type": "user.bogus" }] }))
                .unwrap(),
        ))
        .unwrap();
    let status = app.clone().oneshot(req).await.unwrap().status();
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// The session honors the official `agent` model axis without constructing an
/// unpublished route from a model string. Cause/effect: a plain reference
/// inherits the published model; an equal-id override may change public
/// inference controls; a different id or unavailable version is rejected by the
/// state-layer authority test. Rules: inherit -> published id/version 1;
/// equal id + fast -> same route/fast/inherited version.
#[tokio::test]
async fn session_agent_model_override_is_honored_and_echoed() {
    let app = router(Arc::new(ManagedState::new(EchoFake)));
    // Plain reference → the host default model (`EchoFake::model`), version 1.
    let base = json_call(
        &app,
        "POST",
        "/v1/sessions",
        session_request(serde_json::json!({ "agent": "coder" })),
    )
    .await;
    assert_eq!(base["agent"]["model"]["id"], "test-model");
    assert_eq!(base["agent"]["version"], 1);
    // The official object form changes controls while retaining the published route.
    let over = json_call(
        &app,
        "POST",
        "/v1/sessions",
        session_request(serde_json::json!({
            "agent": {
                "id": "coder",
                "type": "agent_with_overrides",
                "model": { "id": "test-model", "speed": "fast" }
            }
        })),
    )
    .await;
    assert_eq!(over["agent"]["model"]["id"], "test-model");
    assert_eq!(over["agent"]["model"]["speed"], "fast");
    assert_eq!(over["agent"]["version"], 1);
}

/// `agent_with_overrides` with `model: null` clears the model — rejected, since a
/// session always needs one (400, mirroring the API's `agent_model_required`).
#[tokio::test]
async fn clearing_the_model_on_a_session_override_is_rejected() {
    let app = router(Arc::new(ManagedState::new(EchoFake)));
    let req = Request::builder()
        .method("POST")
        .uri("/v1/sessions")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&session_request(serde_json::json!({
                "agent": { "id": "coder", "type": "agent_with_overrides", "model": null }
            })))
            .unwrap(),
        ))
        .unwrap();
    let status = app.clone().oneshot(req).await.unwrap().status();
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Session-create `initial_events` is derived from the documentation's complete
/// cause graph for this field:
///
/// omitted/empty ───────────────────────────────────────────────> create idle
/// 1..=50 message/outcome + <=1 outcome + valid rubric/limits ──> create running,
///                                                                persist in order
/// unsupported type OR invalid member OR >50 OR >1 outcome ─────> reject atomically
///
/// | Rule | Count | Members | Outcomes | Effect |
/// |---|---:|---|---:|---|
/// | C1 | 0 | - | 0 | 200 idle; no execution |
/// | C2 | 1..=50 | message/outcome valid | 0..=1 | 200 running; shared executor |
/// | C3 | valid | unsupported | any | 400; no Session |
/// | C4 | valid | one invalid in mixed batch | any | 400; no partial Session/event |
/// | C5 | 51 | otherwise valid | 0 | 400 |
/// | C6 | valid | outcome missing rubric or two outcomes | >1/invalid | 400 |
#[tokio::test]
async fn session_initial_events_follow_the_atomic_decision_table() {
    let idle_app = router(Arc::new(ManagedState::new(EchoFake)));
    for (rule, body) in [
        (
            "C1 omitted",
            session_request(serde_json::json!({"agent": "coder"})),
        ),
        (
            "C1 empty",
            session_request(serde_json::json!({"agent": "coder", "initial_events": []})),
        ),
    ] {
        let (status, session) = json_response(&idle_app, "POST", "/v1/sessions", body).await;
        assert_eq!(status, StatusCode::OK, "{rule}");
        assert_eq!(session["status"], "idle", "{rule}");
    }

    let running_app = router(Arc::new(ManagedState::new(EchoFake)));
    let (status, session) = json_response(
        &running_app,
        "POST",
        "/v1/sessions",
        session_request(serde_json::json!({
            "agent": "coder",
            "initial_events": [{
                "type": "user.message",
                "content": [{"type": "text", "text": "start"}]
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "C2 message admitted");
    assert_eq!(session["status"], "running", "C2 starts immediately");
    let id = session["id"].as_str().unwrap();
    let mut completed = None;
    for _ in 0..100 {
        let events = json_call(
            &running_app,
            "GET",
            &format!("/v1/sessions/{id}/events"),
            serde_json::Value::Null,
        )
        .await;
        if types(&events).last().map(String::as_str) == Some("session.status_idle") {
            completed = Some(events);
            break;
        }
        tokio::task::yield_now().await;
    }
    let completed = completed.expect("C2 initial event completes through shared executor");
    assert_eq!(
        types(&completed),
        vec![
            "user.message",
            "session.status_running",
            "agent.message",
            "session.status_idle"
        ],
        "C2 preserves inbound-before-output ordering"
    );
    assert!(completed["data"][0]["processed_at"].is_string());

    let outcome_app = router(Arc::new(ManagedState::new(OutcomeFake)));
    let (status, session) = json_response(
        &outcome_app,
        "POST",
        "/v1/sessions",
        session_request(serde_json::json!({
            "agent": "coder",
            "initial_events": [{
                "type": "user.define_outcome",
                "description": "finish",
                "rubric": {"type": "text", "content": "done"},
                "max_iterations": 20
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "C2 single outcome admitted");
    assert_eq!(
        session["status"], "running",
        "C2 outcome starts immediately"
    );

    let rejected_app = router(Arc::new(ManagedState::new(EchoFake)));
    let message = serde_json::json!({
        "type": "user.message",
        "content": [{"type": "text", "text": "must not run"}]
    });
    let outcome = serde_json::json!({
        "type": "user.define_outcome",
        "description": "finish",
        "rubric": {"type": "text", "content": "done"}
    });
    let invalid_cases = [
        (
            "C3 unsupported",
            session_request(serde_json::json!({"agent":"coder", "initial_events":[{
                "type":"system.message", "content":[{"type":"text", "text":"x"}]
            }]})),
        ),
        (
            "C4 mixed atomic",
            session_request(serde_json::json!({"agent":"coder", "initial_events":[
                message.clone(), {"type":"user.interrupt"}
            ]})),
        ),
        (
            "C5 over maximum",
            session_request(serde_json::json!({
                "agent":"coder",
                "initial_events": vec![message.clone(); 51]
            })),
        ),
        (
            "C6 two outcomes",
            session_request(serde_json::json!({
                "agent":"coder",
                "initial_events":[outcome.clone(), outcome]
            })),
        ),
        (
            "C6 missing rubric",
            session_request(serde_json::json!({"agent":"coder", "initial_events":[{
                "type":"user.define_outcome", "description":"finish"
            }]})),
        ),
    ];
    for (rule, body) in invalid_cases {
        let (status, _) = json_response(&rejected_app, "POST", "/v1/sessions", body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{rule}");
    }
    let sessions = json_call(
        &rejected_app,
        "GET",
        "/v1/sessions",
        serde_json::Value::Null,
    )
    .await;
    assert!(sessions["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn happy_path_projects_message_and_idle() {
    let app = router(Arc::new(ManagedState::new(EchoFake)));
    let id = create(&app).await;
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "hi" }] }] }),
    )
    .await;
    let list = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        types(&list),
        vec![
            "user.message",
            "session.status_running",
            "agent.message",
            "session.status_idle"
        ]
    );
}

/// A runtime that reports a provisioned surface, exercised by session creation.
struct CapableFake;

#[async_trait::async_trait]
impl SessionRuntime for CapableFake {
    async fn run(
        &self,
        _a: &str,
        _t: &str,
        _content: Vec<ContentBlock>,
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
    fn capabilities(&self) -> AgentCapabilities {
        AgentCapabilities {
            builtin_tools: vec![
                BuiltinTool {
                    name: "read".into(),
                    ask: false,
                },
                BuiltinTool {
                    name: "write".into(),
                    ask: true,
                },
            ],
            custom_tools: vec![CustomTool {
                name: "submit".into(),
                description: "Submit the answer".into(),
                input_schema: serde_json::json!({ "type": "object" }),
            }],
            skills: vec!["deploy".into()],
            delegates: vec!["researcher".into()],
        }
    }
}

/// A created session advertises the runtime's surface on its agent object: the
/// built-in toolset, a custom tool, skills, and a multiagent roster — not an empty set.
#[tokio::test]
async fn create_session_advertises_capabilities() {
    let app = router(Arc::new(ManagedState::new(CapableFake)));
    let s = json_call(
        &app,
        "POST",
        "/v1/sessions",
        session_request(serde_json::json!({ "agent": "coder" })),
    )
    .await;

    let tools = s["agent"]["tools"].as_array().unwrap();
    assert_eq!(tools[0]["type"], "agent_toolset_20260401");
    assert_eq!(tools[1]["type"], "custom");
    assert_eq!(tools[1]["name"], "submit");
    assert!(s["agent"]["mcp_servers"].as_array().unwrap().is_empty());
    assert_eq!(s["agent"]["skills"][0]["skill_id"], "deploy");
    assert_eq!(s["agent"]["multiagent"]["type"], "coordinator");
    assert!(s["resources"].as_array().unwrap().is_empty());
}

/// The default capability surface is empty: a runtime that does not override
/// `capabilities` advertises no tools, skills, or resources, and omits `multiagent`.
#[tokio::test]
async fn create_session_defaults_to_empty_surface() {
    let app = router(Arc::new(ManagedState::new(EchoFake)));
    let s = json_call(
        &app,
        "POST",
        "/v1/sessions",
        session_request(serde_json::json!({ "agent": "coder" })),
    )
    .await;
    assert!(s["agent"]["tools"].as_array().unwrap().is_empty());
    assert!(s["agent"]["skills"].as_array().unwrap().is_empty());
    assert!(s["agent"]["multiagent"].is_null());
    assert!(s["resources"].as_array().unwrap().is_empty());
}

/// Golden wire contract for the created session's agent object: the exact Managed
/// Agents shapes the SDK parses — one `agent_toolset_20260401` reference (with the
/// unregistered tools disabled and the confirmation-gated ones `always_ask`), a
/// `custom` tool, a `custom` skill reference, a `coordinator` multiagent roster, and
/// empty `mcp_servers` / `resources`. A field rename or extra key breaks this.
#[tokio::test]
async fn session_capability_objects_match_wire_contract() {
    let app = router(Arc::new(ManagedState::new(CapableFake)));
    let s = json_call(
        &app,
        "POST",
        "/v1/sessions",
        session_request(serde_json::json!({ "agent": "coder" })),
    )
    .await;

    assert_eq!(
        s["agent"]["tools"],
        serde_json::json!([
            {
                "type": "agent_toolset_20260401",
                "configs": [
                    { "name": "bash", "enabled": false, "permission_policy": { "type": "always_allow" } },
                    { "name": "write", "enabled": true, "permission_policy": { "type": "always_ask" } },
                    { "name": "edit", "enabled": false, "permission_policy": { "type": "always_allow" } },
                    { "name": "glob", "enabled": false, "permission_policy": { "type": "always_allow" } },
                    { "name": "grep", "enabled": false, "permission_policy": { "type": "always_allow" } },
                    { "name": "web_fetch", "enabled": false, "permission_policy": { "type": "always_allow" } },
                    { "name": "web_search", "enabled": false, "permission_policy": { "type": "always_allow" } }
                ],
                "default_config": { "enabled": true, "permission_policy": { "type": "always_allow" } }
            },
            {
                "type": "custom",
                "name": "submit",
                "description": "Submit the answer",
                "input_schema": { "type": "object" }
            }
        ])
    );
    assert_eq!(s["agent"]["mcp_servers"], serde_json::json!([]));
    assert_eq!(
        s["agent"]["skills"],
        serde_json::json!([{ "type": "custom", "skill_id": "deploy", "version": "latest" }])
    );
    assert_eq!(s["agent"]["multiagent"]["type"], "coordinator");
    let child = &s["agent"]["multiagent"]["agents"][0];
    assert_eq!(child["id"], "researcher");
    assert_eq!(child["name"], "researcher");
    assert_eq!(child["type"], "agent");
    assert_eq!(child["version"], 1);
    assert_eq!(s["resources"], serde_json::json!([]));
}

/// A runtime that awaits on a tool needing approval, then completes on resume.
#[derive(Default)]
struct AwaitingFake {
    awaiting: Mutex<bool>,
}

#[async_trait::async_trait]
impl SessionRuntime for AwaitingFake {
    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        *self.awaiting.lock().unwrap() = true;
        // The assistant asked to run a tool; the run awaiting before executing it.
        Ok(StepOutcome::awaiting(
            vec![Message {
                id: Id("a1".into()),
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call-1".into(),
                    name: "write".into(),
                    input: serde_json::json!({ "path": "x.txt", "content": "hi" }),
                }],
            }],
            Some(Pending {
                tool_use_id: "call-1".into(),
                name: "write".into(),
                input: serde_json::json!({ "path": "x.txt" }),
                client_executed: false,
            }),
            false,
            false,
        ))
    }
    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        decision: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        assert!(decision.allow);
        *self.awaiting.lock().unwrap() = false;
        Ok(ended(vec![
            Message {
                id: Id("t1".into()),
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call-1".into(),
                    content: vec![ContentBlock::text("wrote x.txt")],
                    is_error: false,
                }],
            },
            Message::text(Id("a2".into()), Role::Assistant, "done"),
        ]))
    }
    async fn add_system(&self, _thread: &str, _text: &str) -> Result<(), RunError> {
        Ok(())
    }
    async fn pending_tool(&self, _thread: &str) -> Result<Option<Pending>, RunError> {
        if !*self.awaiting.lock().unwrap() {
            return Ok(None);
        }
        Ok(Some(Pending {
            tool_use_id: "call-1".into(),
            name: "write".into(),
            input: serde_json::json!({"path": "x.txt"}),
            client_executed: false,
        }))
    }
    async fn define_outcome(
        &self,
        _t: &str,
        _d: &str,
        _r: &str,
        _m: u32,
    ) -> Result<OutcomeDrive, RunError> {
        Err(RunError::internal("no outcome"))
    }
    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<ContentBlock>,
        _is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("no custom"))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

/// A fake outcome loop: one revision then satisfied.
struct OutcomeFake;

#[async_trait::async_trait]
impl SessionRuntime for OutcomeFake {
    async fn run(
        &self,
        _a: &str,
        _t: &str,
        _c: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("no turn"))
    }
    async fn resume(
        &self,
        _t: &str,
        _tid: &str,
        _d: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("no resume"))
    }
    async fn add_system(&self, _thread: &str, _text: &str) -> Result<(), RunError> {
        Ok(())
    }
    async fn define_outcome(
        &self,
        _t: &str,
        _d: &str,
        _r: &str,
        _m: u32,
    ) -> Result<OutcomeDrive, RunError> {
        Ok(OutcomeDrive::Completed(OutcomeReport {
            iterations: vec![
                OutcomeIteration {
                    messages: Vec::new(),
                    outcome_id: "outc_1".into(),
                    description: "produce final answer".into(),
                    iteration: 1,
                    result: "needs_revision".into(),
                    explanation: "add FINAL".into(),
                },
                OutcomeIteration {
                    messages: vec![Message::text(
                        Id("r".into()),
                        Role::Assistant,
                        "FINAL answer",
                    )],
                    outcome_id: "outc_1".into(),
                    description: "produce final answer".into(),
                    iteration: 2,
                    result: "satisfied".into(),
                    explanation: "ok".into(),
                },
            ],
        }))
    }
    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<ContentBlock>,
        _is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("no custom"))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

#[tokio::test]
async fn outcome_loop_projects_evaluations() {
    // Causes: explicit max_iterations is the inclusive minimum/maximum or one
    // outside either edge. Constraint: 1..=20. Effects: H8 admits 1/20 and rejects
    // 0/21 before the inbound event is persisted.
    for (iterations, expected) in [
        (0, StatusCode::BAD_REQUEST),
        (1, StatusCode::OK),
        (20, StatusCode::OK),
        (21, StatusCode::BAD_REQUEST),
    ] {
        let boundary_app = router(Arc::new(ManagedState::new(OutcomeFake)));
        let boundary_id = create(&boundary_app).await;
        let (status, _) = json_response(
            &boundary_app,
            "POST",
            &format!("/v1/sessions/{boundary_id}/events"),
            serde_json::json!({ "events": [{
                "type": "user.define_outcome",
                "description": "finish",
                "rubric": { "type": "text", "content": "FINAL" },
                "max_iterations": iterations
            }] }),
        )
        .await;
        assert_eq!(status, expected, "H8 max_iterations={iterations}");
        if expected == StatusCode::BAD_REQUEST {
            let events = json_call(
                &boundary_app,
                "GET",
                &format!("/v1/sessions/{boundary_id}/events"),
                serde_json::Value::Null,
            )
            .await;
            assert!(
                events["data"].as_array().unwrap().is_empty(),
                "H8 invalid boundary is atomic"
            );
        }
    }

    let app = router(Arc::new(ManagedState::new(OutcomeFake)));
    let id = create(&app).await;
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.define_outcome", "description": "finish", "rubric": { "type": "text", "content": "FINAL" }, "max_iterations": 3 }] }),
    )
    .await;
    let list = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        types(&list),
        vec![
            "user.define_outcome",
            "session.status_running",
            "span.outcome_evaluation_start",
            "span.outcome_evaluation_ongoing",
            "span.outcome_evaluation_end",
            "agent.message",
            "span.outcome_evaluation_start",
            "span.outcome_evaluation_ongoing",
            "span.outcome_evaluation_end",
            "session.status_idle"
        ]
    );
    let ends: Vec<&serde_json::Value> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "span.outcome_evaluation_end")
        .collect();
    assert_eq!(ends[0]["result"], "needs_revision");
    assert_eq!(ends[1]["result"], "satisfied");
}

/// Causal graph: two grading rounds for one outcome -> two transient span sequences
/// -> one durable current-state resource keyed by outcome_id.
///
/// Decision table:
/// | rounds | outcome ids | transient ends | durable resources | final state |
/// | 2 | same | 2 | 1 | latest round |
/// This proves replacement behavior, not merely the response shape.
#[tokio::test]
async fn session_records_outcome_evaluations() {
    let app = router(Arc::new(ManagedState::new(OutcomeFake)));
    let id = create(&app).await;
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.define_outcome", "description": "finish", "rubric": { "type": "text", "content": "FINAL" }, "max_iterations": 3 }] }),
    )
    .await;
    let session = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        session["outcome_evaluations"],
        serde_json::json!([
            {
                "completed_at": "2026-01-01T00:00:00Z",
                "description": "produce final answer",
                "explanation": "ok",
                "iteration": 2,
                "outcome_id": "outc_1",
                "result": "satisfied",
                "type": "outcome_evaluation"
            },
        ])
    );
}

#[tokio::test]
async fn hitl_await_confirm_resume() {
    let app = router(Arc::new(ManagedState::new(AwaitingFake::default())));
    let id = create(&app).await;

    // 1. A message -> the run awaits with requires_action.
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "write it" }] }] }),
    )
    .await;
    let list = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        types(&list),
        vec![
            "user.message",
            "session.status_running",
            "agent.tool_use",
            "session.status_idle"
        ]
    );

    let tool_use = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "agent.tool_use")
        .unwrap();
    assert_eq!(tool_use["id"], "call-1");
    assert_eq!(tool_use["evaluated_permission"], "ask");
    let idle = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "session.status_idle")
        .unwrap();
    assert_eq!(idle["stop_reason"]["type"], "requires_action");
    assert_eq!(idle["stop_reason"]["event_ids"][0], "call-1");

    // Causes/constraints while requires_action: system alone or paired only with
    // user.message has no preceding result; a result with the wrong id or wrong
    // built-in/custom kind cannot resolve the pending tool; a matching confirmation
    // followed by system in the same batch is accepted. Effect: every invalid batch
    // is rejected before persistence, then the valid resume completes before the
    // system event is appended. Decision rules: H5-H7.
    for (rule, events) in [
        (
            "H5 system alone",
            serde_json::json!([{
                "type":"system.message",
                "content":[{"type":"text", "text":"after tool"}]
            }]),
        ),
        (
            "H5 system plus user message",
            serde_json::json!([
                {"type":"system.message", "content":[{"type":"text", "text":"after tool"}]},
                {"type":"user.message", "content":[{"type":"text", "text":"continue"}]}
            ]),
        ),
        (
            "H7 wrong tool id",
            serde_json::json!([{
                "type":"user.tool_confirmation",
                "tool_use_id":"call-wrong",
                "result":"allow"
            }]),
        ),
        (
            "H7 wrong resolution kind",
            serde_json::json!([{
                "type":"user.custom_tool_result",
                "custom_tool_use_id":"call-1",
                "content":[{"type":"text", "text":"forged"}]
            }]),
        ),
    ] {
        let (status, _) = json_response(
            &app,
            "POST",
            &format!("/v1/sessions/{id}/events"),
            serde_json::json!({"events": events}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{rule}");
    }
    let unchanged = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(types(&unchanged), types(&list), "H5/H7 no partial events");

    // 2. H6: confirm the tool, then append system context in the same request.
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [
            { "type": "user.tool_confirmation", "tool_use_id": "call-1", "result": "allow" },
            { "type": "system.message", "content": [{"type":"text", "text":"after tool"}] }
        ] }),
    )
    .await;
    let list = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        types(&list),
        vec![
            "user.message",
            "session.status_running",
            "agent.tool_use",
            "session.status_idle",
            "user.tool_confirmation",
            "session.status_running",
            "agent.tool_result",
            "agent.message",
            "session.status_idle",
            "system.message"
        ]
    );
    let last_idle = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|event| event["type"] == "session.status_idle")
        .unwrap();
    assert_eq!(last_idle["stop_reason"]["type"], "end_turn");
}

/// A runtime that awaits on a *client-executed* tool, then completes on the
/// client's result.
#[derive(Default)]
struct CustomToolFake {
    awaiting: Mutex<bool>,
}

#[async_trait::async_trait]
impl SessionRuntime for CustomToolFake {
    async fn run(
        &self,
        _a: &str,
        _t: &str,
        _c: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        *self.awaiting.lock().unwrap() = true;
        Ok(StepOutcome::awaiting(
            vec![Message {
                id: Id("a1".into()),
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "cc1".into(),
                    name: "submit_answer".into(),
                    input: serde_json::json!({ "question": "6x7" }),
                }],
            }],
            Some(Pending {
                tool_use_id: "cc1".into(),
                name: "submit_answer".into(),
                input: serde_json::json!({ "question": "6x7" }),
                client_executed: true,
            }),
            false,
            false,
        ))
    }
    async fn resume(
        &self,
        _t: &str,
        _tid: &str,
        _d: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("expected custom result"))
    }
    async fn resume_custom(
        &self,
        _t: &str,
        _tid: &str,
        content: Vec<ContentBlock>,
        _e: bool,
    ) -> Result<StepOutcome, RunError> {
        *self.awaiting.lock().unwrap() = false;
        let text = extract_text(&content);
        Ok(ended(vec![
            Message {
                id: Id("tr".into()),
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "cc1".into(),
                    content,
                    is_error: false,
                }],
            },
            Message::text(Id("a2".into()), Role::Assistant, format!("got: {text}")),
        ]))
    }
    async fn add_system(&self, _thread: &str, _text: &str) -> Result<(), RunError> {
        Ok(())
    }
    async fn pending_tool(&self, _thread: &str) -> Result<Option<Pending>, RunError> {
        Ok((*self.awaiting.lock().unwrap()).then(|| Pending {
            tool_use_id: "cc1".into(),
            name: "submit_answer".into(),
            input: serde_json::json!({"question": "6x7"}),
            client_executed: true,
        }))
    }
    async fn define_outcome(
        &self,
        _t: &str,
        _d: &str,
        _r: &str,
        _m: u32,
    ) -> Result<OutcomeDrive, RunError> {
        Err(RunError::internal("no outcome"))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

#[tokio::test]
async fn custom_tool_use_await_and_result() {
    // Custom-result cause/effect decision table:
    // R1 matching pending id + text only -> resume and end; R2 matching id +
    // text/image blocks -> preserve ordered blocks through SessionRuntime and the
    // committed agent.tool_result; R3 wrong id/kind (covered by batch validation)
    // -> reject without a partial event. This test exercises R2 while retaining
    // the text-derived assistant behavior expected by text-only consumers.
    let app = router(Arc::new(ManagedState::new(CustomToolFake::default())));
    let id = create(&app).await;

    // A message -> the client tool awaits as agent.custom_tool_use.
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "answer" }] }] }),
    )
    .await;
    let list = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        types(&list),
        vec![
            "user.message",
            "session.status_running",
            "agent.custom_tool_use",
            "session.status_idle"
        ]
    );
    let custom = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "agent.custom_tool_use")
        .unwrap();
    assert_eq!(custom["id"], "cc1");
    assert_eq!(custom["name"], "submit_answer");
    let idle = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "session.status_idle")
        .unwrap();
    assert_eq!(idle["stop_reason"]["type"], "requires_action");
    assert_eq!(idle["stop_reason"]["event_ids"][0], "cc1");

    // The client returns the result -> the run resumes and completes.
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{
            "type": "user.custom_tool_result",
            "custom_tool_use_id": "cc1",
            "content": [
                { "type": "text", "text": "42" },
                { "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo=" } }
            ]
        }] }),
    )
    .await;
    let list = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    let msgs: Vec<&str> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "agent.message")
        .map(|e| e["content"][0]["text"].as_str().unwrap())
        .collect();
    assert!(
        msgs.iter().any(|m| m.contains("got: 42")),
        "messages: {msgs:?}"
    );
    let tool_result = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["type"] == "agent.tool_result")
        .expect("runtime result is projected");
    assert_eq!(tool_result["content"][0]["text"], "42");
    assert_eq!(
        tool_result["content"][1]["source"]["media_type"],
        "image/png"
    );
    assert_eq!(tool_result["content"][1]["source"]["data"], "iVBORw0KGgo=");
    let last = list["data"].as_array().unwrap().last().unwrap();
    assert_eq!(last["stop_reason"]["type"], "end_turn");
}

#[tokio::test]
async fn accept_only_events_are_acknowledged() {
    // Causal rule: every accepted inbound event owns the receipt id, is persisted
    // in request order, and eventually receives `processed_at`; accept-only events
    // produce no additional agent/session projection in this single-machine case.
    let app = router(Arc::new(ManagedState::new(EchoFake)));
    let id = create(&app).await;

    let receipts = json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [
            { "type": "system.message", "content": [{ "type": "text", "text": "be terse" }] },
            { "type": "user.interrupt" }
        ] }),
    )
    .await;
    let receipt_types: Vec<&str> = receipts["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["type"].as_str().unwrap())
        .collect();
    assert_eq!(receipt_types, vec!["system.message", "user.interrupt"]);

    let list = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(types(&list), vec!["system.message", "user.interrupt"]);
    assert!(
        list["data"]
            .as_array()
            .unwrap()
            .iter()
            .all(|event| event["processed_at"].is_string()),
        "accepted inbound events become processed"
    );
    assert_eq!(receipts["data"][0]["id"], list["data"][0]["id"]);
    assert_eq!(receipts["data"][1]["id"], list["data"][1]["id"]);
}

/// A generic `user.tool_result` (keyed by `tool_use_id`) resumes an awaiting run just
/// like `user.custom_tool_result` — both inbound arms land in `resume_custom`
/// (events.rs). The receipt-only test above proves acknowledgement; this proves the
/// generic arm actually drives the resume to completion. `CustomToolFake` is reused
/// unchanged because it awaits a client tool and completes in `resume_custom`.
#[tokio::test]
async fn generic_tool_result_resumes_an_awaiting_run() {
    let app = router(Arc::new(ManagedState::new(CustomToolFake::default())));
    let id = create(&app).await;

    // A message awaits the client tool (asserted by the custom-tool test); here we
    // only need the await so the generic result has a run to resume.
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "answer" }] }] }),
    )
    .await;

    // Deliver the result via the GENERIC arm: `user.tool_result` + `tool_use_id`
    // (not `user.custom_tool_result` + `custom_tool_use_id`).
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.tool_result", "tool_use_id": "cc1", "content": [{ "type": "text", "text": "42" }] }] }),
    )
    .await;

    let list = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    let msgs: Vec<&str> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "agent.message")
        .map(|e| e["content"][0]["text"].as_str().unwrap())
        .collect();
    assert!(
        msgs.iter().any(|m| m.contains("got: 42")),
        "the generic tool_result resumed the run: {msgs:?}"
    );
    let last = list["data"].as_array().unwrap().last().unwrap();
    assert_eq!(last["stop_reason"]["type"], "end_turn");
}

/// A runtime that records the `system.message` text and `interrupt` thread it is
/// handed, so a test can assert the inbound verb actually reached the runtime seam
/// (not merely that a receipt came back — see `accept_only_events_are_acknowledged`).
struct RecordingFake {
    systems: Arc<Mutex<Vec<String>>>,
    interrupts: Arc<Mutex<Vec<String>>>,
    subjects: Arc<Mutex<Vec<Option<String>>>>,
    supports_mid_conversation_system: bool,
}

#[async_trait::async_trait]
impl SessionRuntime for RecordingFake {
    async fn run(
        &self,
        _a: &str,
        _t: &str,
        _c: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("unused"))
    }
    async fn run_streaming_attributed(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<ContentBlock>,
        data_subject_id: Option<String>,
        _sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunError> {
        self.subjects.lock().unwrap().push(data_subject_id);
        Ok(ended(Vec::new()))
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
    async fn add_system(&self, _thread: &str, text: &str) -> Result<(), RunError> {
        self.systems.lock().unwrap().push(text.to_string());
        Ok(())
    }
    async fn supports_mid_conversation_system(&self, _thread: &str) -> bool {
        self.supports_mid_conversation_system
    }
    async fn interrupt(&self, thread: &str) -> Result<(), RunError> {
        self.interrupts.lock().unwrap().push(thread.to_string());
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

/// Causes: system content count is 0, 1, 1000, or 1001 and the selected model
/// supports/does not support mid-conversation system input; an interrupt may share
/// a valid batch.
/// Constraints: system content is 1..=1000 and every batch member is validated
/// before persistence.
/// Effects: valid boundaries reach `add_system` and persist; interrupt reaches its
/// runtime port; invalid count/capability returns 400 with no partial event.
/// Decision rules: H1-H4.
#[tokio::test]
async fn system_message_and_interrupt_follow_the_admission_decision_table() {
    let systems = Arc::new(Mutex::new(Vec::new()));
    let interrupts = Arc::new(Mutex::new(Vec::new()));
    let subjects = Arc::new(Mutex::new(Vec::new()));
    let app = router(Arc::new(ManagedState::new(RecordingFake {
        systems: systems.clone(),
        interrupts: interrupts.clone(),
        subjects,
        supports_mid_conversation_system: true,
    })));
    let id = create(&app).await;

    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [
            { "type": "system.message", "content": [{ "type": "text", "text": "be terse" }] },
            { "type": "user.interrupt" }
        ] }),
    )
    .await;

    let thousand = vec![serde_json::json!({"type": "text", "text": "x"}); 1000];
    let (status, _) = json_response(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({"events": [{"type": "system.message", "content": thousand}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "H2 inclusive maximum");

    assert_eq!(
        *systems.lock().unwrap(),
        vec!["be terse".to_string(), "x".repeat(1000)],
        "H1/H2 system.message text reached runtime.add_system"
    );
    assert_eq!(
        *interrupts.lock().unwrap(),
        vec![id.clone()],
        "user.interrupt reached runtime.interrupt with the session thread"
    );

    for (rule, content) in [
        ("H3 empty", Vec::new()),
        (
            "H3 over maximum",
            vec![serde_json::json!({"type": "text", "text": "x"}); 1001],
        ),
    ] {
        let (status, _) = json_response(
            &app,
            "POST",
            &format!("/v1/sessions/{id}/events"),
            serde_json::json!({"events": [{"type": "system.message", "content": content}]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{rule}");
    }
    let events = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        types(&events),
        vec!["system.message", "user.interrupt", "system.message"],
        "H3 rejects before persistence"
    );

    let unsupported = router(Arc::new(ManagedState::new(RecordingFake {
        systems: Arc::new(Mutex::new(Vec::new())),
        interrupts: Arc::new(Mutex::new(Vec::new())),
        subjects: Arc::new(Mutex::new(Vec::new())),
        supports_mid_conversation_system: false,
    })));
    let unsupported_id = create(&unsupported).await;
    let (status, body) = json_response(
        &unsupported,
        "POST",
        &format!("/v1/sessions/{unsupported_id}/events"),
        serde_json::json!({"events": [{
            "type": "system.message",
            "content": [{"type": "text", "text": "x"}]
        }]}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "H4 unsupported model");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("model_does_not_support_mid_conversation_system")
    );
    let events = json_call(
        &unsupported,
        "GET",
        &format!("/v1/sessions/{unsupported_id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert!(events["data"].as_array().unwrap().is_empty(), "H4 no event");
}

/// Official event-envelope decision table: R1 removed `user_profile_id` present
/// -> 400 and no Runtime effect; R2 official user.message only -> admitted with
/// no out-of-contract attribution. User Profiles are a separate resource and do
/// not add a field to the Session events SDK shape.
#[tokio::test]
async fn removed_user_profile_event_field_is_rejected() {
    let subjects = Arc::new(Mutex::new(Vec::new()));
    let app = router(Arc::new(ManagedState::new(RecordingFake {
        systems: Arc::new(Mutex::new(Vec::new())),
        interrupts: Arc::new(Mutex::new(Vec::new())),
        subjects: subjects.clone(),
        supports_mid_conversation_system: true,
    })));
    let id = create(&app).await;
    let (status, _) = json_response(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({
            "events": [{
                "type": "user.message",
                "content": [{ "type": "text", "text": "attributed" }]
            }],
            "user_profile_id": "user_alice"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "R1");
    assert!(subjects.lock().unwrap().is_empty(), "R1");

    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({"events": [{
            "type": "user.message",
            "content": [{"type": "text", "text": "unattributed"}]
        }]}),
    )
    .await;
    assert_eq!(*subjects.lock().unwrap(), vec![None], "R2");
}

/// A runtime that records the ORDER of `interrupt` vs `run` calls and echoes each
/// turn, so a test can prove the documented interrupt-then-redirect batch flow.
struct InterruptRedirectFake {
    order: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl SessionRuntime for InterruptRedirectFake {
    async fn run(
        &self,
        _a: &str,
        _t: &str,
        content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        let text = Message::new(Id("u".into()), Role::User, content).text_content();
        self.order.lock().unwrap().push(format!("run:{text}"));
        Ok(ended(vec![Message::text(
            Id("a".into()),
            Role::Assistant,
            format!("on it: {text}"),
        )]))
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
    async fn interrupt(&self, _thread: &str) -> Result<(), RunError> {
        self.order.lock().unwrap().push("interrupt".into());
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

/// The documented interrupt-then-redirect batch (events-and-streaming: "Send a
/// `user.interrupt` event to stop the agent mid-execution, then follow up with a
/// `user.message` event to redirect it"): a single `events` array carrying
/// `[user.interrupt, user.message]` interrupts first, then runs the redirect turn —
/// in that order — and the new direction produces the turn's `agent.message`.
#[tokio::test]
async fn interrupt_then_message_redirects_in_order() {
    let order = Arc::new(Mutex::new(Vec::new()));
    let app = router(Arc::new(ManagedState::new(InterruptRedirectFake {
        order: order.clone(),
    })));
    let id = create(&app).await;

    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [
            { "type": "user.interrupt" },
            { "type": "user.message", "content": [{ "type": "text", "text": "fix line 42 instead" }] }
        ] }),
    )
    .await;

    // The interrupt is handled before the redirect turn runs (documented order).
    assert_eq!(
        *order.lock().unwrap(),
        vec![
            "interrupt".to_string(),
            "run:fix line 42 instead".to_string()
        ],
        "interrupt is processed first, then the redirect message runs the turn"
    );
    // The redirect produced this turn's agent.message (the new direction ran).
    let list = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert!(
        types(&list).contains(&"agent.message".to_string()),
        "the redirect turn projected an agent.message: {}",
        list
    );
}

#[tokio::test]
async fn retrieve_session_and_sse_event_names() {
    let app = router(Arc::new(ManagedState::new(EchoFake)));
    let id = create(&app).await;

    // GET /v1/sessions/{id} returns the session.
    let session = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(session["id"], id);
    assert_eq!(session["type"], "session");

    // Run a turn, then the SSE stream carries `event:` lines named by type.
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "hi" }] }] }),
    )
    .await;
    let req = Request::builder()
        .method("GET")
        .uri(format!("/v1/sessions/{id}/events/stream"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let sse = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(sse.contains("event: agent.message"), "sse: {sse}");
    assert!(sse.contains("event: session.status_idle"), "sse: {sse}");
}

/// Read a response's status and JSON body.
async fn raw_call(
    app: &Router,
    method: &str,
    uri: &str,
    body: Body,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(body)
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, json)
}

#[tokio::test]
async fn unknown_session_is_404_with_error_envelope() {
    let app = router(Arc::new(ManagedState::new(EchoFake)));
    let (status, body) = raw_call(&app, "GET", "/v1/sessions/nope/events", Body::empty()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The Anthropic error envelope the SDK parses.
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "not_found_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "message is populated: {body}"
    );
}

#[tokio::test]
async fn malformed_body_is_400_with_error_envelope() {
    let app = router(Arc::new(ManagedState::new(EchoFake)));
    // Syntactically broken JSON on a managed route.
    let (status, body) = raw_call(&app, "POST", "/v1/sessions", Body::from("{ not json ")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty())
    );
}

#[tokio::test]
async fn missing_content_type_is_400_with_error_envelope() {
    // A body without `content-type: application/json` is rejected as an invalid
    // request in the envelope shape (not axum's plain-text default).
    let app = router(Arc::new(ManagedState::new(EchoFake)));
    let req = Request::builder()
        .method("POST")
        .uri("/v1/sessions")
        .body(Body::from("{}"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
}

#[tokio::test]
async fn caller_fault_is_400_with_invalid_request_envelope() {
    // A caller-fault RunError -> 400 + the invalid_request envelope.
    let app = router(Arc::new(ManagedState::new(FailingFake::with_kind(
        RunErrorKind::BadRequest,
    ))));
    let id = create(&app).await;
    let (status, body) = raw_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        Body::from(
            serde_json::to_vec(&serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "hi" }] }] }))
                .unwrap(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty())
    );
}

// --- Tenant session isolation (ADR-0051): the ownership guard ----------------

use awaken_tenancy::WorkspaceScope;

/// Create a session with an edge-resolved owner scope stamped as `WorkspaceScope`
/// (what the ingress guard does), returning its id.
async fn create_owned(app: &Router, scope: Option<&str>) -> String {
    let mut req = Request::builder()
        .method("POST")
        .uri("/v1/sessions")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&session_request(serde_json::json!({ "agent": "coder" }))).unwrap(),
        ))
        .unwrap();
    if let Some(scope) = scope {
        req.extensions_mut()
            .insert(WorkspaceScope(scope.to_string()));
    }
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    v["id"].as_str().unwrap().to_string()
}

/// A request to `uri` carrying (optionally) an edge-resolved `WorkspaceScope`.
async fn call_owned(app: &Router, method: &str, uri: &str, scope: Option<&str>) -> StatusCode {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    if let Some(scope) = scope {
        req.extensions_mut()
            .insert(WorkspaceScope(scope.to_string()));
    }
    app.clone().oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn a_scoped_session_is_invisible_to_another_workspace() {
    let app = router(Arc::new(ManagedState::new(EchoFake)));
    let id = create_owned(&app, Some("ws_a")).await;
    let path = format!("/v1/sessions/{id}");
    // The owner reads it.
    assert_eq!(
        call_owned(&app, "GET", &path, Some("ws_a")).await,
        StatusCode::OK
    );
    // Another workspace gets 404 — never 403, so the id's existence is not disclosed.
    assert_eq!(
        call_owned(&app, "GET", &path, Some("ws_b")).await,
        StatusCode::NOT_FOUND
    );
    // A bare (unscoped) request cannot see a scoped session either.
    assert_eq!(
        call_owned(&app, "GET", &path, None).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_cross_tenant_write_is_also_fenced() {
    let app = router(Arc::new(ManagedState::new(EchoFake)));
    let id = create_owned(&app, Some("ws_a")).await;
    // A write (archive) from another workspace is 404'd before the handler runs.
    assert_eq!(
        call_owned(
            &app,
            "POST",
            &format!("/v1/sessions/{id}/archive"),
            Some("ws_b")
        )
        .await,
        StatusCode::NOT_FOUND
    );
    // The owner's write is admitted (200).
    assert_eq!(
        call_owned(
            &app,
            "POST",
            &format!("/v1/sessions/{id}/archive"),
            Some("ws_a")
        )
        .await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_bare_session_stays_visible_to_bare_requests() {
    // A single-tenant deployment resolves no workspace; the session owns under the
    // seeded default scope and a bare request (also default) never 404s itself.
    let app = router(Arc::new(ManagedState::new(EchoFake)));
    let id = create_owned(&app, None).await;
    assert_eq!(
        call_owned(&app, "GET", &format!("/v1/sessions/{id}"), None).await,
        StatusCode::OK
    );
    // But a scoped request cannot claim a default-owned session.
    assert_eq!(
        call_owned(&app, "GET", &format!("/v1/sessions/{id}"), Some("ws_a")).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn the_collection_route_is_never_fenced() {
    // POST/GET /v1/sessions has no id → the guard passes it through regardless of scope.
    let app = router(Arc::new(ManagedState::new(EchoFake)));
    let _ = create_owned(&app, Some("ws_a")).await;
    assert_eq!(
        call_owned(&app, "GET", "/v1/sessions", Some("ws_b")).await,
        StatusCode::OK
    );
}

/// The event list is paged by cursor (`?page=<event_id>`?cursor=<event_id>&limit=<n>`limit=<n>`): the pages
/// walk the session's events oldest-first with no gap or overlap, `has_more` and
/// `next_page` bracket the walk, and a fabricated cursor is a 400.
#[tokio::test]
async fn events_are_paged_by_cursor() {
    /* Event-list ordering decision table. Causes: C1 order is absent/asc or
     * desc; C2 a page cursor is absent or names the prior page's terminal
     * event; C3 every fixture event has the same processed_at; C4 order is an
     * unsupported value. Effects: E1 default pages remain chronological; E2
     * desc pages start at the newest committed event and walk without gaps or
     * overlap; E3 commit order is the deterministic tie-break for equal
     * timestamps; E4 invalid order is rejected. Rules: R1=C1(asc)+C2=>E1;
     * R2=C1(desc)+C2+C3=>E2+E3; R3=C4=>E4. */
    let app = router(Arc::new(ManagedState::new(EchoFake)));
    let id = create(&app).await;
    // Two turns → 8 events (user/running/message/idle × 2).
    for text in ["one", "two"] {
        json_call(
            &app,
            "POST",
            &format!("/v1/sessions/{id}/events"),
            serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": text }] }] }),
        )
        .await;
    }
    let ids = |list: &serde_json::Value| -> Vec<String> {
        list["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["id"].as_str().unwrap().to_string())
            .collect()
    };

    // Full, unpaged page: all events, no cursor.
    let full = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    let full_ids = ids(&full);
    assert_eq!(full_ids.len(), 8, "two turns produced eight events");
    assert!(full.get("has_more").is_none());
    assert_eq!(full["next_page"], serde_json::Value::Null);

    // First page of 2 → more remain, cursor names the 2nd event.
    let p1 = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events?limit=2"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(ids(&p1), full_ids[0..2]);
    assert!(p1.get("has_more").is_none());
    assert_eq!(p1["next_page"], serde_json::json!(full_ids[1]));

    // RunResume after the cursor, to the end.
    let cursor = p1["next_page"].as_str().unwrap();
    let p2 = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events?page={cursor}&limit=50"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(ids(&p2), full_ids[2..]);
    assert!(p2.get("has_more").is_none());
    assert_eq!(p2["next_page"], serde_json::Value::Null);

    // The two pages reassemble the whole list, in order, no overlap.
    let walked: Vec<String> = ids(&p1).into_iter().chain(ids(&p2)).collect();
    assert_eq!(walked, full_ids);

    let descending = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events?order=desc&limit=2"),
        serde_json::Value::Null,
    )
    .await;
    let descending_ids = ids(&descending);
    assert_eq!(
        descending_ids,
        full_ids.iter().rev().take(2).cloned().collect::<Vec<_>>()
    );
    let descending_cursor = descending["next_page"].as_str().unwrap();
    let descending_tail = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events?order=desc&page={descending_cursor}&limit=50"),
        serde_json::Value::Null,
    )
    .await;
    let descending_walked = descending_ids
        .into_iter()
        .chain(ids(&descending_tail))
        .collect::<Vec<_>>();
    assert_eq!(
        descending_walked,
        full_ids.iter().rev().cloned().collect::<Vec<_>>()
    );

    let invalid_order = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/sessions/{id}/events?order=newest"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(invalid_order.status(), StatusCode::BAD_REQUEST);

    // A fabricated cursor is a caller error (400).
    let bad = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/sessions/{id}/events?page=evt_nope"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
}
