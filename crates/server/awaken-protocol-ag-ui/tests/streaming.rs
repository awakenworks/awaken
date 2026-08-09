//! Integration test: a streamed AG-UI turn emits `TOOL_CALL_START` +
//! `TOOL_CALL_ARGS` argument deltas live and closes with `TOOL_CALL_END` +
//! `RUN_FINISHED`, over a real chunked SSE body driven through the axum router.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::event::{AgentEvent, Delta, Fact};
use awaken_agent_contract::stream::event::Event;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_session_contract::{
    Pending, RunApplication, RunApplicationError, RunResume, StepOutcome,
};
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tower::ServiceExt;

struct StreamingMock;

#[async_trait::async_trait]
impl RunApplication for StreamingMock {
    async fn run(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        Ok(committed())
    }

    async fn run_streaming(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
        sink: Arc<dyn StreamSink>,
    ) -> Result<StepOutcome, RunApplicationError> {
        let run = RunId("r1".into());
        for kind in [
            AgentEvent::Fact(Fact::RunStarted),
            AgentEvent::Delta(Delta::TextDelta {
                delta: "reading ".into(),
            }),
            AgentEvent::Delta(Delta::ToolCallDelta {
                id: "c1".into(),
                name: "read".into(),
                args_delta: "".into(),
            }),
            AgentEvent::Delta(Delta::ToolCallDelta {
                id: "c1".into(),
                name: "read".into(),
                args_delta: "{\"path\":".into(),
            }),
            AgentEvent::Delta(Delta::ToolCallDelta {
                id: "c1".into(),
                name: "read".into(),
                args_delta: "\"x\"}".into(),
            }),
            AgentEvent::Fact(Fact::RunFinished { exhausted: false }),
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
        "mock".into()
    }
}

fn committed() -> StepOutcome {
    StepOutcome::awaiting(
        vec![Message {
            id: Id("a1".into()),
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "c1".into(),
                name: "read".into(),
                input: json!({"path": "x"}),
            }],
        }],
        Some(Pending {
            tool_use_id: "c1".into(),
            name: "read".into(),
            input: json!({"path": "x"}),
            client_executed: true,
        }),
        false,
        false,
    )
}

async fn frames() -> Vec<Value> {
    frames_for(Arc::new(StreamingMock)).await
}

async fn frames_for(runtime: Arc<dyn RunApplication>) -> Vec<Value> {
    let app = awaken_protocol_ag_ui::router::router(runtime);
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

struct PrefixMock {
    events: Vec<AgentEvent>,
    outcome: StepOutcome,
}

#[async_trait::async_trait]
impl RunApplication for PrefixMock {
    async fn run(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        Ok(self.outcome.clone())
    }

    async fn run_streaming(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
        sink: Arc<dyn StreamSink>,
    ) -> Result<StepOutcome, RunApplicationError> {
        for kind in self.events.clone() {
            sink.send(Event {
                run_id: RunId("r1".into()),
                kind,
            })
            .await
            .unwrap();
        }
        Ok(self.outcome.clone())
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
        "mock".into()
    }
}

#[tokio::test]
async fn run_started_and_reasoning_only_prefix_keeps_committed_text_once() {
    // CE-AG3/AG4 router rule: non-projected reasoning does not select a lossy
    // close path, and a live RUN_STARTED is never repeated by completion.
    let frames = frames_for(Arc::new(PrefixMock {
        events: vec![
            AgentEvent::Fact(Fact::RunStarted),
            AgentEvent::Delta(Delta::ReasoningDelta {
                delta: "hmm".into(),
            }),
        ],
        outcome: StepOutcome::ended(
            vec![Message::text(Id("a1".into()), Role::Assistant, "answer")],
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
            false,
            false,
        ),
    }))
    .await;
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["type"] == "RUN_STARTED")
            .count(),
        1
    );
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["type"] == "TEXT_MESSAGE_CONTENT" && frame["delta"] == "answer")
            .count(),
        1
    );
}

#[tokio::test]
async fn text_only_prefix_and_committed_tool_emit_one_complete_tool_bracket() {
    // CE-AG7 router rule: L=empty and F={c1} after visible text requires a full
    // tool bracket; the committed call must not appear as an orphan END.
    let frames = frames_for(Arc::new(PrefixMock {
        events: vec![
            AgentEvent::Fact(Fact::RunStarted),
            AgentEvent::Delta(Delta::TextDelta {
                delta: "reading".into(),
            }),
        ],
        outcome: committed(),
    }))
    .await;
    for kind in ["TOOL_CALL_START", "TOOL_CALL_ARGS", "TOOL_CALL_END"] {
        assert_eq!(
            frames.iter().filter(|frame| frame["type"] == kind).count(),
            1
        );
    }
}

#[tokio::test]
async fn live_only_tool_is_closed_when_not_present_in_committed_outcome() {
    // CE-AG9 router rule: L={c1}, F=empty. Completion closes c1 before the
    // terminal event so a client never retains an open tool input bracket.
    let frames = frames_for(Arc::new(PrefixMock {
        events: vec![
            AgentEvent::Fact(Fact::RunStarted),
            AgentEvent::Delta(Delta::ToolCallDelta {
                id: "c1".into(),
                name: "read".into(),
                args_delta: "{".into(),
            }),
        ],
        outcome: StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
            false,
            false,
        ),
    }))
    .await;
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["type"] == "TOOL_CALL_START")
            .count(),
        1
    );
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["type"] == "TOOL_CALL_END")
            .count(),
        1
    );
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
impl RunApplication for HangupProbe {
    async fn run(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        unreachable!("the disconnect test only streams")
    }

    async fn run_streaming(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
        sink: Arc<dyn StreamSink>,
    ) -> Result<StepOutcome, RunApplicationError> {
        let run = RunId("r1".into());
        for _ in 0..10_000 {
            let sent = sink
                .send(Event {
                    run_id: run.clone(),
                    kind: AgentEvent::Delta(Delta::TextDelta { delta: "x".into() }),
                })
                .await;
            if sent.is_err() {
                self.observed_close.store(true, Ordering::SeqCst);
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        self.finished.notify_one();
        Ok(StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
            false,
            false,
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
