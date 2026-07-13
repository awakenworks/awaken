//! The AG-UI message-history endpoint over the real router: a client that lost
//! its in-band state rehydrates a thread's committed messages from the server, and
//! walks them by cursor. Drives `GET /v1/ag-ui/threads/{id}/messages` against a
//! runtime that persists a fixed transcript.

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_protocol_transport::{DriverError, Pending, ProtocolRuntime, Resume, StepOutcome};
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

/// A runtime whose `history` returns a fixed, server-persisted transcript for the
/// one known thread, and an empty list for anything else — the read model the
/// history endpoint projects.
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

async fn get(rt: Arc<PersistedRuntime>, uri: &str) -> (StatusCode, Value) {
    let app = awaken_protocol_ag_ui::router::router(rt);
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

fn runtime(messages: Vec<Message>) -> Arc<PersistedRuntime> {
    Arc::new(PersistedRuntime {
        thread: "t-hist".into(),
        messages,
    })
}

#[tokio::test]
async fn returns_the_persisted_transcript_in_ag_ui_message_shape() {
    let rt = runtime(vec![
        Message::text(Id("u0".into()), Role::User, "hi"),
        Message {
            id: Id("a0".into()),
            role: Role::Assistant,
            content: vec![
                ContentBlock::text("on it"),
                ContentBlock::ToolUse {
                    id: "c1".into(),
                    name: "read".into(),
                    input: serde_json::json!({ "path": "x" }),
                },
            ],
        },
        Message {
            id: Id("t0".into()),
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "c1".into(),
                content: vec![ContentBlock::text("42")],
            }],
        },
    ]);
    let (status, body) = get(rt, "/v1/ag-ui/threads/t-hist/messages").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["cursor"], Value::Null);
    let msgs = body["items"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0], serde_json::json!({ "id": "u0", "role": "user", "content": "hi" }));
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[1]["toolCalls"][0]["function"]["name"], "read");
    assert_eq!(
        msgs[2],
        serde_json::json!({ "id": "t0", "role": "tool", "content": "42", "toolCallId": "c1" })
    );
}

#[tokio::test]
async fn walks_the_thread_by_cursor() {
    // 3 user/assistant rounds = 6 messages: u0 a0 u1 a1 u2 a2.
    let rt = runtime(transcript(3));

    // First page of 2, more remain, cursor points at the 2nd message.
    let (status, page1) = get(Arc::clone(&rt), "/v1/ag-ui/threads/t-hist/messages?size=2").await;
    assert_eq!(status, StatusCode::OK);
    let ids1: Vec<&str> = page1["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids1, vec!["u0", "a0"]);
    assert_eq!(page1["cursor"], "a0");

    // Resume after the cursor.
    let (_, page2) = get(
        Arc::clone(&rt),
        "/v1/ag-ui/threads/t-hist/messages?size=2&cursor=a0",
    )
    .await;
    let ids2: Vec<&str> = page2["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids2, vec!["u1", "a1"]);
    assert_eq!(page2["cursor"], "a1");

    // Final page: no more, no cursor.
    let (_, page3) = get(
        Arc::clone(&rt),
        "/v1/ag-ui/threads/t-hist/messages?size=2&cursor=a1",
    )
    .await;
    let ids3: Vec<&str> = page3["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids3, vec!["u2", "a2"]);
    assert_eq!(page3["cursor"], Value::Null);
}

#[tokio::test]
async fn an_unknown_thread_is_an_empty_page_not_an_error() {
    let rt = runtime(transcript(1));
    let (status, body) = get(rt, "/v1/ag-ui/threads/does-not-exist/messages").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["items"].as_array().unwrap().is_empty());
    assert_eq!(body["cursor"], Value::Null);
}

#[tokio::test]
async fn a_fabricated_cursor_is_a_400() {
    let rt = runtime(transcript(1));
    let (status, _) = get(rt, "/v1/ag-ui/threads/t-hist/messages?cursor=nope").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
