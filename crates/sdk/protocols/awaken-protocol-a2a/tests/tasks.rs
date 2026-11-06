//! A2A `tasks/get` and `tasks/cancel` over the real router with a runtime that is
//! awaiting on a built-in tool. `tasks/cancel` is A2A's protocol-native "deny": it
//! unblocks the awaiting tool with `allow: false` and reports the task `canceled`.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use awaken_agent_contract::agent::awaiting::PermissionDecision;
use awaken_agent_contract::agent::content::extract_text;
use awaken_agent_contract::agent::message::Message;
use awaken_lease_testkit::ManualClock;
use awaken_protocol_a2a::router;
use awaken_runtime_contract::runs::{Pending, RunError, RunResume, Runs, StepOutcome};
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

/// A runtime perpetually awaiting on a built-in tool `c1`, recording whether it was
/// resumed with a denial.
struct AwaitingRuntime {
    denied: Arc<AtomicBool>,
    started: Arc<AtomicBool>,
}

#[async_trait]
impl Runs for AwaitingRuntime {
    async fn run(
        &self,
        _operation_id: &str,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, RunError> {
        self.started.store(true, Ordering::SeqCst);
        Ok(StepOutcome::awaiting(Vec::new(), Some(pending())))
    }

    async fn resume(
        &self,
        _operation_id: &str,
        _thread: &str,
        _tool_use_id: &str,
        resume: RunResume,
    ) -> Result<StepOutcome, RunError> {
        if let RunResume::Permission(PermissionDecision::Deny { .. }) = resume {
            self.denied.store(true, Ordering::SeqCst);
        }
        Ok(StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }

    async fn pending(&self, _thread: &str) -> Result<Option<Pending>, RunError> {
        Ok(self.started.load(Ordering::SeqCst).then(pending))
    }

    async fn history(&self, _thread: &str) -> Result<Vec<Message>, RunError> {
        Ok(Vec::new())
    }

    fn model(&self) -> String {
        "test".into()
    }
}

fn pending() -> Pending {
    Pending {
        tool_use_id: "c1".into(),
        name: "write".into(),
        input: Value::Null,
        client_executed: false,
    }
}

async fn call(app: Router, body: Value) -> Value {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/a2a")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn rpc(denied: Arc<AtomicBool>, body: Value) -> Value {
    call(
        router(
            Arc::new(AwaitingRuntime {
                denied,
                started: Arc::new(AtomicBool::new(true)),
            }),
            awaken_time::system_clock(),
        ),
        body,
    )
    .await
}

async fn seed(app: Router) -> String {
    let response = call(
        app,
        json!({
            "jsonrpc": "2.0", "id": 0, "method": "message/send",
            "params": { "message": { "kind": "message", "messageId": "seed", "contextId": "ctx", "role": "user", "parts": [{ "kind": "text", "text": "start" }] } }
        }),
    ).await;
    response["result"]["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn tasks_get_on_an_awaiting_run_reads_input_required() {
    // Causes: C1 the injected router clock reads t=1s when an awaiting Task
    // is published; C2 it advances to t=2s before a read; C3 the client then
    // cancels that Task. Effects: R1=C1 stamps input-required at t=1s;
    // R2=C1+C2 retains the committed t=1s snapshot on GET; R3=C1+C2+C3
    // publishes a new canceled status at t=2s. No read-time restamping.
    let clock = Arc::new(ManualClock::new(1_000));
    let app = router(
        Arc::new(AwaitingRuntime {
            denied: Arc::new(AtomicBool::new(false)),
            started: Arc::new(AtomicBool::new(false)),
        }),
        clock.clone(),
    );
    let task_id = seed(app.clone()).await;
    let r = call(
        app.clone(),
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tasks/get", "params": { "id": task_id } }),
    )
    .await;
    assert_eq!(r["result"]["status"]["state"], "input-required", "{r}");
    assert_eq!(
        r["result"]["status"]["timestamp"], "1970-01-01T00:00:01Z",
        "R1"
    );
    clock.set(2_000);
    let retained = call(
        app.clone(),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tasks/get", "params": { "id": task_id } }),
    )
    .await;
    assert_eq!(
        retained["result"]["status"]["timestamp"], "1970-01-01T00:00:01Z",
        "R2"
    );
    let canceled = call(
        app,
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tasks/cancel", "params": { "id": task_id } }),
    )
    .await;
    assert_eq!(canceled["result"]["status"]["state"], "canceled", "R3");
    assert_eq!(
        canceled["result"]["status"]["timestamp"], "1970-01-01T00:00:02Z",
        "R3"
    );
}

#[tokio::test]
async fn message_send_on_an_awaiting_context_resumes_it_rather_than_starting_a_fresh_turn() {
    // A2A approval FMECA rule A1: awaiting built-in + one explicit structured
    // allow decision -> resume exactly once and complete, never start a fresh turn.
    let r = rpc(
        Arc::new(AtomicBool::new(false)),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "message/send",
            "params": { "message": { "kind": "message", "messageId": "m1", "contextId": "ctx", "role": "user", "parts": [{ "kind": "data", "data": { "type": "tool-approval", "allow": true, "note": "reviewed" } }] } }
        }),
    )
    .await;
    assert_eq!(r["result"]["status"]["state"], "completed", "{r}");
}

#[tokio::test]
async fn message_send_can_explicitly_deny_an_awaiting_builtin_tool() {
    // Same cause graph as A1. Rule A2: awaiting built-in + explicit allow=false
    // -> Confirm{allow:false}; the exact denial is delivered without canceling
    // the whole Task. A3 text-only and malformed decisions are router-unit
    // failures and never call this runtime seam.
    let denied = Arc::new(AtomicBool::new(false));
    let r = rpc(
        denied.clone(),
        json!({
            "jsonrpc": "2.0",
            "id": 31,
            "method": "message/send",
            "params": { "message": { "kind": "message", "messageId": "m-deny", "contextId": "ctx", "role": "user", "parts": [{ "kind": "data", "data": { "type": "tool-approval", "allow": false, "note": "unsafe" } }] } }
        }),
    )
    .await;
    assert_eq!(r["result"]["status"]["state"], "completed", "{r}");
    assert!(
        denied.load(Ordering::SeqCst),
        "A2 explicit denial delivered"
    );
}

#[tokio::test]
async fn message_stream_can_explicitly_deny_an_awaiting_builtin_tool() {
    // A2A in-band denial cause/effect table: C1 context awaits a built-in tool;
    // C2 transport is send or stream; C3 exactly one valid structured approval
    // carries allow=false. Effects: E1 the shared driver resumes the bound call
    // with Confirm{allow:false}; E2 no fresh Run starts; E3 send returns a
    // completed Task and stream emits one final completed status. R1=C1+send+C3
    // is covered above; this is R2=C1+stream+C3 -> E1+E2+E3. Missing, duplicate,
    // malformed, or text-only decisions are the fail-closed request-table rules.
    let denied = Arc::new(AtomicBool::new(false));
    let app = router(
        Arc::new(AwaitingRuntime {
            denied: denied.clone(),
            started: Arc::new(AtomicBool::new(true)),
        }),
        awaken_time::system_clock(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/a2a")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "jsonrpc": "2.0",
                        "id": 32,
                        "method": "message/stream",
                        "params": { "message": { "kind": "message", "messageId": "m-stream-deny", "contextId": "ctx", "role": "user", "parts": [{ "kind": "data", "data": { "type": "tool-approval", "allow": false, "note": "unsafe" } }] } }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let frames = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    let terminal = frames
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .next_back()
        .expect("R2 stream has a terminal frame");
    assert_eq!(
        terminal["result"]["status"]["state"], "completed",
        "R2/E3: {frames}"
    );
    assert!(denied.load(Ordering::SeqCst), "R2/E1");
}

/// A runtime awaiting on a *client-executed* tool `c2`, recording the content of the
/// `ClientResult` it is resumed with (the router's non-approval resume path).
struct ClientToolRuntime {
    delivered: Arc<Mutex<Option<String>>>,
}

#[async_trait]
impl Runs for ClientToolRuntime {
    async fn run(
        &self,
        _operation_id: &str,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, RunError> {
        unreachable!("the context is already awaiting, so send takes the resume branch")
    }

    async fn resume(
        &self,
        _operation_id: &str,
        _thread: &str,
        _tool_use_id: &str,
        resume: RunResume,
    ) -> Result<StepOutcome, RunError> {
        if let RunResume::ClientResult { content, is_error } = resume {
            assert!(!is_error, "a plain answer is not an error result");
            *self.delivered.lock().unwrap() = Some(extract_text(&content));
        } else {
            panic!("a client-executed tool must be resumed with a ClientResult, got {resume:?}");
        }
        Ok(StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }

    async fn pending(&self, _thread: &str) -> Result<Option<Pending>, RunError> {
        Ok(Some(Pending {
            tool_use_id: "c2".into(),
            name: "submit_answer".into(),
            input: Value::Null,
            client_executed: true,
        }))
    }

    async fn history(&self, _thread: &str) -> Result<Vec<Message>, RunError> {
        Ok(Vec::new())
    }

    fn model(&self) -> String {
        "test".into()
    }
}

#[tokio::test]
async fn message_send_delivers_the_text_as_the_client_tool_result_on_resume() {
    // Cause/effect graph for the shared send driver: C1 the context is pending;
    // C2 transport is request/response (C3 is streaming); C4 the pending tool is
    // client-executed. Effects: E1 call resume, E2 never start a new run, E3 carry
    // the exact text as ClientResult. Decision rule R1=C1,C2,C4 -> E1,E2,E3.
    let delivered = Arc::new(Mutex::new(None));
    let app = router(
        Arc::new(ClientToolRuntime {
            delivered: delivered.clone(),
        }),
        awaken_time::system_clock(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/a2a")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "jsonrpc": "2.0",
                        "id": 4,
                        "method": "message/send",
                        "params": { "message": { "kind": "message", "messageId": "m1", "contextId": "ctx", "role": "user", "parts": [{ "kind": "text", "text": "42" }] } }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let r: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(r["result"]["status"]["state"], "completed", "{r}");
    assert_eq!(delivered.lock().unwrap().as_deref(), Some("42"));
}

#[tokio::test]
async fn message_stream_on_an_awaiting_context_uses_the_same_resume_driver() {
    // Same graph as R1 above. R2=C1,C3,C4 -> E1,E2,E3 plus one terminal SSE
    // status. R3=!C1,C3 -> run_streaming is covered by the SDK streaming tests.
    // Together R1/R2 pin transport parity and prevent a second send path from
    // silently turning a resume message into a fresh Run.
    let delivered = Arc::new(Mutex::new(None));
    let app = router(
        Arc::new(ClientToolRuntime {
            delivered: delivered.clone(),
        }),
        awaken_time::system_clock(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/a2a")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "jsonrpc": "2.0",
                        "id": 5,
                        "method": "message/stream",
                        "params": { "message": { "kind": "message", "messageId": "m2", "contextId": "ctx", "role": "user", "parts": [{ "kind": "text", "text": "streamed 42" }] } }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let frames = String::from_utf8(bytes.to_vec()).unwrap();
    let terminal = frames
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .next_back()
        .expect("stream has a terminal frame");
    assert_eq!(
        terminal["result"]["status"]["state"], "completed",
        "{frames}"
    );
    assert_eq!(delivered.lock().unwrap().as_deref(), Some("streamed 42"));
}

#[tokio::test]
async fn tasks_cancel_denies_the_awaiting_tool_and_reports_canceled() {
    let denied = Arc::new(AtomicBool::new(false));
    let app = router(
        Arc::new(AwaitingRuntime {
            denied: denied.clone(),
            started: Arc::new(AtomicBool::new(false)),
        }),
        awaken_time::system_clock(),
    );
    let task_id = seed(app.clone()).await;
    let r = call(
        app,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tasks/cancel", "params": { "id": task_id } }),
    )
    .await;
    assert_eq!(r["result"]["status"]["state"], "canceled", "{r}");
    assert!(
        denied.load(Ordering::SeqCst),
        "cancel must deny the awaiting tool (resume with allow:false)"
    );
}
