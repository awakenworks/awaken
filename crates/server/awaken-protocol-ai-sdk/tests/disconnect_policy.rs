//! T32-C01 (stream-close policy) at the AI SDK protocol boundary.
//!
//! Cause/effect decision table. Causes: C1 a Run is active behind a live AI SDK
//! SSE response; C2 only the client connection closes; C3 the Run port is the
//! hosted owner-scoped one (production composition) vs the connection-bound
//! Local one; C4 the client later issues the explicit browser Stop (DELETE).
//! Effects: E1 the owner-scoped Run is detached: no interrupt reaches the Run
//! port and the original drive settles by its own completion (O0/O1); E2 a
//! policy that maps the socket close to a Run terminal is reported by the same
//! oracle checker (negative control, executed against `RunMode::Local`, whose
//! disconnect path interrupts); E3 the explicit Stop reaches the interrupt port
//! exactly once, independently of the passive close (O2), and should return
//! its own receipt (product gap reproducer below).
//! Rules: R1 C1+C2+C3(owner) => E1; R2 C1+C2+C3(local) => E2; R3 R1+C4 => E3.

use std::sync::Arc;
use std::time::Duration;

use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{EndCause, Id as RunId, RunState};
use awaken_agent_contract::event::{AgentEvent, Delta};
use awaken_agent_contract::stream::event::Event;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_session_application_contract::{
    OwnerScopedRunApplication, Pending, RunApplication, RunApplicationError, RunResume, StepOutcome,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tokio::sync::{Notify, mpsc};
use tower::ServiceExt;

const THREAD: &str = "thread-t32";
const OWNER: &str = "test-workspace";

/// Every externally observable effect of the probe Run, in commit order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Probe {
    Started { owner: Option<String> },
    SinkClosed,
    Interrupted(String),
    Settled(RunState),
}

/// A Run that streams a live heartbeat until its sink closes, then waits for
/// its own completion (`release`). An interrupt is the real Run-port cancel:
/// it is recorded and ends the drive as `Cancelled`, like the runtime does.
struct ProbeRun {
    events: mpsc::UnboundedSender<Probe>,
    cancel: Notify,
    release: Notify,
}

impl ProbeRun {
    fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<Probe>) {
        let (events, receiver) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                events,
                cancel: Notify::new(),
                release: Notify::new(),
            }),
            receiver,
        )
    }

    async fn drive(&self, owner: Option<String>, sink: Arc<dyn StreamSink>) -> StepOutcome {
        self.events.send(Probe::Started { owner }).unwrap();
        // The heartbeat is the producer's live cadence, not a wait on a fact:
        // the Run keeps emitting best-effort deltas until the sink is closed.
        let mut heartbeat = tokio::time::interval(Duration::from_millis(1));
        let mut sink_closed = false;
        let cause = loop {
            // Biased: a stored cancel permit always wins over completion, so
            // an interrupt issued before the sink closed decides the terminal.
            tokio::select! {
                biased;
                () = self.cancel.notified() => break EndCause::Cancelled,
                () = self.release.notified(), if sink_closed => break EndCause::NaturalEnd,
                _ = heartbeat.tick(), if !sink_closed => {
                    let sent = sink
                        .send(Event {
                            run_id: RunId("run-t32".into()),
                            kind: AgentEvent::Delta(Delta::TextDelta { delta: "x".into() }),
                        })
                        .await;
                    if sent.is_err() {
                        sink_closed = true;
                        self.events.send(Probe::SinkClosed).unwrap();
                    }
                }
            }
        };
        let outcome = StepOutcome::ended(Vec::new(), cause);
        self.events
            .send(Probe::Settled(outcome.state().clone()))
            .unwrap();
        outcome
    }
}

#[async_trait::async_trait]
impl RunApplication for ProbeRun {
    async fn run(
        &self,
        _: &str,
        _: &str,
        _: Option<String>,
        _: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        unreachable!("T32-C01 drives only the streaming Run")
    }

    async fn run_streaming(
        &self,
        _: &str,
        _: &str,
        _: Option<String>,
        _: Vec<Message>,
        sink: Arc<dyn StreamSink>,
    ) -> Result<StepOutcome, RunApplicationError> {
        Ok(self.drive(None, sink).await)
    }

    async fn resume(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: RunResume,
    ) -> Result<StepOutcome, RunApplicationError> {
        unreachable!()
    }

    async fn interrupt(&self, thread: &str) -> Result<(), RunApplicationError> {
        self.events
            .send(Probe::Interrupted(thread.to_owned()))
            .unwrap();
        self.cancel.notify_one();
        Ok(())
    }

    async fn pending(&self, _: &str) -> Result<Option<Pending>, RunApplicationError> {
        Ok(None)
    }

    async fn history(&self, _: &str) -> Result<Vec<Message>, RunApplicationError> {
        Ok(Vec::new())
    }

    fn model(&self) -> String {
        "t32-probe".into()
    }
}

#[async_trait::async_trait]
impl OwnerScopedRunApplication for ProbeRun {
    async fn run_streaming_for_owner(
        &self,
        owner_scope: &str,
        _: &str,
        _: &str,
        _: Option<String>,
        _: Vec<Message>,
        sink: Arc<dyn StreamSink>,
    ) -> Result<StepOutcome, RunApplicationError> {
        Ok(self.drive(Some(owner_scope.to_owned()), sink).await)
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

fn owner_scoped_app(run: Arc<ProbeRun>) -> Router {
    awaken_protocol_ai_sdk::router::router_with_owner_scope(run).layer(axum::Extension(
        awaken_tenancy::WorkspaceScope(OWNER.into()),
    ))
}

fn local_app(run: Arc<ProbeRun>) -> Router {
    awaken_protocol_ai_sdk::router(run, OWNER.into())
}

async fn next_probe(receiver: &mut mpsc::UnboundedReceiver<Probe>, what: &str) -> Probe {
    tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
        .unwrap_or_else(|| panic!("probe channel closed before {what}"))
}

/// What one passive stream close produced, observed at the Run port.
#[derive(Debug, Clone)]
struct DisconnectObservation {
    starts: Vec<Option<String>>,
    interrupts: Vec<String>,
    terminal: RunState,
}

/// The single T32-C01 oracle checker (O0/O1) shared by the production policy
/// and the negative control. An empty result means the passive close detached.
fn disconnect_policy_violations(observation: &DisconnectObservation) -> Vec<String> {
    let mut violations = Vec::new();
    if !observation.interrupts.is_empty() {
        violations.push(format!(
            "O0: passive stream close was mapped to a Run interrupt: {:?}",
            observation.interrupts
        ));
    }
    if observation.terminal != RunState::Ended(EndCause::NaturalEnd) {
        violations.push(format!(
            "O0: Run terminal {:?} was caused by the socket close, not by its own completion",
            observation.terminal
        ));
    }
    if observation.starts.len() != 1 {
        violations.push(format!(
            "O1: expected one original Run drive, observed {}",
            observation.starts.len()
        ));
    }
    if observation
        .starts
        .iter()
        .any(|owner| owner.as_deref().is_some_and(|owner| owner != OWNER))
    {
        violations.push(format!(
            "O1: the Run owner changed away from the admitted scope: {:?}",
            observation.starts
        ));
    }
    violations
}

/// Open a live Run over the real router, close only the client connection
/// once the Run is active, then let the Run settle by itself if it was not
/// cancelled. Returns what the Run port observed.
async fn close_stream_mid_run(
    app: Router,
    run: &ProbeRun,
    receiver: &mut mpsc::UnboundedReceiver<Probe>,
) -> DisconnectObservation {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/ai-sdk/threads/{THREAD}/runs"))
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
    let mut starts = Vec::new();
    match next_probe(receiver, "the Run to become active").await {
        Probe::Started { owner } => starts.push(owner),
        other => panic!("expected the Run to start first, observed {other:?}"),
    }
    // C2: the client goes away while the Run is active.
    drop(response);

    // Either the Run port is cancelled (Local) or the sink close is observed
    // (owner-scoped). The router drops its live receiver only after its
    // disconnect policy returned, so at SinkClosed every disconnect-caused
    // interrupt is already in this ordered log.
    let mut interrupts = Vec::new();
    let mut terminal = None;
    loop {
        match next_probe(receiver, "the disconnect to reach the Run").await {
            Probe::Started { owner } => starts.push(owner),
            Probe::Interrupted(thread) => interrupts.push(thread),
            Probe::SinkClosed => break,
            Probe::Settled(state) => {
                terminal = Some(state);
                break;
            }
        }
    }
    let terminal = match terminal {
        Some(state) => state,
        None => {
            // The detached Run completes on its own owner after the close.
            run.release.notify_one();
            loop {
                match next_probe(receiver, "the detached Run to settle").await {
                    Probe::Settled(state) => break state,
                    Probe::Interrupted(thread) => interrupts.push(thread),
                    Probe::Started { owner } => starts.push(owner),
                    Probe::SinkClosed => {}
                }
            }
        }
    };
    DisconnectObservation {
        starts,
        interrupts,
        terminal,
    }
}

async fn explicit_stop(app: Router) -> axum::response::Response {
    app.oneshot(
        Request::builder()
            .method("DELETE")
            .uri(format!("/v1/ai-sdk/threads/{THREAD}/runs"))
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t32_c01_owner_scoped_stream_close_detaches_and_explicit_stop_is_separate() {
    // R1 then R3 over the production (owner-scoped) router.
    let (run, mut receiver) = ProbeRun::new();
    let app = owner_scoped_app(run.clone());
    let observation = close_stream_mid_run(app.clone(), &run, &mut receiver).await;
    assert_eq!(
        disconnect_policy_violations(&observation),
        Vec::<String>::new(),
        "R1 owner-scoped close must detach: {observation:?}"
    );
    assert!(observation.interrupts.is_empty(), "R1/O0 no interrupt");
    assert_eq!(
        observation.terminal,
        RunState::Ended(EndCause::NaturalEnd),
        "R1/O0 the original drive settled by its own completion"
    );
    assert_eq!(
        observation.starts,
        vec![Some(OWNER.to_owned())],
        "R1/O1 exactly one drive, on the admitted owner scope"
    );

    // R3: the explicit Stop is its own command and reaches the Run port once.
    let response = explicit_stop(app).await;
    assert!(
        response.status().is_success(),
        "R3 explicit stop accepted: {}",
        response.status()
    );
    assert_eq!(
        next_probe(&mut receiver, "the explicit stop").await,
        Probe::Interrupted(THREAD.to_owned()),
        "R3/O2 the explicit stop is the only interrupt for this thread"
    );
    assert!(
        receiver.try_recv().is_err(),
        "R3/O2 the stop created no extra Run effect"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t32_c01_negative_control_socket_close_as_terminal_is_detected() {
    // R2: `RunMode::Local` unconditionally maps the socket close to a Run
    // interrupt (=> Cancelled terminal). The SAME checker must reject it;
    // otherwise R1's pass above would prove nothing.
    let (run, mut receiver) = ProbeRun::new();
    let observation = close_stream_mid_run(local_app(run.clone()), &run, &mut receiver).await;
    let violations = disconnect_policy_violations(&observation);
    assert_eq!(
        observation.interrupts,
        vec![THREAD.to_owned()],
        "R2 the mutant policy interrupted on close: {observation:?}"
    );
    assert_eq!(
        observation.terminal,
        RunState::Ended(EndCause::Cancelled),
        "R2 the mutant policy turned the close into a Run terminal"
    );
    assert!(
        violations
            .iter()
            .any(|violation| violation.starts_with("O0: passive stream close")),
        "R2 checker must flag the interrupt: {violations:?}"
    );
    assert!(
        violations
            .iter()
            .any(|violation| violation.starts_with("O0: Run terminal")),
        "R2 checker must flag the close-caused terminal: {violations:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t32_c01_explicit_stop_returns_an_independent_receipt() {
    // O2: an explicit cancel must carry its own attributable receipt, distinct
    // per command, like the Managed `user.interrupt` event id/processed_at.
    // RunApplication::stop answers each accepted Stop with its own receipt.
    let (run, _receiver) = ProbeRun::new();
    let app = owner_scoped_app(run);
    let mut receipts = Vec::new();
    for _ in 0..2 {
        let response = explicit_stop(app.clone()).await;
        assert_ne!(
            response.status(),
            StatusCode::NO_CONTENT,
            "O2 explicit stop must return a receipt body, not 204"
        );
        assert!(response.status().is_success(), "{}", response.status());
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let receipt = ["receipt_id", "id", "event_id"]
            .iter()
            .find_map(|key| body.get(*key).and_then(serde_json::Value::as_str))
            .map(str::to_owned)
            .unwrap_or_else(|| panic!("O2 stop response names no receipt id: {body}"));
        assert!(!receipt.is_empty(), "O2 receipt id is non-empty");
        receipts.push(receipt);
    }
    assert_ne!(
        receipts[0], receipts[1],
        "O2 each explicit cancel has its own independent receipt"
    );
}
