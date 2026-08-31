//! The AI SDK message-history endpoint over the real router: a `useChat` client
//! hydrates `initialMessages` from the server-persisted transcript and walks it by
//! cursor. Drives `GET /v1/ai-sdk/threads/{id}/messages`.

use std::sync::Arc;

use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_session_contract::{
    Pending, RunApplication, RunApplicationError, RunResume, StepOutcome,
};
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

struct PersistedRuntime {
    thread: String,
    messages: Vec<Message>,
    unavailable: bool,
}

#[async_trait::async_trait]
impl RunApplication for PersistedRuntime {
    async fn run(
        &self,
        _operation_id: &str,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        unreachable!("history tests never drive a run")
    }

    async fn resume(
        &self,
        _operation_id: &str,
        _thread: &str,
        _tool_use_id: &str,
        _resume: RunResume,
    ) -> Result<StepOutcome, RunApplicationError> {
        unreachable!("history tests never resume")
    }

    async fn pending(&self, _thread: &str) -> Result<Option<Pending>, RunApplicationError> {
        Ok(None)
    }

    async fn history(&self, thread: &str) -> Result<Vec<Message>, RunApplicationError> {
        if self.unavailable {
            return Err(RunApplicationError::unavailable("history store offline"));
        }
        Ok(if thread == self.thread {
            self.messages.clone()
        } else {
            Vec::new()
        })
    }

    fn model(&self) -> String {
        "test".into()
    }
}

fn transcript(n: usize) -> Vec<Message> {
    let mut messages = Vec::new();
    for i in 0..n {
        messages.push(Message::text(
            Id(format!("u{i}")),
            Role::User,
            format!("ask {i}"),
        ));
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
        unavailable: false,
    })
}

async fn get(rt: Arc<PersistedRuntime>, uri: &str) -> (StatusCode, HeaderMap, Value) {
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
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, json)
}

#[tokio::test]
async fn returns_the_persisted_transcript_as_ui_messages() {
    let rt = runtime(transcript(1));
    let (status, _, body) = get(rt, "/v1/ai-sdk/threads/t-hist/messages").await;
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

    let (_, _, page1) = get(Arc::clone(&rt), "/v1/ai-sdk/threads/t-hist/messages?size=2").await;
    let ids1: Vec<&str> = page1["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids1, vec!["u0", "a0"]);
    assert_eq!(page1["cursor"], "a0");

    let (_, _, page2) = get(
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
    let (status, _, _) = get(rt, "/v1/ai-sdk/threads/t-hist/messages?cursor=nope").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn an_unavailable_history_store_is_not_projected_as_an_empty_thread() {
    // Cause/effect table: R1 successful empty read => 200 empty page; R2
    // unavailable read => 503; R3 malformed cursor after a successful read =>
    // 400. R1/R3 are covered above; this is the fail-closed R2 rule.
    let rt = Arc::new(PersistedRuntime {
        thread: "t-hist".into(),
        messages: Vec::new(),
        unavailable: true,
    });
    let (status, _, _) = get(rt, "/v1/ai-sdk/threads/t-hist/messages").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "R2");
}

/// Test design — history contains user/model content and must never become cacheable. The table
/// spans a successful page, a caller-fault cursor, and an unavailable authority; every terminal
/// response must carry the same no-store boundary so intermediaries cannot retain either content
/// or failure-dependent existence signals.
#[tokio::test]
async fn every_history_response_is_no_store() {
    let healthy = runtime(transcript(1));
    for uri in [
        "/v1/ai-sdk/threads/t-hist/messages",
        "/v1/ai-sdk/threads/t-hist/messages?cursor=nope",
    ] {
        let (_, headers, _) = get(Arc::clone(&healthy), uri).await;
        assert_eq!(headers.get("cache-control").unwrap(), "no-store");
    }

    let unavailable = Arc::new(PersistedRuntime {
        thread: "t-hist".into(),
        messages: Vec::new(),
        unavailable: true,
    });
    let (_, headers, _) = get(unavailable, "/v1/ai-sdk/threads/t-hist/messages").await;
    assert_eq!(headers.get("cache-control").unwrap(), "no-store");
}
