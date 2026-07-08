//! Adapter integration tests with fake runtimes: the happy path, and the HITL
//! park -> `requires_action` -> `user.tool_confirmation` -> resume round-trip.

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_protocol_managed::dto::StopReason;
use awaken_protocol_managed::{
    AgentCapabilities, BuiltinTool, CustomTool, Decision, ManagedState, OutcomeIteration,
    OutcomeReport, Pending, RunError, RunErrorKind, SessionRuntime, TurnOutcome, router,
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
    assert_eq!(resp.status(), StatusCode::OK, "{method} {uri}");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn types(list: &serde_json::Value) -> Vec<String> {
    list["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap().to_string())
        .collect()
}

async fn create(app: &Router) -> String {
    let s = json_call(
        app,
        "POST",
        "/v1/sessions",
        serde_json::json!({ "agent": "coder" }),
    )
    .await;
    s["id"].as_str().unwrap().to_string()
}

/// The happy path: one assistant text reply, no tools.
struct EchoFake;

#[async_trait::async_trait]
impl SessionRuntime for EchoFake {
    async fn run_turn(
        &self,
        _agent: &str,
        _thread: &str,
        content: Vec<ContentBlock>,
    ) -> Result<TurnOutcome, RunError> {
        let user_text = Message::new(Id("u".into()), Role::User, content).text_content();
        Ok(TurnOutcome {
            messages: vec![Message::text(
                Id("a".into()),
                Role::Assistant,
                format!("echo: {user_text}"),
            )],
            stop: StopReason::EndTurn,
            pending: None,
            compacted: false,
        })
    }
    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: Decision,
    ) -> Result<TurnOutcome, RunError> {
        Err(RunError::internal("no parked run"))
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
    ) -> Result<OutcomeReport, RunError> {
        Err(RunError::internal("no outcome"))
    }
    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: &str,
        _is_error: bool,
    ) -> Result<TurnOutcome, RunError> {
        Err(RunError::internal("no custom"))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

/// A runtime whose turn fails; the `kind` selects the HTTP status.
struct FailingFake(RunErrorKind);

#[async_trait::async_trait]
impl SessionRuntime for FailingFake {
    async fn run_turn(
        &self,
        _a: &str,
        _t: &str,
        _c: Vec<ContentBlock>,
    ) -> Result<TurnOutcome, RunError> {
        Err(match self.0 {
            RunErrorKind::BadRequest => RunError::bad_request("nope"),
            RunErrorKind::Internal => RunError::internal("boom"),
        })
    }
    async fn resume(&self, _t: &str, _tid: &str, _d: Decision) -> Result<TurnOutcome, RunError> {
        Err(RunError::internal("no resume"))
    }
    async fn resume_custom(
        &self,
        _t: &str,
        _tid: &str,
        _c: &str,
        _e: bool,
    ) -> Result<TurnOutcome, RunError> {
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
    ) -> Result<OutcomeReport, RunError> {
        Err(RunError::internal("no outcome"))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

#[tokio::test]
async fn run_error_kind_maps_to_http_status() {
    for (kind, want) in [
        (RunErrorKind::BadRequest, StatusCode::BAD_REQUEST),
        (RunErrorKind::Internal, StatusCode::INTERNAL_SERVER_ERROR),
    ] {
        let app = router(Arc::new(ManagedState::new(FailingFake(kind))));
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
    async fn run_turn(
        &self,
        _a: &str,
        _t: &str,
        _content: Vec<ContentBlock>,
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
        serde_json::json!({ "agent": "coder" }),
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
        serde_json::json!({ "agent": "coder" }),
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
        serde_json::json!({ "agent": "coder" }),
    )
    .await;

    assert_eq!(
        s["agent"]["tools"],
        serde_json::json!([
            {
                "type": "agent_toolset_20260401",
                "configs": [
                    { "name": "bash", "enabled": false },
                    { "name": "write", "permission_policy": { "type": "always_ask" } },
                    { "name": "edit", "enabled": false },
                    { "name": "glob", "enabled": false },
                    { "name": "grep", "enabled": false },
                    { "name": "web_fetch", "enabled": false },
                    { "name": "web_search", "enabled": false }
                ]
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
    assert_eq!(
        s["agent"]["multiagent"],
        serde_json::json!({ "type": "coordinator", "agents": ["researcher"] })
    );
    assert_eq!(s["resources"], serde_json::json!([]));
}

/// A runtime that parks on a tool needing approval, then completes on resume.
struct ParkingFake;

#[async_trait::async_trait]
impl SessionRuntime for ParkingFake {
    async fn run_turn(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<ContentBlock>,
    ) -> Result<TurnOutcome, RunError> {
        // The assistant asked to run a tool; the run parked before executing it.
        Ok(TurnOutcome {
            messages: vec![Message {
                id: Id("a1".into()),
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call-1".into(),
                    name: "write".into(),
                    input: serde_json::json!({ "path": "x.txt", "content": "hi" }),
                }],
            }],
            stop: StopReason::RequiresAction {
                event_ids: Vec::new(),
            },
            pending: Some(Pending {
                tool_use_id: "call-1".into(),
                name: "write".into(),
                input: serde_json::json!({ "path": "x.txt" }),
                client_executed: false,
            }),
            compacted: false,
        })
    }
    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        decision: Decision,
    ) -> Result<TurnOutcome, RunError> {
        assert!(decision.allow);
        Ok(TurnOutcome {
            messages: vec![
                Message {
                    id: Id("t1".into()),
                    role: Role::Tool,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "call-1".into(),
                        content: vec![ContentBlock::text("wrote x.txt")],
                    }],
                },
                Message::text(Id("a2".into()), Role::Assistant, "done"),
            ],
            stop: StopReason::EndTurn,
            pending: None,
            compacted: false,
        })
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
    ) -> Result<OutcomeReport, RunError> {
        Err(RunError::internal("no outcome"))
    }
    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: &str,
        _is_error: bool,
    ) -> Result<TurnOutcome, RunError> {
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
    async fn run_turn(
        &self,
        _a: &str,
        _t: &str,
        _c: Vec<ContentBlock>,
    ) -> Result<TurnOutcome, RunError> {
        Err(RunError::internal("no turn"))
    }
    async fn resume(&self, _t: &str, _tid: &str, _d: Decision) -> Result<TurnOutcome, RunError> {
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
    ) -> Result<OutcomeReport, RunError> {
        Ok(OutcomeReport {
            iterations: vec![
                OutcomeIteration {
                    messages: Vec::new(),
                    outcome_id: "outc_1".into(),
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
                    iteration: 2,
                    result: "satisfied".into(),
                    explanation: "ok".into(),
                },
            ],
        })
    }
    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: &str,
        _is_error: bool,
    ) -> Result<TurnOutcome, RunError> {
        Err(RunError::internal("no custom"))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

#[tokio::test]
async fn outcome_loop_projects_evaluations() {
    let app = router(Arc::new(ManagedState::new(OutcomeFake)));
    let id = create(&app).await;
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.define_outcome", "description": "finish", "rubric": "FINAL", "max_iterations": 3 }] }),
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
            "session.status_running",
            "span.outcome_evaluation_start",
            "span.outcome_evaluation_end",
            "agent.message",
            "span.outcome_evaluation_start",
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

/// After the outcome loop runs, the session object's `outcome_evaluations` records
/// each graded round in its wire shape — durable state, distinct from the transient
/// `span.outcome_evaluation_*` events — so a `GET /v1/sessions/{id}` is no longer [].
#[tokio::test]
async fn session_records_outcome_evaluations() {
    let app = router(Arc::new(ManagedState::new(OutcomeFake)));
    let id = create(&app).await;
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.define_outcome", "description": "finish", "rubric": "FINAL", "max_iterations": 3 }] }),
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
            { "outcome_id": "outc_1", "result": "needs_revision" },
            { "outcome_id": "outc_1", "result": "satisfied" },
        ])
    );
}

#[tokio::test]
async fn hitl_park_confirm_resume() {
    let app = router(Arc::new(ManagedState::new(ParkingFake)));
    let id = create(&app).await;

    // 1. A message -> the run parks with requires_action.
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

    // 2. Confirm the tool -> the run resumes and completes.
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.tool_confirmation", "tool_use_id": "call-1", "result": "allow" }] }),
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
            "session.status_running",
            "agent.tool_use",
            "session.status_idle",
            "session.status_running",
            "agent.tool_result",
            "agent.message",
            "session.status_idle"
        ]
    );
    let last_idle = list["data"].as_array().unwrap().last().unwrap();
    assert_eq!(last_idle["stop_reason"]["type"], "end_turn");
}

/// A runtime that parks on a *client-executed* tool, then completes on the
/// client's result.
struct CustomToolFake;

#[async_trait::async_trait]
impl SessionRuntime for CustomToolFake {
    async fn run_turn(
        &self,
        _a: &str,
        _t: &str,
        _c: Vec<ContentBlock>,
    ) -> Result<TurnOutcome, RunError> {
        Ok(TurnOutcome {
            messages: vec![Message {
                id: Id("a1".into()),
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "cc1".into(),
                    name: "submit_answer".into(),
                    input: serde_json::json!({ "question": "6x7" }),
                }],
            }],
            stop: StopReason::RequiresAction {
                event_ids: Vec::new(),
            },
            pending: Some(Pending {
                tool_use_id: "cc1".into(),
                name: "submit_answer".into(),
                input: serde_json::json!({ "question": "6x7" }),
                client_executed: true,
            }),
            compacted: false,
        })
    }
    async fn resume(&self, _t: &str, _tid: &str, _d: Decision) -> Result<TurnOutcome, RunError> {
        Err(RunError::internal("expected custom result"))
    }
    async fn resume_custom(
        &self,
        _t: &str,
        _tid: &str,
        content: &str,
        _e: bool,
    ) -> Result<TurnOutcome, RunError> {
        Ok(TurnOutcome {
            messages: vec![
                Message {
                    id: Id("tr".into()),
                    role: Role::Tool,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "cc1".into(),
                        content: vec![ContentBlock::text(content)],
                    }],
                },
                Message::text(Id("a2".into()), Role::Assistant, format!("got: {content}")),
            ],
            stop: StopReason::EndTurn,
            pending: None,
            compacted: false,
        })
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
    ) -> Result<OutcomeReport, RunError> {
        Err(RunError::internal("no outcome"))
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

#[tokio::test]
async fn custom_tool_use_park_and_result() {
    let app = router(Arc::new(ManagedState::new(CustomToolFake)));
    let id = create(&app).await;

    // A message -> the client tool parks as agent.custom_tool_use.
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
        serde_json::json!({ "events": [{ "type": "user.custom_tool_result", "custom_tool_use_id": "cc1", "content": [{ "type": "text", "text": "42" }] }] }),
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
    let last = list["data"].as_array().unwrap().last().unwrap();
    assert_eq!(last["stop_reason"]["type"], "end_turn");
}

#[tokio::test]
async fn accept_only_events_are_acknowledged() {
    // `system.message` is buffered; `user.interrupt` / `user.pause` /
    // `user.resume` are acknowledged with a receipt but drive no projected event
    // in the single-machine model (there is no in-flight turn between requests).
    let app = router(Arc::new(ManagedState::new(EchoFake)));
    let id = create(&app).await;

    let receipts = json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [
            { "type": "system.message", "content": [{ "type": "text", "text": "be terse" }] },
            { "type": "user.interrupt" },
            { "type": "user.pause" },
            { "type": "user.resume" }
        ] }),
    )
    .await;
    let receipt_types: Vec<&str> = receipts["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        receipt_types,
        vec![
            "system.message",
            "user.interrupt",
            "user.pause",
            "user.resume"
        ]
    );

    let list = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert!(
        list["data"].as_array().unwrap().is_empty(),
        "accept-only events project nothing"
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
    let app = router(Arc::new(ManagedState::new(FailingFake(
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
