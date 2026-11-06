//! T32-C01 (断流策略), O2: an explicit AG-UI Stop is a Run command distinct
//! from a passive hang-up, and it has its own receipt.
//!
//! | Row | Request | Expected |
//! |---|---|---|
//! | S1 | `DELETE /v1/ag-ui/threads/{t}/runs` | the Run's interrupt port is invoked exactly once, for `t` only |
//! | S2 | the same Stop twice | each answer carries its own receipt id (ADR-0103) |
//!
//! The passive hang-up rows (Local interrupts, owner-scoped detaches) are
//! covered by this crate's streaming/interrupt tests; this file owns the
//! explicit command. Routed through the owner-scoped router production mounts.

use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_runtime_contract::runs::{OwnedRuns, Pending, RunError, RunResume, Runs, StepOutcome};
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

/// Records every interrupt the explicit Stop reaches; nothing else is driven.
#[derive(Default)]
struct StopRecorder {
    interrupted: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Runs for StopRecorder {
    async fn run(
        &self,
        _operation_id: &str,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
    ) -> Result<StepOutcome, RunError> {
        unreachable!("a Stop never starts a Run")
    }

    async fn resume(
        &self,
        _operation_id: &str,
        _thread: &str,
        _tool_use_id: &str,
        _resume: RunResume,
    ) -> Result<StepOutcome, RunError> {
        unreachable!("a Stop never resumes a Run")
    }

    async fn interrupt(&self, thread: &str) -> Result<(), RunError> {
        self.interrupted.lock().unwrap().push(thread.to_string());
        Ok(())
    }

    async fn pending(&self, _thread: &str) -> Result<Option<Pending>, RunError> {
        Ok(None)
    }

    async fn history(&self, _thread: &str) -> Result<Vec<Message>, RunError> {
        Ok(Vec::new())
    }

    fn model(&self) -> String {
        "stop-recorder".into()
    }
}

#[async_trait::async_trait]
impl OwnedRuns for StopRecorder {
    async fn run_streaming_for_owner(
        &self,
        _owner_scope: &str,
        _operation_id: &str,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<Message>,
        _sink: Arc<dyn StreamSink>,
    ) -> Result<StepOutcome, RunError> {
        unreachable!("a Stop never starts a Run")
    }

    async fn resume_for_owner(
        &self,
        _owner_scope: &str,
        _operation_id: &str,
        _thread: &str,
        _tool_use_id: &str,
        _resume: RunResume,
    ) -> Result<StepOutcome, RunError> {
        unreachable!("a Stop never resumes a Run")
    }
}

async fn stop(runtime: Arc<StopRecorder>, thread: &str) -> (StatusCode, Value) {
    let app =
        awaken_protocol_ag_ui::router::router_with_owner_scope(runtime).layer(axum::Extension(
            awaken_runtime_contract::runs::scope::WorkspaceScope("t32-workspace".into()),
        ));
    let response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/v1/ag-ui/threads/{thread}/runs"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn explicit_stop_reaches_the_runs_interrupt_port_exactly_once() {
    let runtime = Arc::new(StopRecorder::default());
    let (status, _) = stop(runtime.clone(), "t32-thread").await;
    assert!(status.is_success(), "S1 the Stop is accepted: {status}");
    assert_eq!(
        runtime.interrupted.lock().unwrap().as_slice(),
        ["t32-thread".to_string()],
        "S1 exactly one interrupt, for the addressed thread only"
    );
}

#[tokio::test]
async fn explicit_stop_returns_its_own_receipt() {
    let runtime = Arc::new(StopRecorder::default());
    let first = stop(runtime.clone(), "t32-thread").await;
    let second = stop(runtime.clone(), "t32-thread").await;
    for (row, (status, body)) in [("first", &first), ("second", &second)] {
        assert!(
            [StatusCode::OK, StatusCode::ACCEPTED].contains(status),
            "S2 {row} Stop answers with a receipt, not {status}"
        );
        assert!(
            body["id"].as_str().is_some_and(|id| !id.is_empty()),
            "S2 {row} Stop receipt carries its own id: {body}"
        );
    }
    assert_ne!(
        first.1["id"], second.1["id"],
        "S2 each explicit Stop is its own command with its own receipt"
    );
}
