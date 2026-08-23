//! Fail-closed resume over the real AG-UI router: a tool result delivered when no
//! run is awaiting must produce a RUN_ERROR, not a silent success.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use awaken_agent_contract::agent::awaiting::PermissionDecision;
use awaken_agent_contract::agent::message::Message;
use awaken_session_contract::{
    Pending, RunApplication, RunApplicationError, RunResume, StepOutcome,
};
use axum::body::{Body, to_bytes};
use axum::http::Request;
use serde_json::{Value, json};
use tower::ServiceExt;

struct NoAwaitingRuntime;

#[async_trait::async_trait]
impl RunApplication for NoAwaitingRuntime {
    async fn run(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        Ok(StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _resume: RunResume,
    ) -> Result<StepOutcome, RunApplicationError> {
        unreachable!("no run is awaiting, so resume must never be reached")
    }

    // No run is ever awaiting.
    async fn pending(&self, _thread: &str) -> Result<Option<Pending>, RunApplicationError> {
        Ok(None)
    }

    async fn history(&self, _thread: &str) -> Result<Vec<Message>, RunApplicationError> {
        Ok(Vec::new())
    }

    fn model(&self) -> String {
        "test".into()
    }
}

async fn post_frames(
    runtime: Arc<dyn RunApplication>,
    path: &str,
    body: impl Into<String>,
) -> Vec<Value> {
    post_frames_with_content_type(runtime, path, body, "application/json").await
}

async fn post_frames_with_content_type(
    runtime: Arc<dyn RunApplication>,
    path: &str,
    body: impl Into<String>,
    content_type: &str,
) -> Vec<Value> {
    let response = awaken_protocol_ag_ui::router::router(runtime)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", content_type)
                .body(Body::from(body.into()))
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    text.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .map(|d| serde_json::from_str(d).unwrap())
        .collect()
}

#[tokio::test]
async fn request_decode_failures_are_single_structured_sse_errors() {
    // AG-UI request-admission cause/effect table: C1 JSON syntax is valid; C2
    // typed fields match the schema; C3 Content-Type is JSON. Effects: E1 input
    // reaches run admission; E2 one unbracketed RUN_ERROR SSE is returned; E3 no
    // RUN_STARTED/RUN_FINISHED and no submitted body value is reflected.
    // R1 C1+C2+C3 -> E1 (covered by successful router tests); R2 !C1 -> E2+E3;
    // R3 C1+!C2+C3 -> E2+E3; R4 C1+C2+!C3 -> E2+E3. Decode rejection has no
    // Runtime classification, so the optional code is absent by contract.
    for (rule, content_type, body) in [
        ("R2", "application/json", r#"{"threadId":"secret-marker""#),
        ("R3", "application/json", r#"{"threadId":7,"messages":[]}"#),
        (
            "R4",
            "text/plain",
            r#"{"threadId":"secret-marker","messages":[]}"#,
        ),
    ] {
        let frames = post_frames_with_content_type(
            Arc::new(NoAwaitingRuntime),
            "/v1/ag-ui",
            body,
            content_type,
        )
        .await;
        assert_eq!(
            frame_types(&frames),
            vec!["RUN_ERROR"],
            "{rule}: {frames:?}"
        );
        assert!(
            frames[0]["message"].as_str().is_some_and(|m| !m.is_empty()),
            "{rule}"
        );
        assert!(frames[0].get("code").is_none(), "{rule}: {frames:?}");
        assert!(
            !serde_json::to_string(&frames)
                .unwrap()
                .contains("secret-marker"),
            "{rule}/E3"
        );
    }
}

async fn frames(body: Value) -> Vec<Value> {
    post_frames(Arc::new(NoAwaitingRuntime), "/v1/ag-ui", body.to_string()).await
}

fn frame_types(frames: &[Value]) -> Vec<String> {
    frames
        .iter()
        .filter_map(|frame| frame["type"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn a_tool_result_with_no_awaiting_run_fails_closed_with_run_error() {
    // Cause/effect graph: C1 resume-only input; C2 no run awaits; C3 neutral
    // validation error is classified `invalid_request`. Effects: E1 runtime
    // resume is not called; E2 RUN_ERROR, never RUN_FINISHED; E3 exact code and
    // message survive projection. Decision rule R1 C1&&C2&&C3 => E1&&E2&&E3.
    let frames = frames(json!({
        "threadId": "t1",
        "runId": "r1",
        "messages": [{ "role": "tool", "toolCallId": "c1", "content": "sneaky result" }],
    }))
    .await;
    let types: Vec<&str> = frames
        .iter()
        .map(|f| f["type"].as_str().unwrap_or_default())
        .collect();
    assert!(
        types.contains(&"RUN_ERROR"),
        "a stray tool result must fail closed: {types:?}"
    );
    assert!(
        !types.contains(&"RUN_FINISHED"),
        "a fail-closed resume must not report a finished run: {types:?}"
    );
    let error = frames
        .iter()
        .find(|frame| frame["type"] == "RUN_ERROR")
        .unwrap();
    assert_eq!(error["code"], "invalid_request");
    assert_eq!(error["message"], "no awaiting run to resume");
}

/// A runtime awaiting on the built-in tool `c1`; resume drives it to completion.
struct AwaitingRuntime;

#[async_trait::async_trait]
impl RunApplication for AwaitingRuntime {
    async fn run(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        unreachable!("this test only drives the resume path")
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _resume: RunResume,
    ) -> Result<StepOutcome, RunApplicationError> {
        use awaken_agent_contract::agent::message::{Id, Role};
        Ok(StepOutcome::ended(
            vec![Message::text(Id("a1".into()), Role::Assistant, "done")],
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }

    async fn pending(&self, _thread: &str) -> Result<Option<Pending>, RunApplicationError> {
        Ok(Some(Pending {
            tool_use_id: "c1".into(),
            name: "write".into(),
            input: Value::Null,
            client_executed: false,
        }))
    }

    async fn history(&self, _thread: &str) -> Result<Vec<Message>, RunApplicationError> {
        Ok(Vec::new())
    }

    fn model(&self) -> String {
        "test".into()
    }
}

#[tokio::test]
async fn a_matching_tool_result_resumes_an_awaiting_run_to_completion() {
    let body = json!({
        "threadId": "t1",
        "runId": "r1",
        "messages": [{ "role": "tool", "toolCallId": "c1", "content": "approved" }],
    });
    let frames = post_frames(Arc::new(AwaitingRuntime), "/v1/ag-ui", body.to_string()).await;
    let types = frame_types(&frames);
    assert!(types.contains(&"RUN_FINISHED".to_string()), "{types:?}");
    assert!(!types.contains(&"RUN_ERROR".to_string()), "{types:?}");
}

/// RunResume correlation behavior:
///
/// ```text
/// awaiting(c1) + result(c1) -> resume(c1) -> RUN_FINISHED
/// awaiting(c1) + result(c2) -> reject     -> RUN_ERROR
///                                      \-> runtime.resume is never called
/// ```
///
/// | awaiting id | supplied ids | resume called | terminal event |
/// |--------------|--------------|---------------|----------------|
/// | c1           | c1           | yes           | RUN_FINISHED   |
/// | c1           | c2           | no            | RUN_ERROR      |
///
/// The second row prevents a different tool result from being substituted merely
/// because it is first in the request. Identity is behavior, not DTO shape.
struct ExactResumeRuntime {
    resumed: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl RunApplication for ExactResumeRuntime {
    async fn run(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        unreachable!("this test drives only resume")
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _resume: RunResume,
    ) -> Result<StepOutcome, RunApplicationError> {
        self.resumed.store(true, Ordering::SeqCst);
        Ok(StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }

    async fn pending(&self, _thread: &str) -> Result<Option<Pending>, RunApplicationError> {
        Ok(Some(Pending {
            tool_use_id: "c1".into(),
            name: "write".into(),
            input: Value::Null,
            client_executed: false,
        }))
    }

    async fn history(&self, _thread: &str) -> Result<Vec<Message>, RunApplicationError> {
        Ok(Vec::new())
    }

    fn model(&self) -> String {
        "test".into()
    }
}

#[tokio::test]
async fn a_non_matching_tool_result_cannot_resume_the_awaiting_tool() {
    let resumed = Arc::new(AtomicBool::new(false));
    let frames = post_frames(
        Arc::new(ExactResumeRuntime {
            resumed: resumed.clone(),
        }),
        "/v1/ag-ui",
        json!({
            "threadId": "t1",
            "runId": "r1",
            "messages": [
                { "role": "tool", "toolCallId": "c2", "content": "wrong result" }
            ]
        })
        .to_string(),
    )
    .await;
    let types = frame_types(&frames);
    assert!(types.contains(&"RUN_ERROR".to_string()), "{frames:?}");
    assert!(!types.contains(&"RUN_FINISHED".to_string()), "{frames:?}");
    assert!(
        !resumed.load(Ordering::SeqCst),
        "a mismatched result must not invoke runtime.resume"
    );
}

/// Unsupported-input admission behavior:
///
/// ```text
/// supported text/image + no unsupported run extension -> runtime.run -> terminal
/// unsupported media                                  -> RUN_ERROR  -> no runtime call
/// non-empty unimplemented run extension              -> RUN_ERROR  -> no runtime call
/// ```
///
/// | content | tools | state/props/resume | runtime called | result    |
/// |---------|-------|--------------------|----------------|-----------|
/// | text    | empty | empty              | yes            | terminal  |
/// | audio   | empty | empty              | no             | RUN_ERROR |
/// | text    | set   | empty              | no             | RUN_ERROR |
/// | text    | empty | context set        | no             | RUN_ERROR |
///
/// This deliberately tests effects. Parsing an official SDK shape must never be
/// mistaken for support when the runtime cannot honor its semantics.
struct AdmissionRecordingRuntime {
    ran: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl RunApplication for AdmissionRecordingRuntime {
    async fn run(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        self.ran.store(true, Ordering::SeqCst);
        Ok(StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _resume: RunResume,
    ) -> Result<StepOutcome, RunApplicationError> {
        unreachable!("unsupported fresh input must fail before resume")
    }

    async fn pending(&self, _thread: &str) -> Result<Option<Pending>, RunApplicationError> {
        Ok(None)
    }

    async fn history(&self, _thread: &str) -> Result<Vec<Message>, RunApplicationError> {
        Ok(Vec::new())
    }

    fn model(&self) -> String {
        "test".into()
    }
}

async fn assert_admission_rejected_without_run(body: Value) {
    let ran = Arc::new(AtomicBool::new(false));
    let frames = post_frames(
        Arc::new(AdmissionRecordingRuntime { ran: ran.clone() }),
        "/v1/ag-ui",
        body.to_string(),
    )
    .await;
    let types = frame_types(&frames);
    assert!(types.contains(&"RUN_ERROR".to_string()), "{frames:?}");
    assert!(!types.contains(&"RUN_FINISHED".to_string()), "{frames:?}");
    assert!(
        !ran.load(Ordering::SeqCst),
        "rejected input must not invoke runtime.run"
    );
}

#[tokio::test]
async fn unsupported_audio_fails_before_runtime_execution() {
    assert_admission_rejected_without_run(json!({
        "threadId": "t1",
        "runId": "r1",
        "messages": [{
            "role": "user",
            "content": [{
                "type": "audio",
                "source": { "type": "url", "value": "https://example.test/a.wav" }
            }]
        }]
    }))
    .await;
}

#[tokio::test]
async fn unimplemented_per_run_tools_fail_before_runtime_execution() {
    assert_admission_rejected_without_run(json!({
        "threadId": "t1",
        "runId": "r1",
        "messages": [{ "role": "user", "content": "go" }],
        "tools": [{
            "name": "lookup",
            "description": "Look up a value",
            "parameters": { "type": "object" }
        }]
    }))
    .await;
}

#[tokio::test]
async fn unimplemented_per_run_context_fails_before_runtime_execution() {
    assert_admission_rejected_without_run(json!({
        "threadId": "t1",
        "runId": "r1",
        "messages": [{ "role": "user", "content": "go" }],
        "context": [{ "description": "tenant", "value": "acme" }]
    }))
    .await;
}

#[tokio::test]
async fn the_scoped_agent_route_streams_a_fresh_turn() {
    let body = json!({ "threadId": "t1", "runId": "r1", "messages": [{ "role": "user", "content": "go" }] });
    let frames = post_frames(
        Arc::new(NoAwaitingRuntime),
        "/v1/ag-ui/agents/coder",
        body.to_string(),
    )
    .await;
    let types = frame_types(&frames);
    assert!(types.contains(&"RUN_FINISHED".to_string()), "{types:?}");
}

/// A runtime awaiting on a built-in tool that records whether it was resumed denied.
struct DenyRecordingRuntime {
    denied: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl RunApplication for DenyRecordingRuntime {
    async fn run(
        &self,
        _t: &str,
        _a: Option<String>,
        _m: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        unreachable!()
    }
    async fn resume(
        &self,
        _t: &str,
        _id: &str,
        resume: RunResume,
    ) -> Result<StepOutcome, RunApplicationError> {
        if let RunResume::Permission(PermissionDecision::Deny { .. }) = resume {
            self.denied.store(true, Ordering::SeqCst);
        }
        Ok(StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }
    async fn pending(&self, _t: &str) -> Result<Option<Pending>, RunApplicationError> {
        Ok(Some(Pending {
            tool_use_id: "c1".into(),
            name: "write".into(),
            input: Value::Null,
            client_executed: false,
        }))
    }
    async fn history(&self, _t: &str) -> Result<Vec<Message>, RunApplicationError> {
        Ok(Vec::new())
    }
    fn model(&self) -> String {
        "test".into()
    }
}

#[tokio::test]
async fn an_error_flagged_tool_result_denies_an_awaiting_builtin_tool() {
    let denied = Arc::new(AtomicBool::new(false));
    let body = json!({
        "threadId": "t1",
        "runId": "r1",
        "messages": [{ "role": "tool", "toolCallId": "c1", "content": "", "error": "not permitted" }],
    });
    let _ = post_frames(
        Arc::new(DenyRecordingRuntime {
            denied: denied.clone(),
        }),
        "/v1/ag-ui",
        body.to_string(),
    )
    .await;
    assert!(
        denied.load(Ordering::SeqCst),
        "an error-flagged tool result must deny the awaiting built-in tool (allow:false)"
    );
}

struct ClassifiedFailureRuntime;

#[async_trait::async_trait]
impl RunApplication for ClassifiedFailureRuntime {
    async fn run(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        Err(RunApplicationError::classified(
            "provider_overloaded",
            "try later",
        ))
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _resume: RunResume,
    ) -> Result<StepOutcome, RunApplicationError> {
        unreachable!()
    }

    async fn pending(&self, _thread: &str) -> Result<Option<Pending>, RunApplicationError> {
        Ok(None)
    }

    async fn history(&self, _thread: &str) -> Result<Vec<Message>, RunApplicationError> {
        Ok(Vec::new())
    }

    fn model(&self) -> String {
        "test".into()
    }
}

#[tokio::test]
async fn classified_stream_failure_preserves_code_separately_from_message() {
    // Cause/effect graph: C1 runtime failure has a stable code; C2 failure occurs
    // before any live event. E1 RUN_STARTED brackets the attempt; E2 RUN_ERROR
    // carries the exact code; E3 message is unchanged (not code-prefixed).
    // Decision rule R1 C1&&C2 => E1&&E2&&E3. The complementary unclassified
    // decode failure is covered by the wire-shape unit rule and omits `code`.
    let frames = post_frames(
        Arc::new(ClassifiedFailureRuntime),
        "/v1/ag-ui",
        json!({
            "threadId": "t1",
            "runId": "r1",
            "messages": [{ "role": "user", "content": "go" }]
        })
        .to_string(),
    )
    .await;
    assert_eq!(frame_types(&frames), vec!["RUN_STARTED", "RUN_ERROR"]);
    let error = &frames[1];
    assert_eq!(error["code"], "provider_overloaded");
    assert_eq!(error["message"], "try later");
}

/// A runtime whose turn panics, so the spawned turn task dies (JoinError).
struct AgUiPanickingRuntime;

#[async_trait::async_trait]
impl RunApplication for AgUiPanickingRuntime {
    async fn run(
        &self,
        _t: &str,
        _a: Option<String>,
        _m: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        panic!("the turn task died");
    }
    async fn resume(
        &self,
        _t: &str,
        _id: &str,
        _r: RunResume,
    ) -> Result<StepOutcome, RunApplicationError> {
        unreachable!()
    }
    async fn pending(&self, _t: &str) -> Result<Option<Pending>, RunApplicationError> {
        Ok(None)
    }
    async fn history(&self, _t: &str) -> Result<Vec<Message>, RunApplicationError> {
        Ok(Vec::new())
    }
    fn model(&self) -> String {
        "test".into()
    }
}

async fn ag_ui_frame_types(runtime: Arc<dyn RunApplication>, body: String) -> Vec<String> {
    frame_types(&post_frames(runtime, "/v1/ag-ui", body).await)
}

#[tokio::test]
async fn a_dead_turn_task_surfaces_a_run_error_not_a_hang() {
    let body = json!({ "threadId": "t1", "runId": "r1", "messages": [{ "role": "user", "content": "go" }] });
    let frames = post_frames(
        Arc::new(AgUiPanickingRuntime),
        "/v1/ag-ui",
        body.to_string(),
    )
    .await;
    assert!(
        frame_types(&frames).contains(&"RUN_ERROR".to_string()),
        "a dead turn task must surface RUN_ERROR: {frames:?}"
    );
}

#[tokio::test]
async fn an_oversized_body_is_refused_in_stream() {
    // AG-UI surfaces the request-body-limit rejection as an in-stream RUN_ERROR
    // (its streaming errors are events, not HTTP status) — the body is bounded, and
    // the run does not complete.
    let big = "x".repeat(3 * 1024 * 1024);
    let body = format!(r#"{{"threadId":"t1","messages":[{{"role":"user","content":"{big}"}}]}}"#);
    let types = ag_ui_frame_types(Arc::new(NoAwaitingRuntime), body).await;
    assert!(types.contains(&"RUN_ERROR".to_string()), "{types:?}");
    assert!(!types.contains(&"RUN_FINISHED".to_string()), "{types:?}");
}
