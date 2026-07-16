//! Integration test: a streamed AG-UI turn emits `TOOL_CALL_START` +
//! `TOOL_CALL_ARGS` argument deltas live and closes with `TOOL_CALL_END` +
//! `RUN_FINISHED`, over a real chunked SSE body driven through the axum router.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::stream::event::{Event, Kind};
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_protocol_transport::{
    DriverError, Pending, ProtocolRuntime, Resume, StepOutcome, Terminal,
};
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tower::ServiceExt;

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
                text: "reading ".into(),
            },
            Kind::ToolCallDelta {
                call_id: "c1".into(),
                tool_id: "read".into(),
                args_delta: "".into(),
            },
            Kind::ToolCallDelta {
                call_id: "c1".into(),
                tool_id: "read".into(),
                args_delta: "{\"path\":".into(),
            },
            Kind::ToolCallDelta {
                call_id: "c1".into(),
                tool_id: "read".into(),
                args_delta: "\"x\"}".into(),
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

async fn frames() -> Vec<Value> {
    let app = awaken_protocol_ag_ui::router::router(Arc::new(StreamingMock));
    let body = json!({
        "threadId": "t1",
        "runId": "r1",
        "messages": [{ "role": "user", "content": "go" }],
    });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ag-ui")
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
        .map(|d| serde_json::from_str(d).unwrap())
        .collect()
}

#[tokio::test]
async fn streams_tool_call_args_then_closes_with_end_and_finish() {
    let frames = frames().await;
    let types: Vec<&str> = frames.iter().map(|f| f["type"].as_str().unwrap()).collect();

    let idx = |t: &str| types.iter().position(|k| *k == t);
    let start = idx("RUN_STARTED").expect("RUN_STARTED");
    let tool_start = idx("TOOL_CALL_START").expect("TOOL_CALL_START");
    let first_args = idx("TOOL_CALL_ARGS").expect("TOOL_CALL_ARGS");
    let tool_end = idx("TOOL_CALL_END").expect("TOOL_CALL_END");
    let finish = idx("RUN_FINISHED").expect("RUN_FINISHED");
    assert!(start < tool_start && tool_start < first_args && first_args < tool_end);
    assert!(tool_end < finish);
    assert_eq!(types.iter().filter(|k| **k == "RUN_STARTED").count(), 1);

    // The argument deltas concatenate to the full tool input JSON.
    let joined: String = frames
        .iter()
        .filter(|f| f["type"] == "TOOL_CALL_ARGS")
        .map(|f| f["delta"].as_str().unwrap())
        .collect();
    assert_eq!(joined, "{\"path\":\"x\"}");
}

/// A runtime that streams best-effort live text until its sink closes, recording
/// when it observes the close. Paced so the buffer stays small and a client
/// disconnect propagates within a couple of iterations; the cap guards against a
/// regression that never notices the hang-up.
struct HangupProbe {
    observed_close: Arc<AtomicBool>,
    finished: Arc<Notify>,
}

#[async_trait::async_trait]
impl ProtocolRuntime for HangupProbe {
    async fn run(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, DriverError> {
        unreachable!("the disconnect test only streams")
    }

    async fn run_streaming(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
        sink: Arc<dyn StreamSink>,
    ) -> Result<StepOutcome, DriverError> {
        let run = RunId("r1".into());
        for _ in 0..10_000 {
            let sent = sink
                .send(Event {
                    run_id: run.clone(),
                    kind: Kind::OutputText { text: "x".into() },
                })
                .await;
            if sent.is_err() {
                self.observed_close.store(true, Ordering::SeqCst);
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        self.finished.notify_one();
        Ok(StepOutcome {
            new_messages: Vec::new(),
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
        "mock".into()
    }
}

#[tokio::test]
async fn a_client_hangup_mid_stream_stops_the_producer_without_hanging() {
    let observed_close = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(Notify::new());
    let rt = Arc::new(HangupProbe {
        observed_close: observed_close.clone(),
        finished: finished.clone(),
    });
    let app = awaken_protocol_ag_ui::router::router(rt);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ag-ui")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "threadId": "t1", "messages": [{ "role": "user", "content": "go" }] })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // The client disconnects mid-stream: drop the response and its streaming body
    // without reading it. The producer's `out_tx.send(...).is_err()` branch must
    // fire and return, dropping the sink receiver so the runtime sees the close —
    // and the whole thing must terminate, not hang or spin.
    drop(response);
    tokio::time::timeout(Duration::from_secs(5), finished.notified())
        .await
        .expect("run_streaming must terminate after the client hangs up (no hang)");
    assert!(
        observed_close.load(Ordering::SeqCst),
        "the runtime observed the sink closing once the client disconnected"
    );
}
