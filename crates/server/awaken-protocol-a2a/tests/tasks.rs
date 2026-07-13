//! A2A `tasks/get` and `tasks/cancel` over the real router with a runtime that is
//! parked on a built-in tool. `tasks/cancel` is A2A's protocol-native "deny": it
//! unblocks the parked tool with `allow: false` and reports the task `canceled`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;
use awaken_protocol_a2a::router;
use awaken_protocol_transport::{DriverError, Pending, ProtocolRuntime, Resume, StepOutcome};
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

/// A runtime perpetually parked on a built-in tool `c1`, recording whether it was
/// resumed with a denial.
struct ParkedRuntime {
    denied: Arc<AtomicBool>,
}

#[async_trait]
impl ProtocolRuntime for ParkedRuntime {
    async fn run(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, DriverError> {
        Ok(StepOutcome {
            new_messages: Vec::new(),
            waiting: true,
            exhausted: false,
            pending: Some(pending()),
        })
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        resume: Resume,
    ) -> Result<StepOutcome, DriverError> {
        if let Resume::Confirm { allow: false, .. } = resume {
            self.denied.store(true, Ordering::SeqCst);
        }
        Ok(StepOutcome {
            new_messages: Vec::new(),
            waiting: false,
            exhausted: false,
            pending: None,
        })
    }

    async fn pending(&self, _thread: &str) -> Option<Pending> {
        Some(pending())
    }

    async fn history(&self, _thread: &str) -> Vec<Message> {
        Vec::new()
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

async fn rpc(denied: Arc<AtomicBool>, body: Value) -> Value {
    let app = router(Arc::new(ParkedRuntime { denied }));
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

#[tokio::test]
async fn tasks_get_on_a_parked_run_reads_input_required() {
    let r = rpc(
        Arc::new(AtomicBool::new(false)),
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tasks/get", "params": { "id": "task-ctx" } }),
    )
    .await;
    assert_eq!(r["result"]["status"]["state"], "input-required", "{r}");
}

#[tokio::test]
async fn message_send_on_a_parked_context_resumes_it_rather_than_starting_a_fresh_turn() {
    // The context is parked, so the send takes the resume branch (pending →
    // resume, which completes) rather than starting a fresh (still-parked) turn.
    let r = rpc(
        Arc::new(AtomicBool::new(false)),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "message/send",
            "params": { "message": { "messageId": "m1", "contextId": "ctx", "role": "user", "parts": [{ "kind": "text", "text": "the answer" }] } }
        }),
    )
    .await;
    assert_eq!(r["result"]["status"]["state"], "completed", "{r}");
}

#[tokio::test]
async fn tasks_cancel_denies_the_parked_tool_and_reports_canceled() {
    let denied = Arc::new(AtomicBool::new(false));
    let r = rpc(
        denied.clone(),
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tasks/cancel", "params": { "id": "task-ctx" } }),
    )
    .await;
    assert_eq!(r["result"]["status"]["state"], "canceled", "{r}");
    assert!(
        denied.load(Ordering::SeqCst),
        "cancel must deny the parked tool (resume with allow:false)"
    );
}
