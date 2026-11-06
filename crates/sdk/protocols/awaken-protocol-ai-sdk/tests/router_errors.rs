//! Fail-closed resume over the real AI SDK router: a tool decision delivered when
//! no run is awaiting yields an `error` stream frame, not a silent success.

use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_runtime_contract::runs::{
    OwnedRuns, Pending, RunError, RunResume, Runs, StepOutcome, blocks_text,
};
use axum::body::{Body, to_bytes};
use axum::http::Request;
use serde_json::{Value, json};
use tower::ServiceExt;

struct NoAwaitingRuntime;

#[async_trait::async_trait]
impl Runs for NoAwaitingRuntime {
    async fn run(
        &self,
        _operation_id: &str,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, RunError> {
        Ok(StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }

    async fn resume(
        &self,
        _operation_id: &str,
        _thread: &str,
        _tool_use_id: &str,
        _resume: RunResume,
    ) -> Result<StepOutcome, RunError> {
        unreachable!("nothing is awaiting, so resume must never be reached")
    }

    async fn pending(&self, _thread: &str) -> Result<Option<Pending>, RunError> {
        Ok(None)
    }

    async fn history(&self, _thread: &str) -> Result<Vec<Message>, RunError> {
        Ok(Vec::new())
    }

    fn model(&self) -> String {
        "test".into()
    }
}

#[derive(Default)]
struct ReplayRecordingRuntime {
    runs: Mutex<Vec<(String, Vec<Message>)>>,
    owners: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Runs for ReplayRecordingRuntime {
    async fn run(
        &self,
        operation_id: &str,
        _thread: &str,
        _agent: Option<String>,
        messages: Vec<Message>,
    ) -> Result<StepOutcome, RunError> {
        self.runs
            .lock()
            .unwrap()
            .push((operation_id.to_string(), messages));
        Ok(StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }

    async fn resume(
        &self,
        _operation_id: &str,
        _thread: &str,
        _tool_use_id: &str,
        _resume: RunResume,
    ) -> Result<StepOutcome, RunError> {
        unreachable!("a known turn replay is a Run admission, not a tool resume")
    }

    async fn pending(&self, _thread: &str) -> Result<Option<Pending>, RunError> {
        Ok(None)
    }

    async fn history(&self, _thread: &str) -> Result<Vec<Message>, RunError> {
        Ok(vec![Message::text(Id("u1".into()), Role::User, "original")])
    }

    fn model(&self) -> String {
        "replay-recording".into()
    }
}

#[async_trait::async_trait]
impl OwnedRuns for ReplayRecordingRuntime {
    async fn run_streaming_for_owner(
        &self,
        owner_scope: &str,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
        _sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunError> {
        self.owners.lock().unwrap().push(owner_scope.to_string());
        self.run(operation_id, thread, agent, messages).await
    }

    async fn resume_for_owner(
        &self,
        _owner_scope: &str,
        _operation_id: &str,
        _thread: &str,
        _tool_use_id: &str,
        _resume: RunResume,
    ) -> Result<StepOutcome, RunError> {
        unreachable!("replay recording never resumes")
    }
}

async fn frames(body: Value) -> Vec<Value> {
    frames_at("/v1/ai-sdk/chat", body).await
}

async fn frames_at(uri: &str, body: Value) -> Vec<Value> {
    let app = awaken_protocol_ai_sdk::router::router(
        Arc::new(NoAwaitingRuntime),
        "test-workspace".into(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    text.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .filter_map(|d| serde_json::from_str(d).ok())
        .collect()
}

#[tokio::test]
async fn a_tool_decision_with_no_awaiting_run_yields_an_error_frame() {
    // RunResume-only input (an assistant tool decision, no new user turn) with nothing awaiting.
    let frames = frames(json!({
        "threadId": "t1",
        "messages": [{
            "id": "a1",
            "role": "assistant",
            "parts": [{
                "type": "tool-probe",
                "toolCallId": "c1",
                "state": "output-available",
                "output": "sneaky"
            }]
        }]
    }))
    .await;
    let types: Vec<&str> = frames
        .iter()
        .map(|f| f["type"].as_str().unwrap_or_default())
        .collect();
    assert!(
        types.contains(&"error"),
        "a stray tool decision must fail closed with an error frame: {types:?}"
    );
}

#[tokio::test]
async fn a_known_operation_replays_the_run_while_history_only_fails_closed() {
    // Router classification decision table. C1 the request's current operation
    // is a known user input; C2 a tool decision is present. Effects: E1 C1
    // forwards the same operation/message to the existing Run authority; E2
    // !C1+!C2 rejects without Run or resume; E3 !C1+C2 uses resume. Constraint:
    // committed-history dedup may remove only older history, never the current
    // operation whose durable equality check is downstream. Rules: R1=C1=>E1;
    // R2=!C1+!C2=>E2; R3=!C1+C2=>E3 (the adjacent resume tests own R3).
    let runtime = Arc::new(ReplayRecordingRuntime::default());
    let app = awaken_protocol_ai_sdk::router::router(runtime.clone(), "test-workspace".into());
    let replay = json!({
        "threadId": "t1",
        "messages": [{
            "id": "u1",
            "role": "user",
            "parts": [{ "type": "text", "text": "original" }]
        }]
    });
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/chat")
                .header("content-type", "application/json")
                .body(Body::from(replay.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(body.contains("\"type\":\"finish\""), "R1/E1: {body}");
    assert!(!body.contains("\"type\":\"error\""), "R1/E1: {body}");

    let history_only = json!({
        "threadId": "t1",
        "messages": [
            {
                "id": "u1",
                "role": "user",
                "parts": [{ "type": "text", "text": "original" }]
            },
            {
                "id": "a1",
                "role": "assistant",
                "parts": [{ "type": "text", "text": "already answered" }]
            }
        ]
    });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/chat")
                .header("content-type", "application/json")
                .body(Body::from(history_only.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let body = String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(body.contains("\"type\":\"error\""), "R2/E2: {body}");

    let runs = runtime.runs.lock().unwrap();
    assert_eq!(runs.len(), 1, "R1-R2 exactly one Run admission");
    assert_eq!(runs[0].0, "u1", "R1/E1 stable operation");
    assert_eq!(runs[0].1.len(), 1, "R1/E1 one operation input");
    assert_eq!(runs[0].1[0].id.0, "u1", "R1/E1 stable message");
    assert_eq!(blocks_text(&runs[0].1[0].content), "original", "R1/E1");
}

#[tokio::test]
async fn authenticated_workspace_scope_reaches_the_same_ai_sdk_run_admission() {
    // Cause/effect rules: A1 a guarded request has an exact WorkspaceScope and
    // a known user operation => forward both to the one Run port; A2 no scope
    // => existing unscoped protocol behavior (covered by the adjacent replay
    // test). The scope must not be inferred from the Thread id or body text.
    let runtime = Arc::new(ReplayRecordingRuntime::default());
    let app = awaken_protocol_ai_sdk::router::router_with_owner_scope(runtime.clone());
    let body = json!({
        "threadId": "t1",
        "messages": [{
            "id": "u1",
            "role": "user",
            "parts": [{ "type": "text", "text": "original" }]
        }]
    });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/threads/t1/runs")
                .header("content-type", "application/json")
                .extension(awaken_runtime_contract::runs::scope::WorkspaceScope(
                    "exact-workspace".into(),
                ))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let stream = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert!(String::from_utf8_lossy(&stream).contains("\"type\":\"finish\""));
    assert_eq!(
        runtime.owners.lock().unwrap().as_slice(),
        ["exact-workspace"]
    );
    assert_eq!(runtime.runs.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn guarded_router_without_workspace_scope_fails_closed() {
    // D1 hosted router + no authenticated WorkspaceScope => required extractor
    // rejects before Run; D2 local router => its composition supplies an
    // explicit Workspace, covered by the adjacent unscoped protocol tests.
    let runtime = Arc::new(ReplayRecordingRuntime::default());
    let app = awaken_protocol_ai_sdk::router::router_with_owner_scope(runtime.clone());
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/threads/t1/runs")
                .header("content-type", "application/json")
                .body(Body::from(user_turn().to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        !response.status().is_success(),
        "D1 missing scope must reject"
    );
    assert!(runtime.runs.lock().unwrap().is_empty(), "D1 no Run");
}

fn user_turn() -> serde_json::Value {
    json!({ "messages": [{ "id": "u1", "role": "user", "parts": [{ "type": "text", "text": "go" }] }] })
}

#[tokio::test]
async fn the_thread_scoped_run_route_streams_a_turn() {
    let frames = frames_at("/v1/ai-sdk/threads/t9/runs", user_turn()).await;
    assert!(
        frames.iter().any(|f| f["type"] == "finish"),
        "thread-scoped run should stream to a finish: {frames:?}"
    );
}

#[tokio::test]
async fn the_agent_scoped_run_route_streams_a_turn() {
    let frames = frames_at("/v1/ai-sdk/agents/coder/runs", user_turn()).await;
    assert!(
        frames.iter().any(|f| f["type"] == "finish"),
        "agent-scoped run should stream to a finish: {frames:?}"
    );
}

/// A runtime awaiting on a built-in tool `c1`; resume drives it to completion.
#[derive(Default)]
struct AwaitingRuntime {
    owners: Option<Arc<Mutex<Vec<String>>>>,
}

#[async_trait::async_trait]
impl Runs for AwaitingRuntime {
    async fn run(
        &self,
        _operation_id: &str,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, RunError> {
        unreachable!("this test only drives the resume path")
    }

    async fn resume(
        &self,
        _operation_id: &str,
        _thread: &str,
        _tool_use_id: &str,
        _resume: RunResume,
    ) -> Result<StepOutcome, RunError> {
        use awaken_agent_contract::agent::message::{Id, Role};
        Ok(StepOutcome::ended(
            vec![Message::text(Id("a1".into()), Role::Assistant, "done")],
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }

    async fn pending(&self, _thread: &str) -> Result<Option<Pending>, RunError> {
        Ok(Some(Pending {
            tool_use_id: "c1".into(),
            name: "write".into(),
            input: serde_json::Value::Null,
            client_executed: false,
        }))
    }

    async fn history(&self, _thread: &str) -> Result<Vec<Message>, RunError> {
        Ok(Vec::new())
    }

    fn model(&self) -> String {
        "test".into()
    }
}

#[async_trait::async_trait]
impl OwnedRuns for AwaitingRuntime {
    async fn run_streaming_for_owner(
        &self,
        _owner_scope: &str,
        _operation_id: &str,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
        _sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunError> {
        unreachable!("awaiting runtime never admits a new Run")
    }

    async fn resume_for_owner(
        &self,
        owner_scope: &str,
        operation_id: &str,
        thread: &str,
        tool_use_id: &str,
        resume: RunResume,
    ) -> Result<StepOutcome, RunError> {
        if let Some(owners) = &self.owners {
            owners.lock().unwrap().push(owner_scope.to_string());
        }
        self.resume(operation_id, thread, tool_use_id, resume).await
    }
}

#[tokio::test]
async fn a_matching_tool_decision_resumes_an_awaiting_run_to_a_finish() {
    let app = awaken_protocol_ai_sdk::router::router(
        Arc::new(AwaitingRuntime::default()),
        "test-workspace".into(),
    );
    let body = matching_tool_decision_body();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/chat")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let types = response_frame_types(response).await;
    assert!(types.contains(&"finish".to_string()), "{types:?}");
    assert!(!types.contains(&"error".to_string()), "{types:?}");
}

fn matching_tool_decision_body() -> Value {
    json!({
        "threadId": "t1",
        "messages": [{
            "id": "a1",
            "role": "assistant",
            "parts": [{ "type": "tool-write", "toolCallId": "c1", "state": "output-available", "output": "ok" }]
        }]
    })
}

async fn response_frame_types(response: axum::response::Response) -> Vec<String> {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    text.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .filter_map(|frame| frame["type"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn authenticated_workspace_scope_reaches_ai_sdk_tool_resume() {
    // Cause/effect rule S1: a guarded request carries exact WorkspaceScope
    // and one matching tool result => resume under that same owner; S2: a
    // missing/mismatched decision => the adjacent fail-closed tests reject
    // before resume. The adapter never caches a second owner for the Thread.
    let owners = Arc::new(Mutex::new(Vec::new()));
    let app = awaken_protocol_ai_sdk::router::router_with_owner_scope(Arc::new(AwaitingRuntime {
        owners: Some(owners.clone()),
    }));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/threads/t1/runs")
                .header("content-type", "application/json")
                .extension(awaken_runtime_contract::runs::scope::WorkspaceScope(
                    "exact-workspace".into(),
                ))
                .body(Body::from(matching_tool_decision_body().to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let types = response_frame_types(response).await;
    assert!(types.contains(&"finish".to_string()), "{types:?}");
    assert!(!types.contains(&"error".to_string()), "{types:?}");
    assert_eq!(owners.lock().unwrap().as_slice(), ["exact-workspace"]);
}

/// A runtime whose turn panics, so the spawned turn task dies (JoinError).
struct PanickingRuntime;

#[async_trait::async_trait]
impl Runs for PanickingRuntime {
    async fn run(
        &self,
        _operation_id: &str,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, RunError> {
        panic!("the turn task died");
    }
    async fn resume(
        &self,
        _operation_id: &str,
        _thread: &str,
        _tool_use_id: &str,
        _resume: RunResume,
    ) -> Result<StepOutcome, RunError> {
        unreachable!()
    }
    async fn pending(&self, _thread: &str) -> Result<Option<Pending>, RunError> {
        Ok(None)
    }
    async fn history(&self, _thread: &str) -> Result<Vec<Message>, RunError> {
        Ok(Vec::new())
    }
    fn model(&self) -> String {
        "test".into()
    }
}

#[tokio::test]
async fn a_dead_turn_task_surfaces_an_error_frame_not_a_hang() {
    // The turn runs on its own task; if it dies (panic/cancel → JoinError) the
    // stream must still close with an `error` frame rather than hanging.
    let app =
        awaken_protocol_ai_sdk::router::router(Arc::new(PanickingRuntime), "test-workspace".into());
    let body = json!({ "messages": [{ "id": "u1", "role": "user", "parts": [{ "type": "text", "text": "go" }] }] });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/chat")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let has_error = text
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .any(|f| f["type"] == "error");
    assert!(
        has_error,
        "a dead turn task must surface an error frame: {text}"
    );
}

/// A runtime awaiting on built-in tool `c2` that records every resume it is
/// given, so a test can prove a mis-correlated decision never reaches it.
struct AwaitingC2Runtime {
    resumes: Mutex<Vec<(String, RunResume)>>,
}

impl AwaitingC2Runtime {
    fn new() -> Self {
        Self {
            resumes: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl Runs for AwaitingC2Runtime {
    async fn run(
        &self,
        _operation_id: &str,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, RunError> {
        unreachable!("this test drives only the resume path")
    }

    async fn resume(
        &self,
        _operation_id: &str,
        _thread: &str,
        tool_use_id: &str,
        resume: RunResume,
    ) -> Result<StepOutcome, RunError> {
        self.resumes
            .lock()
            .unwrap()
            .push((tool_use_id.to_string(), resume));
        Ok(StepOutcome::ended(
            vec![Message::text(Id("a2".into()), Role::Assistant, "done")],
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }

    async fn pending(&self, _thread: &str) -> Result<Option<Pending>, RunError> {
        Ok(Some(Pending {
            tool_use_id: "c2".into(),
            name: "bash".into(),
            input: Value::Null,
            client_executed: false,
        }))
    }

    async fn history(&self, _thread: &str) -> Result<Vec<Message>, RunError> {
        Ok(Vec::new())
    }

    fn model(&self) -> String {
        "awaiting-c2".into()
    }
}

async fn decide(app: axum::Router, parts: Value) -> String {
    let body = json!({
        "threadId": "t1",
        "messages": [{ "id": "a1", "role": "assistant", "parts": parts }]
    });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/chat")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}

fn approval(call_id: &str, approved: bool) -> Value {
    json!({
        "type": "tool-bash",
        "toolCallId": call_id,
        "state": "approval-responded",
        "approval": { "id": call_id, "approved": approved }
    })
}

/// Resume correlation is identity, never position.
///
/// | awaiting | decisions posted        | resume called | stream terminal |
/// |----------|-------------------------|---------------|-----------------|
/// | c2       | c1 approved             | no            | error           |
/// | c2       | c1 approved, c2 denied  | yes, as Deny  | finish          |
///
/// Row 1 is the live hazard: a stale replay, a UI re-send, or the losing side
/// of a parallel-tool race posts only `c1`, and a positional fallback would
/// apply `c1`'s human **approval** to the different tool call `c2`. Row 2 shows
/// the identity match still wins when it is not first in the request, so the
/// fix is exact correlation and not "reject whenever more than one arrives".
#[tokio::test]
async fn an_approval_for_another_tool_call_cannot_resume_the_awaiting_one() {
    let runtime = Arc::new(AwaitingC2Runtime::new());
    let app = awaken_protocol_ai_sdk::router::router(runtime.clone(), "test-workspace".into());
    let text = decide(app, json!([approval("c1", true)])).await;

    assert!(
        text.contains("\"type\":\"error\""),
        "a decision for another tool call must fail closed: {text}"
    );
    assert!(
        !text.contains("\"type\":\"finish\"") || text.contains("\"finishReason\":\"error\""),
        "the stream must not report a clean finish: {text}"
    );
    assert!(
        runtime.resumes.lock().unwrap().is_empty(),
        "a mismatched decision must never reach runtime.resume: {:?}",
        runtime.resumes.lock().unwrap()
    );
}

#[tokio::test]
async fn the_matching_decision_wins_even_when_it_is_not_first() {
    let runtime = Arc::new(AwaitingC2Runtime::new());
    let app = awaken_protocol_ai_sdk::router::router(runtime.clone(), "test-workspace".into());
    let text = decide(app, json!([approval("c1", true), approval("c2", false)])).await;

    assert!(
        !text.contains("\"type\":\"error\""),
        "an exact decision for the awaiting tool must resume: {text}"
    );
    let resumes = runtime.resumes.lock().unwrap();
    assert_eq!(resumes.len(), 1, "exactly one resume: {resumes:?}");
    assert_eq!(resumes[0].0, "c2", "resumed the awaiting call");
    assert!(
        matches!(
            resumes[0].1,
            RunResume::Permission(
                awaken_agent_contract::agent::awaiting::PermissionDecision::Deny { .. }
            )
        ),
        "c2's own denial must be applied, not c1's approval: {:?}",
        resumes[0].1
    );
}
