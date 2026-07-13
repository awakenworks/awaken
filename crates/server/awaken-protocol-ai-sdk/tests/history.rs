//! The AI SDK message-history endpoint over the real router: a `useChat` client
//! hydrates `initialMessages` from the server-persisted transcript and walks it by
//! cursor. Drives `GET /v1/ai-sdk/threads/{id}/messages`.

use std::sync::Arc;

use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_protocol_transport::{DriverError, Pending, ProtocolRuntime, Resume, StepOutcome};
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

struct PersistedRuntime {
    thread: String,
    messages: Vec<Message>,
}

#[async_trait::async_trait]
impl ProtocolRuntime for PersistedRuntime {
    async fn run(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, DriverError> {
        unreachable!("history tests never drive a run")
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _resume: Resume,
    ) -> Result<StepOutcome, DriverError> {
        unreachable!("history tests never resume")
    }

    async fn pending(&self, _thread: &str) -> Option<Pending> {
        None
    }

    async fn history(&self, thread: &str) -> Vec<Message> {
        if thread == self.thread {
            self.messages.clone()
        } else {
            Vec::new()
        }
    }

    fn model(&self) -> String {
        "test".into()
    }
}

fn transcript(n: usize) -> Vec<Message> {
    let mut messages = Vec::new();
    for i in 0..n {
        messages.push(Message::text(Id(format!("u{i}")), Role::User, format!("ask {i}")));
        messages.push(Message::text(
            Id(format!("a{i}")),
            Role::Assistant,
            format!("answer {i}"),
        ));
    }
    messages
}

fn runtime(messages: Vec<Message>) -> Arc<PersistedRuntime> {
    Arc::new(PersistedRuntime {
        thread: "t-hist".into(),
        messages,
    })
}

async fn get(rt: Arc<PersistedRuntime>, uri: &str) -> (StatusCode, Value) {
    let app = awaken_protocol_ai_sdk::router::router(rt);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

#[tokio::test]
async fn returns_the_persisted_transcript_as_ui_messages() {
    let rt = runtime(transcript(1));
    let (status, body) = get(rt, "/v1/ai-sdk/threads/t-hist/messages").await;
    assert_eq!(status, StatusCode::OK);
    // House cursor-page envelope: { items, cursor }. `cursor` null = last page.
    assert_eq!(body["cursor"], Value::Null);
    let msgs = body["items"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    // AI SDK UIMessage shape: { id, role, parts:[{type:"text",text}] }.
    assert_eq!(msgs[0]["id"], "u0");
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[0]["parts"][0]["text"], "ask 0");
}

#[tokio::test]
async fn walks_the_thread_by_cursor() {
    let rt = runtime(transcript(3)); // u0 a0 u1 a1 u2 a2

    let (_, page1) = get(Arc::clone(&rt), "/v1/ai-sdk/threads/t-hist/messages?size=2").await;
    let ids1: Vec<&str> = page1["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids1, vec!["u0", "a0"]);
    assert_eq!(page1["cursor"], "a0");

    let (_, page2) = get(
        Arc::clone(&rt),
        "/v1/ai-sdk/threads/t-hist/messages?size=2&cursor=a0",
    )
    .await;
    let ids2: Vec<&str> = page2["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids2, vec!["u1", "a1"]);
}

#[tokio::test]
async fn a_fabricated_cursor_is_a_400() {
    let rt = runtime(transcript(1));
    let (status, _) = get(rt, "/v1/ai-sdk/threads/t-hist/messages?cursor=nope").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
