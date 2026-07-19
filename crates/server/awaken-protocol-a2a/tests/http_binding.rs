//! The A2A HTTP+JSON binding (`message:send`, scoped `message:send`) and the served
//! agent card, over the real router — the binding the outbound client speaks but
//! which had no wire-level test.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;
use awaken_protocol_a2a::router;
use awaken_protocol_transport::{
    DriverError, Pending, ProtocolRuntime, Resume, StepOutcome, Terminal,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

struct NoopRuntime;

#[async_trait]
impl ProtocolRuntime for NoopRuntime {
    async fn run(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, DriverError> {
        Ok(StepOutcome {
            new_messages: vec![Message::text(
                awaken_agent_contract::agent::message::Id("a1".into()),
                awaken_agent_contract::agent::message::Role::Assistant,
                "done",
            )],
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
}

async fn send(method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    let app = router(Arc::new(NoopRuntime));
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "a2a.test");
    if method == "POST" {
        builder = builder.header("content-type", "application/json");
    }
    let resp = app
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn message(context: &str) -> Value {
    json!({
        "message": {
            "messageId": "m1",
            "contextId": context,
            "role": "user",
            "parts": [{ "kind": "text", "text": "hi" }]
        }
    })
}

#[tokio::test]
async fn http_json_message_send_returns_a_task() {
    let (status, body) = send("POST", "/v1/a2a/message:send", message("c1")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["task"]["status"]["state"], "completed", "{body}");
    assert_eq!(body["task"]["contextId"], "c1");
}

#[tokio::test]
async fn scoped_http_json_message_send_returns_a_task() {
    let (status, body) = send("POST", "/v1/a2a/agents/coder/message:send", message("c2")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["task"]["status"]["state"], "completed", "{body}");
}

#[tokio::test]
async fn agent_card_advertises_url_and_jsonrpc_transport() {
    let (status, card) = send("GET", "/v1/a2a/agent-card", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    // The url is derived from the Host header.
    assert!(
        card["url"]
            .as_str()
            .unwrap_or_default()
            .contains("a2a.test"),
        "card url should reflect the host: {card}"
    );
    assert!(card["capabilities"].is_object(), "{card}");
}

#[tokio::test]
async fn agent_card_advertises_transport_protocol_and_skills() {
    let (status, card) = send("GET", "/v1/a2a/agent-card", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(card["preferredTransport"], "JSONRPC", "{card}");
    assert_eq!(card["protocolVersion"], "0.3.0");
    assert_eq!(card["capabilities"]["streaming"], true);
    assert_eq!(card["capabilities"]["pushNotifications"], true);
    assert!(
        !card["skills"].as_array().unwrap().is_empty(),
        "the card advertises at least one skill: {card}"
    );
}

/// A runtime whose turn always faults internally.
struct FailingRuntime;

#[async_trait]
impl ProtocolRuntime for FailingRuntime {
    async fn run(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, DriverError> {
        Err(DriverError::Internal("boom".into()))
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
}

#[tokio::test]
async fn an_internal_runtime_fault_maps_to_a_500_with_the_a2a_error_envelope() {
    let app = router(Arc::new(FailingRuntime));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/a2a/message:send")
                .header("host", "a2a.test")
                .header("content-type", "application/json")
                .body(Body::from(message("c1").to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"]["code"], -32603, "{body}");
}
