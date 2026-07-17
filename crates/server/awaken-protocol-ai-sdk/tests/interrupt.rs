//! Integration test: a client hang-up mid-stream must actually FIRE
//! `rt.interrupt(&thread)` so an abandoned turn stops burning tokens.
//!
//! The sibling `streaming::a_client_hangup_mid_stream_stops_the_producer_without_hanging`
//! only proves the *producer* stops (the runtime observes its sink closing). It
//! does NOT prove the router called the neutral cancel verb — a router that simply
//! returned on the send error, never calling `interrupt`, would pass that test
//! while leaking the in-flight turn. This test closes that gap with a recording
//! `ProtocolRuntime` double that captures every `interrupt` call, mirroring the
//! contract the ag-ui adapter upholds (both converge on `interrupt`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::event::{AgentEvent, Delta};
use awaken_agent_contract::stream::event::Event;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_protocol_transport::{
    DriverError, Pending, ProtocolRuntime, Resume, StepOutcome, Terminal,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tower::ServiceExt;

/// A runtime that streams live text until its sink closes, then records the thread
/// id of every `interrupt` call it receives — the recording double this test asserts on.
struct InterruptRecorder {
    /// Every thread id passed to `interrupt`, in call order.
    interrupted: Arc<Mutex<Vec<String>>>,
    /// Set once the streaming producer observes its sink close (the client hung up).
    observed_close: Arc<AtomicBool>,
    /// Fired once `run_streaming` returns, so the test waits without sleeping.
    finished: Arc<Notify>,
}

#[async_trait::async_trait]
impl ProtocolRuntime for InterruptRecorder {
    async fn run(
        &self,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, DriverError> {
        unreachable!("this test only streams")
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

    async fn interrupt(&self, thread: &str) -> Result<(), DriverError> {
        self.interrupted.lock().await.push(thread.to_string());
        Ok(())
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_hangup_fires_interrupt_for_the_thread() {
    let interrupted = Arc::new(Mutex::new(Vec::<String>::new()));
    let observed_close = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(Notify::new());
    let rt = Arc::new(InterruptRecorder {
        interrupted: interrupted.clone(),
        observed_close: observed_close.clone(),
        finished: finished.clone(),
    });

    let app = awaken_protocol_ai_sdk::router(rt);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/ai-sdk/threads/thread-XYZ/runs")
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

    // The client disconnects mid-stream: drop the streaming body unread. The router's
    // `out_tx.send(...).is_err()` branch must fire `rt.interrupt(thread)` before it
    // returns — not merely stop looping.
    drop(response);
    tokio::time::timeout(Duration::from_secs(5), finished.notified())
        .await
        .expect("run_streaming terminates after the hang-up");

    assert!(
        observed_close.load(Ordering::SeqCst),
        "the runtime observed its sink close (the client went away)"
    );
    let calls = interrupted.lock().await;
    assert_eq!(
        calls.as_slice(),
        &["thread-XYZ".to_string()],
        "the hang-up path must call interrupt exactly once, for the run's thread"
    );
}
