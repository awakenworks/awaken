//! Cause-effect / decision-table coverage for the event → wire projection and the
//! session-lifecycle accounting that the happy-path `adapter.rs` suite leaves
//! uncovered: a terminal run *fault* projected as `session.error` (never swallowed
//! into a success idle), the compaction marker, cumulative usage accounting, an
//! all-empty-text assistant message dropped (matching the shared agent-contract
//! projection), the MCP tool-call events, the `retries_exhausted` idle, the
//! subagent-delegate child-thread lifecycle, the `session.updated` event, and the
//! archived-session read-only fence + committed terminal event.

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Failure};
use awaken_protocol_managed::{
    Decision, ManagedState, OutcomeReport, RunError, SessionRuntime, SessionUsage, StepOutcome,
    router,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

// --- Test harness ------------------------------------------------------------

async fn raw_call(
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
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, json)
}

async fn json_call(
    app: &Router,
    method: &str,
    uri: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let (status, json) = raw_call(app, method, uri, body).await;
    assert_eq!(status, StatusCode::OK, "{method} {uri}: {json}");
    json
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

async fn send_user(app: &Router, id: &str, text: &str) -> serde_json::Value {
    json_call(
        app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": text }] }] }),
    )
    .await
}

async fn list_events(app: &Router, id: &str) -> serde_json::Value {
    json_call(
        app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await
}

/// A runtime whose single turn is a scripted [`StepOutcome`] (rebuilt per call so
/// the non-`Clone` fields are fresh) with a configurable cumulative usage tally.
/// Every other operational method is unused by these projection tests.
struct ScriptFake {
    make: Box<dyn Fn() -> StepOutcome + Send + Sync>,
    usage: SessionUsage,
}

impl ScriptFake {
    fn new(make: impl Fn() -> StepOutcome + Send + Sync + 'static) -> Self {
        Self {
            make: Box::new(make),
            usage: SessionUsage::default(),
        }
    }
    fn with_usage(mut self, usage: SessionUsage) -> Self {
        self.usage = usage;
        self
    }
}

#[async_trait::async_trait]
impl SessionRuntime for ScriptFake {
    async fn run(
        &self,
        _a: &str,
        _t: &str,
        _c: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        Ok((self.make)())
    }
    async fn resume(&self, _t: &str, _tid: &str, _d: Decision) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("no resume"))
    }
    async fn resume_custom(
        &self,
        _t: &str,
        _tid: &str,
        _c: &str,
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
    ) -> Result<OutcomeReport, RunError> {
        Err(RunError::internal("no outcome"))
    }
    async fn session_usage(&self, _t: &str) -> SessionUsage {
        self.usage
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

fn assistant_text(id: &str, text: &str) -> Message {
    Message::text(Id(id.into()), Role::Assistant, text)
}

fn ended(messages: Vec<Message>) -> StepOutcome {
    StepOutcome::ended(messages, EndCause::NaturalEnd, false, false)
}

fn ended_with_flags(messages: Vec<Message>, compacted: bool, rescheduled: bool) -> StepOutcome {
    StepOutcome::ended(messages, EndCause::NaturalEnd, compacted, rescheduled)
}

fn failed(messages: Vec<Message>, code: &str, message: &str) -> StepOutcome {
    StepOutcome::ended(
        messages,
        EndCause::Error(Failure::Inference {
            code: code.into(),
            message: message.into(),
        }),
        false,
        false,
    )
}

// --- CE: terminal run fault → session.error (never swallowed into success) ----

/// CRITICAL bug-class guard: a terminal run fault must project a *distinct*
/// `session.error` event carrying the fault message and `retry_status: exhausted`,
/// so a streaming/listing client observes the failure — it is NOT collapsed into
/// a bare success idle. The turn still idles afterward with `retries_exhausted`,
/// derived from the same `EndCause::Error` that produces the error event.
#[tokio::test]
async fn a_terminal_run_fault_projects_session_error_before_idle() {
    let app = router(Arc::new(ManagedState::new(ScriptFake::new(|| {
        failed(
            vec![assistant_text("a", "partial work")],
            "provider_unavailable",
            "upstream model timed out",
        )
    }))));
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    let list = list_events(&app, &id).await;

    // The error is a first-class event between running and idle — not dropped.
    assert_eq!(
        types(&list),
        vec![
            "session.status_running",
            "session.error",
            "agent.message",
            "session.status_idle"
        ],
        "the fault surfaces as session.error, the turn still idles"
    );
    let err = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "session.error")
        .unwrap();
    assert_eq!(err["error"]["type"], "unknown_error");
    assert_eq!(err["error"]["retry_status"]["type"], "exhausted");
    assert_eq!(
        err["error"]["message"], "upstream model timed out",
        "the neutral fault message is carried through"
    );
    // The idle and error event derive from the same terminal authority.
    let idle = list["data"].as_array().unwrap().last().unwrap();
    assert_eq!(idle["stop_reason"]["type"], "retries_exhausted");
}

/// A classified fault `code` selects the richer SDK error variant + retry status
/// instead of always collapsing to `unknown_error`/`exhausted`: a `rate_limited`
/// code projects `model_rate_limited_error`, and a `context_overflow` code is
/// `model_request_failed_error` with a `terminal` retry status.
#[tokio::test]
async fn a_classified_fault_projects_the_matching_sdk_error_variant() {
    for (code, kind, retry) in [
        ("rate_limited", "model_rate_limited_error", "exhausted"),
        ("context_overflow", "model_request_failed_error", "terminal"),
    ] {
        let app = router(Arc::new(ManagedState::new(ScriptFake::new(move || {
            failed(vec![assistant_text("a", "partial")], code, "boom")
        }))));
        let id = create(&app).await;
        send_user(&app, &id, "go").await;
        let list = list_events(&app, &id).await;
        let err = list["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["type"] == "session.error")
            .unwrap();
        assert_eq!(err["error"]["type"], kind, "code {code} → error type");
        assert_eq!(
            err["error"]["retry_status"]["type"], retry,
            "code {code} → retry"
        );
        assert_eq!(err["error"]["message"], "boom");
    }
}

/// A turn the runtime transparently retried (transient auto-recovery) projects
/// `session.status_rescheduled` between the running marker and the turn's output,
/// so a client observes the recovery rather than an unexplained pause.
#[tokio::test]
async fn a_rescheduled_turn_projects_the_rescheduled_status() {
    let app = router(Arc::new(ManagedState::new(ScriptFake::new(|| {
        ended_with_flags(vec![assistant_text("a", "after a retry")], false, true)
    }))));
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    let list = list_events(&app, &id).await;
    assert_eq!(
        types(&list),
        vec![
            "session.status_running",
            "session.status_rescheduled",
            "agent.message",
            "session.status_idle",
        ],
        "the transient retry surfaces as session.status_rescheduled before the output"
    );
}

// --- CE: compaction marker ---------------------------------------------------

/// A turn that folded its context projects `agent.thread_context_compacted` ahead
/// of the turn's messages (compaction ran at BeforeInference), exactly once.
#[tokio::test]
async fn a_compacted_turn_projects_the_compaction_marker() {
    let app = router(Arc::new(ManagedState::new(ScriptFake::new(|| {
        ended_with_flags(vec![assistant_text("a", "after compaction")], true, false)
    }))));
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    let list = list_events(&app, &id).await;
    assert_eq!(
        types(&list),
        vec![
            "session.status_running",
            "agent.thread_context_compacted",
            "agent.message",
            "session.status_idle"
        ]
    );
    // Exactly one marker (emit-once upstream).
    assert_eq!(
        types(&list)
            .iter()
            .filter(|t| *t == "agent.thread_context_compacted")
            .count(),
        1
    );
}

// --- CE: usage accounting ----------------------------------------------------

/// The session's `usage` object accumulates from the runtime's committed tally: a
/// fresh session is zero, and after a turn a GET reflects the runtime's counts
/// (each wire field mapped from the neutral `SessionUsage`, none transposed).
#[tokio::test]
async fn session_usage_reflects_the_runtime_tally() {
    let usage = SessionUsage {
        input_tokens: 120,
        output_tokens: 45,
        cache_read_tokens: 30,
        cache_creation_tokens: 12,
    };
    let app = router(Arc::new(ManagedState::new(
        ScriptFake::new(|| ended(vec![assistant_text("a", "hi")])).with_usage(usage),
    )));
    let id = create(&app).await;

    // Zero until the first turn commits usage.
    let before = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(before["usage"]["input_tokens"], 0);

    send_user(&app, &id, "go").await;
    let after = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(after["usage"]["input_tokens"], 120);
    assert_eq!(after["usage"]["output_tokens"], 45);
    // The neutral cache_read/cache_creation map onto the SDK's prefixed field names.
    assert_eq!(after["usage"]["cache_read_input_tokens"], 30);
    assert_eq!(after["usage"]["cache_creation_input_tokens"], 12);
}

// --- CE: all-empty-text assistant message dropped ----------------------------

/// The shared agent-contract projection drops an all-empty-text assistant message
/// (no visible text, no tool calls). The managed projection inherits that fold, so
/// such a turn commits *no* `agent.message` — only the running/idle bracket.
#[tokio::test]
async fn an_all_empty_text_assistant_message_is_dropped() {
    let app = router(Arc::new(ManagedState::new(ScriptFake::new(|| {
        ended(vec![assistant_text("a", "")])
    }))));
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    let list = list_events(&app, &id).await;
    assert_eq!(
        types(&list),
        vec!["session.status_running", "session.status_idle"],
        "an empty assistant message projects no agent.message"
    );
    assert!(
        !types(&list).iter().any(|t| t == "agent.message"),
        "no useless empty agent.message event"
    );
}

// --- CE: MCP tool call → distinct mcp events ---------------------------------

/// A host-executed MCP tool call (`mcp__<server>__<tool>`) projects the *distinct*
/// `agent.mcp_tool_use` / `agent.mcp_tool_result` events (carrying `mcp_server_name`
/// and keyed by `mcp_tool_use_id`), not the generic `agent.tool_use` pair.
#[tokio::test]
async fn an_mcp_tool_call_projects_mcp_events() {
    let app = router(Arc::new(ManagedState::new(ScriptFake::new(|| {
        ended(vec![
            Message::new(
                Id("a".into()),
                Role::Assistant,
                vec![
                    ContentBlock::text("searching"),
                    ContentBlock::ToolUse {
                        id: "mc1".into(),
                        name: "mcp__github__search".into(),
                        input: serde_json::json!({ "q": "rust" }),
                    },
                ],
            ),
            Message::new(
                Id("t".into()),
                Role::Tool,
                vec![ContentBlock::ToolResult {
                    tool_use_id: "mc1".into(),
                    content: vec![ContentBlock::text("3 hits")],
                }],
            ),
        ])
    }))));
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    let list = list_events(&app, &id).await;
    assert_eq!(
        types(&list),
        vec![
            "session.status_running",
            "agent.message",
            "agent.mcp_tool_use",
            "agent.mcp_tool_result",
            "session.status_idle"
        ]
    );
    let use_ev = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "agent.mcp_tool_use")
        .unwrap();
    assert_eq!(use_ev["id"], "mc1");
    assert_eq!(use_ev["mcp_server_name"], "github");
    assert_eq!(use_ev["evaluated_permission"], "allow");
    let res_ev = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "agent.mcp_tool_result")
        .unwrap();
    assert_eq!(res_ev["mcp_tool_use_id"], "mc1");
}

// --- CE: retries_exhausted idle stop reason ----------------------------------

/// A turn whose `stop` is `RetriesExhausted` idles with that exact tagged stop
/// reason on the wire (distinct from `end_turn` / `requires_action`).
#[tokio::test]
async fn a_retries_exhausted_turn_idles_with_that_stop_reason() {
    let app = router(Arc::new(ManagedState::new(ScriptFake::new(|| {
        StepOutcome::ended(
            vec![assistant_text("a", "gave up")],
            EndCause::MaxSteps,
            false,
            false,
        )
    }))));
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    let list = list_events(&app, &id).await;
    let idle = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "session.status_idle")
        .unwrap();
    assert_eq!(idle["stop_reason"]["type"], "retries_exhausted");
}

// --- CE: subagent-delegate child-thread lifecycle ----------------------------

/// An inline `agent_run` delegation projects the delegate's full child-thread
/// lifecycle after the turn's own idle — `session.thread_created` →
/// `session.thread_status_running` → the input sent → the reply received →
/// `session.thread_status_idle` — and the child thread is enumerated by
/// `GET /threads` with the primary as its parent.
#[tokio::test]
async fn a_delegation_projects_the_child_thread_lifecycle() {
    let app = router(Arc::new(ManagedState::new(ScriptFake::new(|| {
        ended(vec![
            Message::new(
                Id("a".into()),
                Role::Assistant,
                vec![
                    ContentBlock::text("delegating"),
                    ContentBlock::ToolUse {
                        id: "d1".into(),
                        name: "agent_run".into(),
                        input: serde_json::json!({ "agent_id": "researcher", "input": "find the docs" }),
                    },
                ],
            ),
            Message::new(
                Id("t".into()),
                Role::Tool,
                vec![ContentBlock::ToolResult {
                    tool_use_id: "d1".into(),
                    content: vec![ContentBlock::text("here are the docs")],
                }],
            ),
        ])
    }))));
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    let list = list_events(&app, &id).await;
    assert_eq!(
        types(&list),
        vec![
            "session.status_running",
            "agent.message",
            "agent.tool_use",
            "agent.tool_result",
            "session.status_idle",
            "session.thread_created",
            "session.thread_status_running",
            "agent.thread_message_sent",
            "agent.thread_message_received",
            "session.thread_status_idle",
        ]
    );
    let created = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "session.thread_created")
        .unwrap();
    assert_eq!(created["agent_name"], "researcher");
    let child_thread_id = created["session_thread_id"].as_str().unwrap().to_string();
    // The input the coordinator sent and the reply it received are carried on the wire.
    let sent = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "agent.thread_message_sent")
        .unwrap();
    assert_eq!(sent["content"][0]["text"], "find the docs");
    let recv = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "agent.thread_message_received")
        .unwrap();
    assert_eq!(recv["content"][0]["text"], "here are the docs");

    // GET /threads enumerates the primary plus the child, parented to the primary.
    let threads = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/threads"),
        serde_json::Value::Null,
    )
    .await;
    let arr = threads["data"].as_array().unwrap();
    assert_eq!(arr.len(), 2, "primary + one delegate child");
    let child = arr
        .iter()
        .find(|t| t["id"] == child_thread_id.as_str())
        .unwrap();
    assert_eq!(child["parent_thread_id"], format!("{id}:primary"));
    assert_eq!(child["agent"]["name"], "researcher");
}

// --- CE: session.updated event -----------------------------------------------

/// `POST /v1/sessions/{id}` (title + metadata patch) commits a `session.updated`
/// event carrying the new title and the full metadata bag, so a streaming/listing
/// client observes the mutation — not only the mutated GET view.
#[tokio::test]
async fn updating_a_session_commits_a_session_updated_event() {
    let app = router(Arc::new(ManagedState::new(ScriptFake::new(|| {
        ended(vec![assistant_text("a", "hi")])
    }))));
    let id = create(&app).await;
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}"),
        serde_json::json!({ "title": "renamed", "metadata": { "team": "research" } }),
    )
    .await;
    let list = list_events(&app, &id).await;
    let updated = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "session.updated")
        .unwrap();
    assert_eq!(updated["title"], "renamed");
    assert_eq!(updated["metadata"]["team"], "research");
}

// --- CE: archive commits the terminal event + makes the session read-only ----

/// Archiving commits a `session.status_terminated` event (a streaming/listing
/// client sees the terminal transition), fences every subsequent write with a 409
/// read-only conflict, and is idempotent: a re-archive commits no second terminal
/// event.
#[tokio::test]
async fn archiving_commits_a_terminal_event_and_fences_writes() {
    let app = router(Arc::new(ManagedState::new(ScriptFake::new(|| {
        ended(vec![assistant_text("a", "hi")])
    }))));
    let id = create(&app).await;

    let archived = json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/archive"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(archived["status"], "terminated");
    assert!(archived["archived_at"].is_string());

    let list = list_events(&app, &id).await;
    assert_eq!(
        types(&list)
            .iter()
            .filter(|t| *t == "session.status_terminated")
            .count(),
        1,
        "one committed terminal event"
    );

    // A write to the archived session is 409 read-only, in the error envelope.
    let (status, body) = raw_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "again" }] }] }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["type"], "invalid_request_error");

    // Re-archive is idempotent: still terminated, and no second terminal event.
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/archive"),
        serde_json::Value::Null,
    )
    .await;
    let list = list_events(&app, &id).await;
    assert_eq!(
        types(&list)
            .iter()
            .filter(|t| *t == "session.status_terminated")
            .count(),
        1,
        "re-archive commits no second terminal event"
    );
}
