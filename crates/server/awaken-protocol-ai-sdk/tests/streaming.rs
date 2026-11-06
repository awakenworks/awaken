//! Integration test: a streamed turn emits `tool-input-start`/`tool-input-delta`
//! frames live and closes with the authoritative `tool-input-available` +
//! `finish`, over a real chunked SSE body driven through the axum router.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::event::{AgentEvent, Delta, Fact};
use awaken_agent_contract::stream::event::Event;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_runtime_contract::runs::{Pending, RunError, RunResume, Runs, StepOutcome};
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tower::ServiceExt;

/// A runtime that scripts a streamed turn: it pushes a text delta and three
/// cumulative tool-argument snapshots onto the live sink, then returns the
/// committed step (an awaiting client tool call with the parsed input).
struct StreamingMock;

#[async_trait::async_trait]
impl Runs for StreamingMock {
    async fn run(
        &self,
        _operation_id: &str,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, RunError> {
        Ok(committed())
    }

    async fn run_streaming(
        &self,
        _operation_id: &str,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
        sink: Arc<dyn StreamSink>,
    ) -> Result<StepOutcome, RunError> {
        let run = RunId("r1".into());
        for kind in [
            AgentEvent::Fact(Fact::RunStarted),
            AgentEvent::Delta(Delta::TextDelta {
                delta: "Let me read ".into(),
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
        _operation_id: &str,
        _thread: &str,
        _tool_use_id: &str,
        _resume: RunResume,
    ) -> Result<StepOutcome, RunError> {
        unreachable!()
    }

    async fn pending(&self, _thread: &str) -> Result<Option<Pending>, RunError> {
        Ok(None)
    }

    async fn history(&self, _thread: &str) -> Result<Vec<Message>, RunError> {
        Ok(Vec::new())
    }

    fn model(&self) -> String {
        "mock".into()
    }
}

/// The committed step: a client tool call the run awaiting on, with parsed input.
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
    )
}

/// Collect the SSE body into the ordered list of decoded `{ "type": ... }` frames.
async fn frames() -> Vec<Value> {
    let app = awaken_protocol_ai_sdk::router(Arc::new(StreamingMock), "test-workspace".into());
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
    // Causes: the fixtures below establish `falls back to full projection when nothing streamed`
    // with the concrete inputs, state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `all output, state, side-effect, error, and terminal
    // assertions below hold together` and every asserted state transition or side effect must hold.
    // Constraints/invariants: this adapter owns only its public wire mapping; the shared neutral
    // Runtime and committed facts remain the single execution authority.
    // Coverage rationale: `falls back to full projection when nothing streamed` is one independent
    // branch selecting `all output, state, side-effect, error, and terminal assertions below hold
    // together`; a multi-row decision table is not applicable, and sibling tests own alternate
    // causes.
    // A runtime whose `run_streaming` is the default (no live events) still
    // yields a well-formed stream with a single `start` and the committed tail.
    struct SilentMock;
    #[async_trait::async_trait]
    impl Runs for SilentMock {
        async fn run(
            &self,
            _operation_id: &str,
            _t: &str,
            _a: Option<String>,
            _m: Vec<Message>,
        ) -> Result<StepOutcome, RunError> {
            Ok(StepOutcome::ended(
                vec![Message::text(Id("a1".into()), Role::Assistant, "hello")],
                awaken_agent_contract::agent::run::EndCause::NaturalEnd,
            ))
        }
        async fn resume(
            &self,
            _operation_id: &str,
            _t: &str,
            _id: &str,
            _r: RunResume,
        ) -> Result<StepOutcome, RunError> {
            unreachable!()
        }
        async fn pending(&self, _t: &str) -> Result<Option<Pending>, RunError> {
            Ok(None)
        }
        async fn history(&self, _t: &str) -> Result<Vec<Message>, RunError> {
            Ok(Vec::new())
        }
        fn model(&self) -> String {
            "silent".into()
        }
    }

    let app = awaken_protocol_ai_sdk::router(Arc::new(SilentMock), "test-workspace".into());
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

#[tokio::test]
async fn a_retained_best_effort_sink_cannot_hold_the_completed_stream_open() {
    // Causes: the fixtures below establish `a retained best effort sink` with the concrete inputs,
    // state, dependencies, and failure triggers used by this case.
    // Effects: the observable result `cannot hold the completed stream open` and every asserted
    // state transition or side effect must hold.
    // Constraints/invariants: this adapter owns only its public wire mapping; the shared neutral
    // Runtime and committed facts remain the single execution authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect decision table. C1 Run terminal, C2 sink retained, C3 queued
    // live event, C4 client connected. R1 C1+C2+!C3+C4 => finish+[DONE]+EOF;
    // R2 C1+C2+C3+C4 => drain queued events, then the same terminal sequence;
    // R3 !C1+C4 => continue receiving/keep-alive; R4 !C4 => interrupt Run.
    // This test owns R1 and the adjacent live-stream/disconnect tests own R2-R4.
    struct RetainedSinkMock {
        retained: Mutex<Option<Arc<dyn StreamSink>>>,
    }

    #[async_trait::async_trait]
    impl Runs for RetainedSinkMock {
        async fn run(
            &self,
            _operation_id: &str,
            _thread: &str,
            _agent: Option<String>,
            _messages: Vec<Message>,
        ) -> Result<StepOutcome, RunError> {
            unreachable!("the completion test uses run_streaming")
        }

        async fn run_streaming(
            &self,
            _operation_id: &str,
            _thread: &str,
            _agent: Option<String>,
            _messages: Vec<Message>,
            sink: Arc<dyn StreamSink>,
        ) -> Result<StepOutcome, RunError> {
            *self.retained.lock().unwrap() = Some(sink);
            Ok(StepOutcome::ended(
                vec![Message::text(Id("a1".into()), Role::Assistant, "complete")],
                awaken_agent_contract::agent::run::EndCause::NaturalEnd,
            ))
        }

        async fn resume(
            &self,
            _operation_id: &str,
            _thread: &str,
            _tool_use_id: &str,
            _resume: RunResume,
        ) -> Result<StepOutcome, RunError> {
            unreachable!()
        }

        async fn pending(&self, _thread: &str) -> Result<Option<Pending>, RunError> {
            Ok(None)
        }

        async fn history(&self, _thread: &str) -> Result<Vec<Message>, RunError> {
            Ok(Vec::new())
        }

        fn model(&self) -> String {
            "retained-sink".into()
        }
    }

    let runtime = Arc::new(RetainedSinkMock {
        retained: Mutex::new(None),
    });
    let app = awaken_protocol_ai_sdk::router(runtime.clone(), "test-workspace".into());
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
    let bytes = tokio::time::timeout(
        Duration::from_secs(1),
        to_bytes(response.into_body(), usize::MAX),
    )
    .await
    .expect("a completed Run closes without waiting for retained sink clones")
    .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("complete"));
    assert!(text.contains("\"type\":\"finish\""));
    assert!(text.contains("data: [DONE]"));
    assert!(runtime.retained.lock().unwrap().is_some());
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
impl Runs for HangupProbe {
    async fn run(
        &self,
        _operation_id: &str,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, RunError> {
        unreachable!("the disconnect test only streams")
    }

    async fn run_streaming(
        &self,
        _operation_id: &str,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
        sink: Arc<dyn StreamSink>,
    ) -> Result<StepOutcome, RunError> {
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
        ))
    }

    async fn resume(
        &self,
        _operation_id: &str,
        _thread: &str,
        _tool_use_id: &str,
        _resume: RunResume,
    ) -> Result<StepOutcome, RunError> {
        unreachable!()
    }
    async fn pending(&self, _thread: &str) -> Result<Option<Pending>, RunError> {
        Ok(None)
    }
    async fn history(&self, _thread: &str) -> Result<Vec<Message>, RunError> {
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
    let app = awaken_protocol_ai_sdk::router::router(rt, "test-workspace".into());
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/chat")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "messages": [{ "role": "user", "parts": [{ "type": "text", "text": "go" }] }] })
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

struct ResumeProbe {
    release: Arc<Notify>,
    interrupted: Arc<AtomicBool>,
    scopes: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Runs for ResumeProbe {
    async fn run(
        &self,
        _: &str,
        _: &str,
        _: Option<String>,
        _: Vec<Message>,
    ) -> Result<StepOutcome, RunError> {
        unreachable!()
    }
    async fn resume(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: RunResume,
    ) -> Result<StepOutcome, RunError> {
        unreachable!("a live continuation must not use the buffered resume method")
    }
    async fn resume_streaming(
        &self,
        operation: &str,
        thread: &str,
        call: &str,
        resume: RunResume,
        sink: Arc<dyn StreamSink>,
    ) -> Result<StepOutcome, RunError> {
        assert!(!operation.is_empty());
        assert_eq!(thread, "t1");
        assert_eq!(call, "c1");
        assert!(matches!(
            resume,
            RunResume::ClientResult {
                is_error: false,
                ..
            }
        ));
        for kind in [
            AgentEvent::Fact(Fact::RunStarted),
            AgentEvent::Delta(Delta::TextDelta {
                delta: "已有直营样板。".into(),
            }),
        ] {
            sink.send(Event {
                run_id: RunId("r1".into()),
                kind,
            })
            .await
            .unwrap();
        }
        self.release.notified().await;
        if self.interrupted.load(Ordering::SeqCst) {
            return Ok(StepOutcome::ended(
                Vec::new(),
                awaken_agent_contract::agent::run::EndCause::NaturalEnd,
            ));
        }
        sink.send(Event {
            run_id: RunId("r1".into()),
            kind: AgentEvent::Delta(Delta::TextDelta {
                delta: "API 服务由谁交付？".into(),
            }),
        })
        .await
        .unwrap();
        Ok(StepOutcome::ended(
            vec![Message::text(
                Id("reply".into()),
                Role::Assistant,
                "已有直营样板。API 服务由谁交付？",
            )],
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }
    async fn interrupt(&self, _: &str) -> Result<(), RunError> {
        self.interrupted.store(true, Ordering::SeqCst);
        self.release.notify_one();
        Ok(())
    }
    async fn pending(&self, _: &str) -> Result<Option<Pending>, RunError> {
        Ok(Some(Pending {
            tool_use_id: "c1".into(),
            name: "AskUserQuestion".into(),
            input: json!({}),
            client_executed: true,
        }))
    }
    async fn history(&self, _: &str) -> Result<Vec<Message>, RunError> {
        Ok(Vec::new())
    }
    fn model(&self) -> String {
        "mock".into()
    }
    async fn usage(&self, _: &str) -> Result<(u64, u64), RunError> {
        Ok((12, 34))
    }
}

#[async_trait::async_trait]
impl awaken_runtime_contract::runs::OwnedRuns for ResumeProbe {
    async fn run_streaming_for_owner(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: Option<String>,
        _: Vec<Message>,
        _: Arc<dyn StreamSink>,
    ) -> Result<StepOutcome, RunError> {
        unreachable!()
    }
    async fn resume_for_owner(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: RunResume,
    ) -> Result<StepOutcome, RunError> {
        unreachable!("owner scope must not downgrade streaming")
    }
    async fn resume_streaming_for_owner(
        &self,
        scope: &str,
        operation: &str,
        thread: &str,
        call: &str,
        resume: RunResume,
        sink: Arc<dyn StreamSink>,
    ) -> Result<StepOutcome, RunError> {
        self.scopes.lock().unwrap().push(scope.into());
        self.resume_streaming(operation, thread, call, resume, sink)
            .await
    }
}

#[tokio::test]
async fn client_tool_continuation_streams_before_commit_and_preserves_scope_and_usage() {
    // Cause/effect table: R1 exact client result + model still running -> first
    // real delta before commit; R2 authenticated scoped request -> only scoped
    // port; R3 later delta + committed tail -> no repeated text, one finish,
    // authoritative usage. The gate makes a buffered implementation time out.
    use http_body_util::BodyExt;
    let rt = Arc::new(ResumeProbe {
        release: Arc::new(Notify::new()),
        interrupted: Arc::new(AtomicBool::new(false)),
        scopes: Mutex::new(Vec::new()),
    });
    let app =
        awaken_protocol_ai_sdk::router::router_with_owner_scope(rt.clone()).layer(axum::Extension(
            awaken_runtime_contract::runs::scope::WorkspaceScope("owner-1".into()),
        ));
    let response = app.oneshot(Request::builder().method("POST").uri("/v1/ai-sdk/threads/t1/runs")
        .header("content-type", "application/json").body(Body::from(json!({"messages":[{"id":"a1","role":"assistant","parts":[{"type":"tool-AskUserQuestion","toolCallId":"c1","state":"output-available","output":{"status":"answered"}}]}]}).to_string())).unwrap()).await.unwrap();
    let mut body = response.into_body();
    let mut prefix = String::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !prefix.contains("已有直营样板") {
            let frame = body.frame().await.unwrap().unwrap();
            if let Ok(data) = frame.into_data() {
                prefix.push_str(std::str::from_utf8(&data).unwrap());
            }
        }
    })
    .await
    .expect("R1 text arrives while continuation is blocked");
    assert!(!prefix.contains("\"type\":\"finish\""));
    assert_eq!(*rt.scopes.lock().unwrap(), vec!["owner-1"], "R2");
    rt.release.notify_one();
    let tail = to_bytes(body, usize::MAX).await.unwrap();
    let all = prefix + std::str::from_utf8(&tail).unwrap();
    assert_eq!(
        all.matches("已有直营样板").count(),
        1,
        "R3 no replayed prefix"
    );
    assert!(all.contains("API 服务由谁交付"));
    assert_eq!(all.matches("\"type\":\"finish\"").count(), 1);
    assert!(all.contains("\"inputTokens\":12"));
    assert!(all.contains("\"outputTokens\":34"));
}

#[tokio::test]
async fn a_stalled_tool_continuation_interrupts_immediately_on_disconnect() {
    // D1 a resumed model emits a prefix then stalls with no new event;
    // D2 the client drops the SSE body -> same Run interrupt immediately,
    // not after a token or keep-alive timer. No additional resume is sent.
    use http_body_util::BodyExt;
    let rt = Arc::new(ResumeProbe {
        release: Arc::new(Notify::new()),
        interrupted: Arc::new(AtomicBool::new(false)),
        scopes: Mutex::new(Vec::new()),
    });
    let app = awaken_protocol_ai_sdk::router::router(rt.clone(), "owner-1".into());
    let response = app.oneshot(Request::builder().method("POST").uri("/v1/ai-sdk/threads/t1/runs")
        .header("content-type", "application/json").body(Body::from(json!({"messages":[{"id":"a1","role":"assistant","parts":[{"type":"tool-AskUserQuestion","toolCallId":"c1","state":"output-available","output":{"status":"answered"}}]}]}).to_string())).unwrap()).await.unwrap();
    let mut body = response.into_body();
    tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(body);
    tokio::time::timeout(Duration::from_secs(1), async {
        while !rt.interrupted.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("D2 interrupted without waiting for another live frame");
}
