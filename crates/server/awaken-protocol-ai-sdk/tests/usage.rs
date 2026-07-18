//! The AI SDK `finish` frame carries the run's token usage (`totalUsage`), read
//! from the runtime's `usage()` port after the turn commits.

use std::sync::Arc;

use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_protocol_transport::{
    DriverError, Pending, ProtocolRuntime, Resume, StepOutcome, Terminal,
};
use axum::body::{Body, to_bytes};
use axum::http::Request;
use serde_json::{Value, json};
use tower::ServiceExt;

struct UsageRuntime;

#[async_trait::async_trait]
impl ProtocolRuntime for UsageRuntime {
    async fn run(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, DriverError> {
        Ok(StepOutcome {
            new_messages: vec![Message::text(Id("a1".into()), Role::Assistant, "hi")],
            terminal: Terminal::Finished,
        })
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _resume: Resume,
    ) -> Result<StepOutcome, DriverError> {
        unreachable!()
    }

    async fn pending(&self, _thread: &str) -> Option<Pending> {
        None
    }

    async fn history(&self, _thread: &str) -> Vec<Message> {
        Vec::new()
    }

    fn model(&self) -> String {
        "test".into()
    }

    async fn usage(&self, _thread: &str) -> (u64, u64) {
        (10, 20)
    }
}

async fn frames(body: Value) -> Vec<Value> {
    let app = awaken_protocol_ai_sdk::router::router(Arc::new(UsageRuntime));
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
    String::from_utf8(bytes.to_vec())
        .unwrap()
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .filter_map(|d| serde_json::from_str(d).ok())
        .collect()
}

#[tokio::test]
async fn the_finish_frame_carries_total_usage() {
    let frames = frames(json!({
        "threadId": "t1",
        "messages": [{ "id": "u1", "role": "user", "parts": [{ "type": "text", "text": "go" }] }]
    }))
    .await;
    let finish = frames
        .iter()
        .find(|f| f["type"] == "finish")
        .expect("a finish frame");
    let usage = &finish["messageMetadata"]["totalUsage"];
    assert_eq!(usage["inputTokens"], 10, "{finish}");
    assert_eq!(usage["outputTokens"], 20, "{finish}");
    assert_eq!(usage["totalTokens"], 30, "{finish}");
}

/// A runtime awaiting on a client tool, so a decision-only (resume) request drives
/// the router's `resume_step` branch. `usage()` reports the whole thread's tally.
struct ResumeUsageRuntime;

#[async_trait::async_trait]
impl ProtocolRuntime for ResumeUsageRuntime {
    async fn run(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, DriverError> {
        unreachable!("a resume-only request never calls run")
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _resume: Resume,
    ) -> Result<StepOutcome, DriverError> {
        // The resumed run finishes cleanly after answering the awaiting tool.
        Ok(StepOutcome {
            new_messages: vec![Message::text(Id("a2".into()), Role::Assistant, "done")],
            terminal: Terminal::Finished,
        })
    }

    async fn pending(&self, _thread: &str) -> Option<Pending> {
        Some(Pending {
            tool_use_id: "c1".into(),
            name: "submit_answer".into(),
            input: serde_json::Value::Null,
            client_executed: true,
        })
    }

    async fn history(&self, _thread: &str) -> Vec<Message> {
        Vec::new()
    }

    fn model(&self) -> String {
        "resume".into()
    }

    async fn usage(&self, _thread: &str) -> (u64, u64) {
        (7, 11)
    }
}

/// The resume path (`ProcessedRequest::is_resume_only`) frames the committed step
/// whole via `encode_step`, then `attach_usage` reads `usage()` — the same usage
/// slot the streamed turn fills. Only the fresh-turn path was covered; this pins
/// that a decision-only resume also carries `totalUsage` on its finish frame.
#[tokio::test]
async fn the_resume_path_also_attaches_total_usage() {
    let app = awaken_protocol_ai_sdk::router::router(Arc::new(ResumeUsageRuntime));
    // A decision-only body: the assistant `tool-*` part answers the awaiting `c1`,
    // and no new user/system message → `is_resume_only()` → the resume branch.
    let body = json!({
        "threadId": "t1",
        "messages": [{
            "role": "assistant",
            "parts": [{
                "type": "tool-submit_answer",
                "toolCallId": "c1",
                "state": "output-available",
                "output": "42",
            }],
        }],
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
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let frames: Vec<Value> = String::from_utf8(bytes.to_vec())
        .unwrap()
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .filter_map(|d| serde_json::from_str(d).ok())
        .collect();
    let finish = frames
        .iter()
        .find(|f| f["type"] == "finish")
        .expect("a finish frame on the resume path");
    let usage = &finish["messageMetadata"]["totalUsage"];
    assert_eq!(usage["inputTokens"], 7, "{finish}");
    assert_eq!(usage["outputTokens"], 11, "{finish}");
    assert_eq!(usage["totalTokens"], 18, "{finish}");
}
