//! Integration test: a streamed turn emits `tool-input-start`/`tool-input-delta`
//! frames live and closes with the authoritative `tool-input-available` +
//! `finish`, over a real chunked SSE body driven through the axum router.

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::stream::event::{Event, Kind};
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_protocol_transport::{
    DriverError, Pending, ProtocolRuntime, Resume, StepOutcome, Terminal,
};
use axum::body::{Body, to_bytes};
use axum::http::Request;
use serde_json::{Value, json};
use tower::ServiceExt;

/// A runtime that scripts a streamed turn: it pushes a text delta and three
/// cumulative tool-argument snapshots onto the live sink, then returns the
/// committed step (a parked client tool call with the parsed input).
struct StreamingMock;

#[async_trait::async_trait]
impl ProtocolRuntime for StreamingMock {
    async fn run(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, DriverError> {
        Ok(committed())
    }

    async fn run_streaming(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
        sink: Arc<dyn StreamSink>,
    ) -> Result<StepOutcome, DriverError> {
        let run = RunId("r1".into());
        for kind in [
            Kind::RunStarted,
            Kind::OutputText {
                text: "Let me read ".into(),
            },
            Kind::ToolCall {
                call_id: "c1".into(),
                tool_id: "read".into(),
                arguments: json!(""),
            },
            Kind::ToolCall {
                call_id: "c1".into(),
                tool_id: "read".into(),
                arguments: json!("{\"path\":"),
            },
            Kind::ToolCall {
                call_id: "c1".into(),
                tool_id: "read".into(),
                arguments: json!("{\"path\":\"x\"}"),
            },
            Kind::RunFinished,
        ] {
            sink.send(Event {
                run_id: run.clone(),
                kind,
            })
            .await
            .unwrap();
        }
        Ok(committed())
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
        "mock".into()
    }
}

/// The committed step: a client tool call the run parked on, with parsed input.
fn committed() -> StepOutcome {
    StepOutcome {
        new_messages: vec![Message {
            id: Id("a1".into()),
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "c1".into(),
                name: "read".into(),
                input: json!({"path": "x"}),
            }],
        }],
        terminal: Terminal::Waiting {
            pending: Some(Pending {
                tool_use_id: "c1".into(),
                name: "read".into(),
                input: json!({"path": "x"}),
                client_executed: true,
            }),
        },
    }
}

/// Collect the SSE body into the ordered list of decoded `{ "type": ... }` frames.
async fn frames() -> Vec<Value> {
    let app = awaken_protocol_ai_sdk::router(Arc::new(StreamingMock));
    let body =
        json!({ "messages": [{ "role": "user", "parts": [{ "type": "text", "text": "go" }] }] });
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
    text.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .map(|d| serde_json::from_str(d).unwrap())
        .collect()
}

fn kinds(frames: &[Value]) -> Vec<String> {
    frames
        .iter()
        .map(|f| f["type"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn streams_tool_input_deltas_then_authoritative_available() {
    let frames = frames().await;
    let types = kinds(&frames);

    // The in-flight prefix is streamed, then the authoritative tail — in order,
    // with no second `start` after the deltas.
    let idx = |t: &str| types.iter().position(|k| k == t);
    let start = idx("start").expect("start");
    let text = idx("text-delta").expect("text-delta");
    let tool_start = idx("tool-input-start").expect("tool-input-start");
    let first_delta = idx("tool-input-delta").expect("tool-input-delta");
    let available = idx("tool-input-available").expect("tool-input-available");
    let finish = idx("finish").expect("finish");
    assert!(start < text && text < tool_start && tool_start < first_delta);
    assert!(first_delta < available && available < finish);
    assert_eq!(types.iter().filter(|k| *k == "start").count(), 1);

    // The delta suffixes concatenate to the full input JSON.
    let joined: String = frames
        .iter()
        .filter(|f| f["type"] == "tool-input-delta")
        .map(|f| f["inputTextDelta"].as_str().unwrap())
        .collect();
    assert_eq!(joined, "{\"path\":\"x\"}");

    // The authoritative frame carries the parsed object, not the raw text.
    let available_frame = frames
        .iter()
        .find(|f| f["type"] == "tool-input-available")
        .unwrap();
    assert_eq!(available_frame["input"], json!({"path": "x"}));
    assert_eq!(available_frame["toolCallId"], "c1");
}

#[tokio::test]
async fn falls_back_to_full_projection_when_nothing_streamed() {
    // A runtime whose `run_streaming` is the default (no live events) still
    // yields a well-formed stream with a single `start` and the committed tail.
    struct SilentMock;
    #[async_trait::async_trait]
    impl ProtocolRuntime for SilentMock {
        async fn run(
            &self,
            _t: &str,
            _a: Option<String>,
            _m: Vec<Message>,
        ) -> Result<StepOutcome, DriverError> {
            Ok(StepOutcome {
                new_messages: vec![Message::text(Id("a1".into()), Role::Assistant, "hello")],
                terminal: Terminal::Finished,
            })
        }
        async fn resume(
            &self,
            _t: &str,
            _id: &str,
            _r: Resume,
        ) -> Result<StepOutcome, DriverError> {
            unreachable!()
        }
        async fn pending(&self, _t: &str) -> Option<Pending> {
            None
        }
        async fn history(&self, _t: &str) -> Vec<Message> {
            Vec::new()
        }
        fn model(&self) -> String {
            "silent".into()
        }
    }

    let app = awaken_protocol_ai_sdk::router(Arc::new(SilentMock));
    let body =
        json!({ "messages": [{ "role": "user", "parts": [{ "type": "text", "text": "go" }] }] });
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
    let types: Vec<&str> = text
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .map(|d| {
            let v: Value = serde_json::from_str(d).unwrap();
            match v["type"].as_str().unwrap() {
                "text-delta" => "text-delta",
                "start" => "start",
                "finish" => "finish",
                _ => "other",
            }
        })
        .collect();
    assert_eq!(types.iter().filter(|k| **k == "start").count(), 1);
    assert!(types.contains(&"text-delta"));
    assert!(types.contains(&"finish"));
    assert!(text.contains("hello"));
}
