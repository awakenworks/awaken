//! Integration test: a client hang-up mid-stream must actually FIRE
//! `rt.interrupt(&thread)` so an abandoned turn stops burning tokens.
//!
//! The sibling `streaming::a_client_hangup_mid_stream_stops_the_producer_without_hanging`
//! only proves the *producer* stops (the runtime observes its sink closing). It
//! does NOT prove the router called the neutral cancel verb — a router that simply
//! returned on the send error, never calling `interrupt`, would pass that test
//! while leaking the in-flight turn. This test closes that gap with a recording
//! `RunApplication` double that captures every `interrupt` call, mirroring the
//! contract the ag-ui adapter upholds (both converge on `interrupt`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::event::{AgentEvent, Delta};
use awaken_agent_contract::stream::event::Event;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_session_application_contract::{
    Pending, RunApplication, RunApplicationError, RunResume, StepOutcome,
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
    closed: Arc<Notify>,
    release_after_close: Option<Arc<Notify>>,
    /// Fired once `run_streaming` returns, so the test waits without sleeping.
    finished: Arc<Notify>,
}

#[async_trait::async_trait]
impl RunApplication for InterruptRecorder {
    async fn run(
        &self,
        _operation_id: &str,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        unreachable!("this test only streams")
    }

    async fn run_streaming(
        &self,
        _operation_id: &str,
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
                self.closed.notify_one();
                if let Some(release) = &self.release_after_close {
                    release.notified().await;
                }
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
    ) -> Result<StepOutcome, RunApplicationError> {
        unreachable!()
    }

    async fn interrupt(&self, thread: &str) -> Result<(), RunApplicationError> {
        self.interrupted.lock().await.push(thread.to_string());
        Ok(())
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

#[async_trait::async_trait]
impl awaken_session_application_contract::OwnerScopedRunApplication for InterruptRecorder {
    async fn run_streaming_for_owner(
        &self,
        _: &str,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
        sink: Arc<dyn StreamSink>,
    ) -> Result<StepOutcome, RunApplicationError> {
        self.run_streaming(operation_id, thread, agent, messages, sink)
            .await
    }

    async fn resume_for_owner(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: RunResume,
    ) -> Result<StepOutcome, RunApplicationError> {
        unreachable!()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_hangup_fires_interrupt_for_the_thread() {
    let interrupted = Arc::new(Mutex::new(Vec::<String>::new()));
    let observed_close = Arc::new(AtomicBool::new(false));
    let closed = Arc::new(Notify::new());
    let finished = Arc::new(Notify::new());
    let rt = Arc::new(InterruptRecorder {
        interrupted: interrupted.clone(),
        observed_close: observed_close.clone(),
        closed,
        release_after_close: None,
        finished: finished.clone(),
    });

    let app = awaken_protocol_ai_sdk::router(rt, "test-workspace".into());
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_scoped_client_hangup_detaches_without_interrupting_the_run() {
    // Causes: C1 an authenticated owner-scoped Run was admitted; C2 only its
    // SSE client closes; C3 its executor later returns. Effects: E1 C2 closes
    // the live sink but emits no Run interrupt; E2 C3 still completes the
    // original drive. R1=C1+C2->E1, R2=R1+C3->E2. Local hang-up is covered
    // above and remains interruptible through the same shared Run port.
    let interrupted = Arc::new(Mutex::new(Vec::<String>::new()));
    let observed_close = Arc::new(AtomicBool::new(false));
    let closed = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let finished = Arc::new(Notify::new());
    let rt = Arc::new(InterruptRecorder {
        interrupted: interrupted.clone(),
        observed_close: observed_close.clone(),
        closed: closed.clone(),
        release_after_close: Some(release.clone()),
        finished: finished.clone(),
    });
    let app = awaken_protocol_ai_sdk::router::router_with_owner_scope(rt).layer(axum::Extension(
        awaken_tenancy::WorkspaceScope("test-workspace".into()),
    ));
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
    drop(response);
    tokio::time::timeout(Duration::from_secs(5), closed.notified())
        .await
        .expect("the best-effort sink closes after the observer leaves");
    assert!(observed_close.load(Ordering::SeqCst), "R1");
    assert!(interrupted.lock().await.is_empty(), "R1");
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), finished.notified())
        .await
        .expect("the original hosted drive completes after disconnection");
    assert!(interrupted.lock().await.is_empty(), "R2");
}

#[tokio::test]
async fn explicit_stop_reaches_the_exact_existing_interrupt_port() {
    // Causes: C1 the guarded browser DELETE names an external Thread and has
    // its exact resolved Session extension; C2 the same Stop is repeated.
    // Effects: E1/E2 the sole RunApplication interrupt sees only the bound
    // Session, each response is 204, and no new Run or cancel store is made.
    let interrupted = Arc::new(Mutex::new(Vec::<String>::new()));
    let rt = Arc::new(InterruptRecorder {
        interrupted: interrupted.clone(),
        observed_close: Arc::new(AtomicBool::new(false)),
        closed: Arc::new(Notify::new()),
        release_after_close: None,
        finished: Arc::new(Notify::new()),
    });
    let app = awaken_protocol_ai_sdk::router::router_with_owner_scope(rt)
        .layer(axum::Extension(awaken_tenancy::WorkspaceScope(
            "test-workspace".into(),
        )))
        .layer(axum::Extension(awaken_tenancy::ResolvedResourceId(
            "sesn_existing".into(),
        )));
    for _ in 0..2 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/v1/ai-sdk/threads/customer-thread/runs")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
    assert_eq!(
        interrupted.lock().await.as_slice(),
        &["sesn_existing".to_string(), "sesn_existing".to_string()],
    );
}
