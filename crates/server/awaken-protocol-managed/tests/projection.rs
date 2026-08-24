//! Cause-effect / decision-table coverage for the event → wire projection and the
//! session-lifecycle accounting that the happy-path `adapter.rs` suite leaves
//! uncovered: a terminal run *fault* projected as `session.error` (never swallowed
//! into a success idle), the compaction marker, cumulative usage accounting, an
//! all-empty-text assistant message dropped (matching the shared agent-contract
//! projection), the MCP tool-call events, the `retries_exhausted` idle, the
//! subagent-delegate child-thread lifecycle, the `session.updated` event, and the
//! archived-session read-only fence + committed terminal event.

mod support;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Failure, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::{RunLifecycleCursor, RunLifecycleEvent, RunLifecyclePage};
use awaken_protocol_managed::test_support::CoordinatedRuntimeFake;
use awaken_protocol_managed::{ManagedState, router};
use awaken_session_contract::{
    OutcomeDrive, RunError, SessionRuntime, SessionUsage, StepOutcome, ToolPermissionDecision,
};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

// --- Test harness ------------------------------------------------------------

async fn raw_call(
    app: &Router,
    method: &str,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(if body.is_null() {
            Body::empty()
        } else {
            Body::from(serde_json::to_vec(&body).unwrap())
        })
        .unwrap();
    let resp = Box::pin(app.clone().oneshot(req)).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, json)
}

async fn json_call(
    app: &Router,
    method: &str,
    uri: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let (status, json) = Box::pin(raw_call(app, method, uri, body)).await;
    assert_eq!(status, StatusCode::OK, "{method} {uri}: {json}");
    json
}

fn types(list: &serde_json::Value) -> Vec<String> {
    list["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap().to_string())
        .collect()
}

async fn create(app: &Router) -> String {
    let s = json_call(
        app,
        "POST",
        "/v1/sessions",
        serde_json::json!({
            "agent": "coder",
            "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID
        }),
    )
    .await;
    s["id"].as_str().unwrap().to_string()
}

async fn send_user(app: &Router, id: &str, text: &str) -> serde_json::Value {
    Box::pin(json_call(
        app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": text }] }] }),
    ))
    .await
}

fn app_with_runtime(runtime: impl SessionRuntime + 'static) -> (Arc<ManagedState>, Router) {
    let state = Arc::new(ManagedState::new(runtime));
    let app = router(state.clone());
    (state, app)
}

async fn reconcile_published_run(state: &ManagedState, session_id: &str) {
    // A queued Event's first opportunistic drive publishes its reserved Run and
    // returns at that external boundary. The lifecycle supervisor's next scan
    // observes terminal committed truth and advances the retained Event. This
    // in-process fake has no Runtime Host completion callback, so the harness
    // then settles the exact application-owned activity epoch through the same
    // canonical API. Box the application future like the production supervisor.
    support::drive_retained_session_events(state, session_id).await;
    let application = state.session_application();
    let session = Box::pin(application.session(session_id)).await.unwrap();
    for epoch in session.active_activity_epochs {
        Box::pin(application.settle_activity(session_id, epoch))
            .await
            .unwrap();
    }
}

async fn list_events(app: &Router, id: &str) -> serde_json::Value {
    json_call(
        app,
        "GET",
        &format!("/v1/sessions/{id}/events"),
        serde_json::Value::Null,
    )
    .await
}

/// A runtime whose single Run is a scripted [`StepOutcome`] (rebuilt per call so
/// the non-`Clone` fields are fresh), with optional committed recovery evidence
/// and a configurable cumulative usage tally. The evidence uses the production
/// snapshot/lifecycle contracts; it is not a Step-local observation substitute.
struct ScriptFake {
    make: Box<dyn Fn() -> StepOutcome + Send + Sync>,
    usage: SessionUsage,
    evidence_profile: Option<CommittedEvidence>,
    durable: Mutex<ScriptDurableState>,
}

struct CommittedRunEvidence {
    snapshot: awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
    lifecycle: Vec<RunLifecycleEvent>,
}

#[derive(Default)]
struct ScriptDurableState {
    reservations: HashMap<String, awaken_session_contract::SessionUserRunCommand>,
    run_states: HashMap<String, RunState>,
    committed_run: Option<CommittedRunEvidence>,
}

#[derive(Debug, Clone, Copy)]
enum CommittedEvidence {
    Terminal,
    Compaction,
    Reschedule,
}

impl CommittedEvidence {
    fn audit_records(
        self,
        run_id: &RunId,
        terminal_state: &RunState,
    ) -> Vec<awaken_agent_contract::audit::record::Record> {
        let mut events = vec![(
            1,
            awaken_agent_contract::audit::run_event::RunEvent::RunStateChanged {
                state: RunState::Running,
                await_reason: None,
            },
        )];
        if matches!(self, Self::Reschedule) {
            events.push((
                2,
                awaken_agent_contract::audit::run_event::RunEvent::RunRescheduled {
                    state: RunState::Running,
                    claim_epoch: 2,
                },
            ));
        }
        events.push((
            3,
            awaken_agent_contract::audit::run_event::RunEvent::RunStateChanged {
                state: terminal_state.clone(),
                await_reason: None,
            },
        ));
        events
            .into_iter()
            .map(|(sequence, event)| {
                let draft: awaken_agent_contract::audit::draft::Draft = event.into();
                awaken_agent_contract::audit::record::Record {
                    sequence,
                    run_id: run_id.clone(),
                    kind: draft.kind,
                    payload: draft.payload,
                }
            })
            .collect()
    }

    fn snapshot(
        self,
        thread_id: &str,
        run_id: RunId,
        outcome: StepOutcome,
    ) -> awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
        let terminal_state = outcome.state().clone();
        // The recovery snapshot owns committed Run output only. Root User input
        // is retained by the Session event batch and must never be fabricated by
        // a Runtime fixture, otherwise provenance and idempotency are untestable.
        let messages = outcome.new_messages;
        let state = if matches!(self, Self::Compaction) {
            vec![
                awaken_runtime_contract::compaction::RunCompactionMarker::command("other-run"),
                awaken_runtime_contract::compaction::RunCompactionMarker::command(&run_id.0),
            ]
        } else {
            Vec::new()
        };
        let events = self.audit_records(&run_id, &terminal_state);
        awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
            thread_id: ThreadId(thread_id.into()),
            claimed_run_id: run_id.clone(),
            runs: vec![awaken_agent_contract::agent::run::Record {
                id: run_id.clone(),
                thread_id: ThreadId(thread_id.into()),
                state: terminal_state,
            }],
            latest_run_id: Some(run_id),
            messages,
            state,
            events,
            resume_tickets: Vec::new(),
            thread_version: 3,
            store_cursor: 3,
            next_commit_ordinal: 1,
        }
    }

    fn lifecycle(
        self,
        thread_id: &str,
        run_id: RunId,
        terminal_state: RunState,
    ) -> Vec<RunLifecycleEvent> {
        let mut previous = None;
        self.audit_records(&run_id, &terminal_state)
            .into_iter()
            .filter_map(|record| {
                let state: RunState = serde_json::from_value(record.payload["state"].clone())
                    .expect("typed RunEvent always carries RunState");
                let kind = awaken_agent_contract::classify_run_lifecycle_record(
                    &record.kind,
                    &state,
                    previous.as_ref(),
                )?;
                if record.kind == awaken_agent_contract::audit::kind::Kind::RunStateChanged {
                    previous = Some(state.clone());
                }
                Some(RunLifecycleEvent {
                    cursor: awaken_agent_contract::encode_run_lifecycle_cursor(record.sequence, 0)
                        .expect("fixture cursor fits"),
                    source_commit_cursor: record.sequence,
                    thread_id: ThreadId(thread_id.into()),
                    run_id: run_id.clone(),
                    kind,
                    state,
                    await_reason: None,
                })
            })
            .collect()
    }
}

impl ScriptFake {
    fn new(make: impl Fn() -> StepOutcome + Send + Sync + 'static) -> Self {
        Self {
            make: Box::new(make),
            usage: SessionUsage::default(),
            evidence_profile: None,
            durable: Mutex::new(ScriptDurableState::default()),
        }
    }
    fn with_usage(mut self, usage: SessionUsage) -> Self {
        self.usage = usage;
        self
    }

    fn with_committed_evidence(mut self, evidence: CommittedEvidence) -> Self {
        self.evidence_profile = Some(evidence);
        self
    }
}

#[async_trait::async_trait]
impl SessionRuntime for ScriptFake {
    async fn execute_terminal_cleanup(
        &self,
        command: awaken_session_contract::SessionCleanupCommand,
    ) -> Result<awaken_session_contract::SessionCleanupCompletion, RunError> {
        Ok(awaken_session_contract::SessionCleanupCompletion::new(
            &command,
            Vec::new(),
        ))
    }

    async fn run(
        &self,
        _a: &str,
        _thread_id: &str,
        _c: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        Ok((self.make)())
    }
    async fn resume(
        &self,
        _t: &str,
        _tid: &str,
        _d: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("no resume"))
    }
    async fn resume_custom(
        &self,
        _t: &str,
        _tid: &str,
        _c: Vec<ContentBlock>,
        _e: bool,
    ) -> Result<StepOutcome, RunError> {
        Err(RunError::internal("no custom"))
    }
    async fn define_outcome(
        &self,
        _t: &str,
        _d: &str,
        _r: &str,
        _m: u32,
    ) -> Result<OutcomeDrive, RunError> {
        Err(RunError::internal("no outcome"))
    }
    async fn session_thread_recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        if session_id != thread_id {
            return Ok(None);
        }
        // Fake decision rule S1: before the scripted Run atomically publishes
        // its snapshot/lifecycle pair, selectors observe no committed Run.
        // Falling back to independently read messages/tickets would teach these
        // tests a consistency path that production intentionally rejects.
        Ok(self
            .durable
            .lock()
            .unwrap()
            .committed_run
            .as_ref()
            .map(|evidence| evidence.snapshot.clone()))
    }
    async fn committed_run_lifecycle(
        &self,
        thread_id: &str,
        cursor: RunLifecycleCursor,
        limit: usize,
    ) -> Result<RunLifecyclePage, RunError> {
        let events = self
            .durable
            .lock()
            .unwrap()
            .committed_run
            .as_ref()
            .map(|evidence| evidence.lifecycle.clone())
            .unwrap_or_default()
            .into_iter()
            .filter(|event| event.thread_id.0 == thread_id)
            .filter(|event| event.cursor > cursor)
            .take(limit)
            .collect::<Vec<_>>();
        Ok(RunLifecyclePage {
            next_cursor: events.last().map_or(cursor, |event| event.cursor),
            events,
        })
    }
    async fn reserve_session_user_run(
        &self,
        command: awaken_session_contract::SessionUserRunCommand,
    ) -> Result<awaken_session_contract::SessionUserRunReservation, RunError> {
        let mut durable = self.durable.lock().unwrap();
        if durable.run_states.contains_key(&command.run_id.0) {
            return Ok(awaken_session_contract::SessionUserRunReservation::Completed);
        }
        if let Some(existing) = durable.reservations.get(&command.run_id.0) {
            if existing != &command {
                return Err(RunError::bad_request(
                    "scripted Run id was reused with different input",
                ));
            }
            return Ok(awaken_session_contract::SessionUserRunReservation::AlreadyReserved);
        }
        durable
            .reservations
            .insert(command.run_id.0.clone(), command);
        Ok(awaken_session_contract::SessionUserRunReservation::Reserved)
    }
    async fn activate_session_user_run(
        &self,
        delivery: awaken_session_contract::SessionUserRunDelivery,
    ) -> Result<awaken_session_contract::SessionUserRunActivation, RunError> {
        let mut durable = self.durable.lock().unwrap();
        if durable.run_states.contains_key(&delivery.run_id.0) {
            return Ok(awaken_session_contract::SessionUserRunActivation::Completed);
        }
        let command = durable
            .reservations
            .get(&delivery.run_id.0)
            .ok_or_else(|| RunError::internal("scripted activation has no reservation"))?;
        if command.session_id != delivery.session_id {
            return Err(RunError::bad_request(
                "scripted activation changed its Session identity",
            ));
        }
        let outcome = (self.make)();
        if let Some(profile) = self.evidence_profile {
            // Fixture commit boundary: snapshot and lifecycle become visible
            // together only after this reserved Run is published. The stored
            // contract values, rather than a foreground boolean or Step-local
            // completion flag, are the read-side authority.
            durable.committed_run = Some(CommittedRunEvidence {
                snapshot: profile.snapshot(
                    &delivery.session_id,
                    delivery.run_id.clone(),
                    outcome.clone(),
                ),
                lifecycle: profile.lifecycle(
                    &delivery.session_id,
                    delivery.run_id.clone(),
                    outcome.state().clone(),
                ),
            });
        }
        durable
            .run_states
            .insert(delivery.run_id.0, outcome.state().clone());
        Ok(awaken_session_contract::SessionUserRunActivation::Activated)
    }
    async fn session_user_run_state(
        &self,
        _session_id: &str,
        run_id: &RunId,
    ) -> Result<Option<RunState>, RunError> {
        Ok(self
            .durable
            .lock()
            .unwrap()
            .run_states
            .get(&run_id.0)
            .cloned())
    }
    async fn session_usage(&self, _t: &str) -> Result<SessionUsage, RunError> {
        Ok(self.usage.clone())
    }
    fn model(&self) -> String {
        "test-model".into()
    }
}

fn assistant_text(id: &str, text: &str) -> Message {
    Message::text(Id(id.into()), Role::Assistant, text)
}

fn ended(messages: Vec<Message>) -> StepOutcome {
    StepOutcome::ended(messages, EndCause::NaturalEnd)
}

fn failed(messages: Vec<Message>, code: &str, message: &str) -> StepOutcome {
    StepOutcome::ended(
        messages,
        EndCause::Error(Failure::Inference {
            code: code.into(),
            message: message.into(),
        }),
    )
}

// --- CE: terminal run fault → session.error (never swallowed into success) ----

/// CRITICAL bug-class guard: a terminal run fault must project a *distinct*
/// `session.error` event carrying the fault message and `retry_status: exhausted`,
/// so a streaming/listing client observes the failure — it is NOT collapsed into
/// a bare success idle. The Run still idles afterward with `retries_exhausted`,
/// derived from the same `EndCause::Error` that produces the error event.
/// Cause/effect rule F1: one terminal primary fault produces Session Running,
/// public primary Thread Running, output/error, primary Thread Idle(exhausted),
/// usage, then aggregate Session Idle(exhausted), each exactly once.
#[tokio::test]
async fn a_terminal_run_fault_projects_session_error_before_idle() {
    // Causes: the fixtures below establish `a terminal run fault` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Effects: the observable result `projects session error before idle` and every asserted state
    // transition or side effect must hold.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let (state, app) = app_with_runtime(
        ScriptFake::new(|| {
            failed(
                vec![assistant_text("a", "partial work")],
                "provider_unavailable",
                "upstream model timed out",
            )
        })
        .with_committed_evidence(CommittedEvidence::Terminal),
    );
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    reconcile_published_run(&state, &id).await;
    let list = list_events(&app, &id).await;

    // The error is a first-class event between running and idle — not dropped.
    assert_eq!(
        types(&list),
        vec![
            "user.message",
            "session.status_running",
            "session.thread_status_running",
            "agent.message",
            "session.error",
            "session.thread_status_idle",
            "session.usage",
            "session.status_idle"
        ],
        "the fault surfaces as session.error, the Run still idles"
    );
    let err = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "session.error")
        .unwrap();
    assert_eq!(err["error"]["type"], "unknown_error");
    assert_eq!(err["error"]["retry_status"]["type"], "exhausted");
    assert_eq!(
        err["error"]["message"], "upstream model timed out",
        "the neutral fault message is carried through"
    );
    // The idle and error event derive from the same terminal authority.
    let idle = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["type"] == "session.status_idle")
        .unwrap();
    assert_eq!(idle["stop_reason"]["type"], "retries_exhausted");
    let thread_idle = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["type"] == "session.thread_status_idle")
        .unwrap();
    assert_eq!(thread_idle["stop_reason"]["type"], "retries_exhausted");
    assert!(
        thread_idle["session_thread_id"]
            .as_str()
            .is_some_and(|thread_id| thread_id.starts_with("sthr_")),
        "the primary failure status uses the public Thread identity"
    );
}

/// A classified fault `code` selects the richer SDK error variant + retry status
/// instead of always collapsing to `unknown_error`/`exhausted`: a `rate_limited`
/// code projects `model_rate_limited_error`, and a `context_overflow` code is
/// `model_request_failed_error` with a `terminal` retry status.
/// Causes: C1 a root User Event is retained; C2 the same primary Run has a
/// committed terminal failure prefix; C3 its stable failure code is either
/// `rate_limited` or `context_overflow`. Effects: E1 the failure is emitted once;
/// E2 C3 selects the matching public error kind and retry status without changing
/// its message. Decision rules: R1=C1+C2+C3(rate_limited)=>E1+rate-limit/exhausted;
/// R2=C1+C2+C3(context_overflow)=>E1+request-failed/terminal.
#[tokio::test]
async fn a_classified_fault_projects_the_matching_sdk_error_variant() {
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Coverage rationale: `a classified fault` is one independent branch selecting `projects the
    // matching sdk error variant`; a multi-row decision table is not applicable, and sibling tests
    // own alternate causes.
    for (code, kind, retry) in [
        ("rate_limited", "model_rate_limited_error", "exhausted"),
        ("context_overflow", "model_request_failed_error", "terminal"),
    ] {
        let (state, app) = app_with_runtime(
            ScriptFake::new(move || failed(vec![assistant_text("a", "partial")], code, "boom"))
                .with_committed_evidence(CommittedEvidence::Terminal),
        );
        let id = create(&app).await;
        send_user(&app, &id, "go").await;
        reconcile_published_run(&state, &id).await;
        let list = list_events(&app, &id).await;
        let err = list["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["type"] == "session.error")
            .unwrap();
        assert_eq!(err["error"]["type"], kind, "code {code} → error type");
        assert_eq!(
            err["error"]["retry_status"]["type"], retry,
            "code {code} → retry"
        );
        assert_eq!(err["error"]["message"], "boom");
    }
}

/// Causes: C1 a primary Run starts; C2 a typed `RunRescheduled` audit fact is in
/// the committed recovery prefix and its canonical lifecycle feed; C3 the
/// replacement completes. Effects: E1 aggregate and primary Thread enter
/// Running; E2 reschedule projects aggregate Rescheduled plus primary Thread
/// Rescheduled -> Running; E3 output precedes primary Thread Idle and aggregate
/// Idle; E4 every Thread status carries the listed public `sthr_` id.
/// Decision table: R1(C1,!C2)->E1; R2(C1,C2,!C3)->E1+E2;
/// R3(C1,C2,C3)->E1+E2+E3+E4.
#[tokio::test]
async fn a_rescheduled_run_projects_the_complete_thread_status_sequence() {
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let runtime = ScriptFake::new(|| ended(vec![assistant_text("a", "after a retry")]))
        .with_committed_evidence(CommittedEvidence::Reschedule);
    let (state, app) = app_with_runtime(runtime);
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    reconcile_published_run(&state, &id).await;
    let list = list_events(&app, &id).await;
    assert_eq!(
        types(&list),
        vec![
            "user.message",
            "session.status_running",
            "session.thread_status_running",
            "session.status_rescheduled",
            "session.thread_status_rescheduled",
            "session.thread_status_running",
            "agent.message",
            "session.thread_status_idle",
            "session.usage",
            "session.status_idle",
        ],
        "R3/E1-E3"
    );
    let primary = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/threads"),
        serde_json::Value::Null,
    )
    .await["data"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(primary.starts_with("sthr_"), "R3/E4");
    assert!(
        list["data"].as_array().unwrap().iter().all(|event| {
            !event["type"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("session.thread_status_"))
                || event["session_thread_id"] == primary
        }),
        "R3/E4"
    );
}

// --- CE: compaction marker ---------------------------------------------------

/// Causes: C1 the committed recovery snapshot contains the exact
/// `compaction/<run_id>` marker; C2 it also contains another Run's marker.
/// Effects: E1 the target Run projects `agent.thread_context_compacted` before
/// its message exactly once; E2 the unrelated marker projects nothing.
/// Decision rule R1(C1,C2)->E1+E2. The reschedule case above supplies !C1 and
/// proves the marker is absent without adding a Step-local observation path.
#[tokio::test]
async fn a_compacted_run_projects_the_compaction_marker() {
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Coverage rationale: `a compacted run` is one independent branch selecting `projects the
    // compaction marker`; a multi-row decision table is not applicable, and sibling tests own
    // alternate causes.
    let runtime = ScriptFake::new(|| ended(vec![assistant_text("a", "after compaction")]))
        .with_committed_evidence(CommittedEvidence::Compaction);
    let (state, app) = app_with_runtime(runtime);
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    reconcile_published_run(&state, &id).await;
    let list = list_events(&app, &id).await;
    assert_eq!(
        types(&list),
        vec![
            "user.message",
            "session.status_running",
            "session.thread_status_running",
            "agent.thread_context_compacted",
            "agent.message",
            "session.thread_status_idle",
            "session.usage",
            "session.status_idle"
        ]
    );
    // Exactly one marker (emit-once upstream).
    assert_eq!(
        types(&list)
            .iter()
            .filter(|t| *t == "agent.thread_context_compacted")
            .count(),
        1
    );
}

// --- CE: usage accounting ----------------------------------------------------

/// Cause/effect graph: C1 a root User Event is durably retained; C2 the same
/// primary Run has a committed recovery snapshot plus canonical Running→Ended
/// lifecycle facts; C3 Runtime cumulative usage contains token/cache counters;
/// C4 the warm projection is read again. Effects: E1 the Session view maps each
/// neutral counter without transposition; E2 the committed terminal prefix emits
/// one matching `session.usage`; E3 C4 is stable and adds no second usage event.
/// The `Terminal` fixture supplies C2 only through production snapshot/lifecycle
/// ports; its scripted `StepOutcome` is not a projection or completion flag.
///
/// | Rule | C1 | C2 | C3 | C4 | Effects |
/// |---|---|---|---|---|---|
/// | U1 | T | T | T | F | E1,E2 |
/// | U2 | T | T | T | T | E1,E2,E3 |
#[tokio::test]
async fn session_usage_reflects_the_runtime_tally() {
    // Causes: the fixtures below establish `session usage reflects the runtime tally` with the
    // concrete inputs, state, dependencies, and failure triggers used by this case.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let usage = SessionUsage {
        input_tokens: 120,
        output_tokens: 45,
        cache_read_tokens: 30,
        cache_creation_tokens: 12,
        ..Default::default()
    };
    let (state, app) = app_with_runtime(
        ScriptFake::new(|| ended(vec![assistant_text("a", "hi")]))
            .with_usage(usage)
            .with_committed_evidence(CommittedEvidence::Terminal),
    );
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    reconcile_published_run(&state, &id).await;
    let after = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(after["usage"]["input_tokens"], 120, "U1/E1");
    assert_eq!(after["usage"]["output_tokens"], 45, "U1/E1");
    // The neutral cache fields map onto the official nested cache-creation shape.
    assert_eq!(after["usage"]["cache_read_input_tokens"], 30, "U1/E1");
    assert_eq!(
        after["usage"]["cache_creation"]["ephemeral_5m_input_tokens"], 12,
        "U1/E1"
    );
    let replay = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(replay["usage"], after["usage"], "U2/E3");
    let events = list_events(&app, &id).await;
    let usage_events = events["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["type"] == "session.usage")
        .collect::<Vec<_>>();
    assert_eq!(usage_events.len(), 1, "U1-U2/E2-E3");
    let usage_event = usage_events[0];
    assert_eq!(usage_event["usage"]["input_tokens"], 120, "U1/E2");
    assert_eq!(usage_event["usage"]["output_tokens"], 45, "U1/E2");
    assert!(usage_event.get("budget").is_none());
}

#[tokio::test]
async fn session_usage_event_keeps_zero_counters_instead_of_omitting_the_snapshot() {
    // Causes: the fixtures below establish `session usage event` with the concrete inputs, state,
    // dependencies, and failure triggers used by this case.
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // Cause/effect graph: C1 a root User Event and its terminal Run prefix are
    // committed through the canonical snapshot/lifecycle ports; C2 every
    // metered counter is zero; C3 the Session has no frozen budget snapshot.
    // Effects: E1 emit one point-in-time `session.usage` with explicit zero
    // counters; E2 omit budget and list cost rather than suppressing the event or
    // fabricating a price. Decision rule Z1=C1+C2+C3=>E1+E2. The fixture's
    // `Terminal` evidence is the only completion authority.
    let (state, app) = app_with_runtime(
        ScriptFake::new(|| ended(vec![assistant_text("a", "unmetered")]))
            .with_committed_evidence(CommittedEvidence::Terminal),
    );
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    reconcile_published_run(&state, &id).await;
    let events = list_events(&app, &id).await;
    let usage = events["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["type"] == "session.usage")
        .unwrap();
    for field in [
        "active_seconds",
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
    ] {
        assert_eq!(usage["usage"][field], 0, "Z1/E1 {field}");
    }
    assert!(usage.get("budget").is_none(), "Z1/E2");
    assert!(usage["usage"].get("list_cost").is_none(), "Z1/E2");
}

// --- CE: all-empty-text assistant message dropped ----------------------------

/// Causes: C1 a root User Event is retained; C2 the same primary Run commits one
/// assistant message containing only empty text; C3 its Running→Ended lifecycle
/// is committed. Effects: E1 the User Event and lifecycle bracket remain; E2 the
/// empty output produces no `agent.message`; E3 usage and aggregate Idle still
/// follow the terminal prefix. Decision rule R1=C1+C2+C3=>E1+E2+E3.
#[tokio::test]
async fn an_all_empty_text_assistant_message_is_dropped() {
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let (state, app) = app_with_runtime(
        ScriptFake::new(|| ended(vec![assistant_text("a", "")]))
            .with_committed_evidence(CommittedEvidence::Terminal),
    );
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    reconcile_published_run(&state, &id).await;
    let list = list_events(&app, &id).await;
    assert_eq!(
        types(&list),
        vec![
            "user.message",
            "session.status_running",
            "session.thread_status_running",
            "session.thread_status_idle",
            "session.usage",
            "session.status_idle"
        ],
        "an empty assistant message projects no agent.message"
    );
    assert!(
        !types(&list).iter().any(|t| t == "agent.message"),
        "no useless empty agent.message event"
    );
}

// --- CE: MCP tool call → distinct mcp events ---------------------------------

/// Causes: C1 a root User Event is retained; C2 the same terminal Run commits a
/// host-executed MCP call with a batch-local Runtime call id; C3 its result
/// references that raw id. Effects: E1 Managed publishes a stable,
/// Thread/message-qualified public id; E2 the result references that same public
/// id; E3 the distinct MCP event shapes and server name remain intact; E4 the
/// call/result are bracketed by the committed lifecycle rather than Step-local
/// completion state.
///
/// | Rule | MCP call | Matching result | Effects |
/// |---|---|---|---|
/// | M1 | present | present | E1,E2,E3,E4 |
#[tokio::test]
async fn an_mcp_tool_call_projects_mcp_events() {
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let (state, app) = app_with_runtime(
        ScriptFake::new(|| {
            ended(vec![
                Message::new(
                    Id("a".into()),
                    Role::Assistant,
                    vec![
                        ContentBlock::text("searching"),
                        ContentBlock::ToolUse {
                            id: "mc1".into(),
                            name: "mcp__github__search".into(),
                            input: serde_json::json!({ "q": "rust" }),
                        },
                    ],
                ),
                Message::new(
                    Id("t".into()),
                    Role::Tool,
                    vec![ContentBlock::ToolResult {
                        tool_use_id: "mc1".into(),
                        content: vec![ContentBlock::text("3 hits")],
                        is_error: false,
                    }],
                ),
            ])
        })
        .with_committed_evidence(CommittedEvidence::Terminal),
    );
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    reconcile_published_run(&state, &id).await;
    let list = list_events(&app, &id).await;
    assert_eq!(
        types(&list),
        vec![
            "user.message",
            "session.status_running",
            "session.thread_status_running",
            "agent.message",
            "agent.mcp_tool_use",
            "agent.mcp_tool_result",
            "session.thread_status_idle",
            "session.usage",
            "session.status_idle"
        ]
    );
    let use_ev = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "agent.mcp_tool_use")
        .unwrap();
    let public_id = use_ev["id"].as_str().unwrap();
    assert_ne!(public_id, "mc1", "M1/E1 hides the batch-local raw id");
    assert_eq!(use_ev["mcp_server_name"], "github");
    assert_eq!(use_ev["evaluated_permission"], "allow");
    let res_ev = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "agent.mcp_tool_result")
        .unwrap();
    assert_eq!(res_ev["mcp_tool_use_id"], public_id, "M1/E2");
}

// --- CE: retries_exhausted idle stop reason ----------------------------------

/// Causes: C1 a root User Event is retained; C2 the same primary Run commits an
/// `EndCause::MaxSteps` terminal prefix. Effects: E1 the public Session reaches
/// Idle exactly once; E2 its stop reason is `retries_exhausted`, distinct from
/// the fixed wire values `end_turn` and `requires_action`. Decision rule
/// R1=C1+C2=>E1+E2.
#[tokio::test]
async fn a_retries_exhausted_run_idles_with_that_stop_reason() {
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Coverage rationale: `a retries exhausted run idles with that stop reason` is one independent
    // branch selecting `all output, state, side-effect, error, and terminal assertions below hold
    // together`; a multi-row decision table is not applicable, and sibling tests own alternate
    // causes.
    let (state, app) = app_with_runtime(
        ScriptFake::new(|| {
            StepOutcome::ended(vec![assistant_text("a", "gave up")], EndCause::MaxSteps)
        })
        .with_committed_evidence(CommittedEvidence::Terminal),
    );
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    reconcile_published_run(&state, &id).await;
    let list = list_events(&app, &id).await;
    let idle = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "session.status_idle")
        .unwrap();
    assert_eq!(idle["stop_reason"]["type"], "retries_exhausted");
}

// --- CE: real coordinated child Thread / multi-Run lifecycle -----------------

/// Causes: C0 the fake synchronously commits each primary terminal Run after
/// reporting its reservation `Activated`; C1 the canonical lifecycle supervisor
/// observes that terminal prefix and the Runtime Host settles its retained
/// Session activity epoch; C2 first accepted
/// `send_to_agent` creates a real stable Thread; C3 its ordinary child Run
/// completes with private Thinking plus public Text; C4 a follow-up targets that
/// Thread and creates a second Run; C5 warm refresh repeats. Effects: E0 each
/// aggregate lifecycle closes after canonical recovery rather than an unrelated
/// inbound Event; E1 one Thread only;
/// E2 parent sees created→running→sent→received→idle while the child sees
/// running→received→sent→idle (creation is written only to the parent output
/// stream); the terminal report is never duplicated as thinking/message, and no
/// Thinking content block crosses either message direction; E3 the
/// follow-up reopens then idles the same Thread; E4 primary/child streams stay
/// isolated; E5 Session usage is root+child exactly once; E6 the primary Run
/// has its own Running→Idle Thread bracket around the primary-owned output, and
/// its later terminal cursor closes only after the child terminal cursor.
///
/// | Rule | First call | Recovery drive | Follow-up | Replay | Effects |
/// |---|---|---|---|---|---|
/// | M1 | accepted+complete | yes | no | yes | E0,E1,E2,E4,E5,E6 |
/// | M2 | already idle | yes | accepted+complete | yes | E0,E1,E3,E4,E5,E6 |
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delegation_projects_the_child_thread_lifecycle() {
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let state = Arc::new(ManagedState::new(CoordinatedRuntimeFake::default()));
    let app = router(state.clone());
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    reconcile_published_run(&state, &id).await;
    let list = list_events(&app, &id).await;
    assert_eq!(
        types(&list),
        vec![
            "user.message",
            "session.status_running",
            "session.thread_status_running",
            "agent.message",
            "agent.tool_use",
            "agent.tool_result",
            "session.thread_created",
            "session.thread_status_running",
            "agent.thread_message_sent",
            "agent.thread_message_received",
            "session.thread_status_idle",
            "session.thread_status_idle",
            "session.usage",
            "session.status_idle",
        ]
    );
    let created = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "session.thread_created")
        .unwrap();
    assert_eq!(created["agent_name"], "researcher");
    let child_thread_id = created["session_thread_id"].as_str().unwrap().to_string();
    assert_eq!(child_thread_id, CoordinatedRuntimeFake::CHILD_THREAD_ID);
    // The input the coordinator sent and the reply it received are carried on the wire.
    let sent = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "agent.thread_message_sent")
        .unwrap();
    assert_eq!(sent["content"][0]["text"], "find the docs");
    let recv = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "agent.thread_message_received")
        .unwrap();
    assert_eq!(recv["content"][0]["text"], "here are the docs");
    assert!(
        recv["content"]
            .as_array()
            .unwrap()
            .iter()
            .all(|block| block["type"] != "thinking"),
        "M1/E2 provider reasoning never enters the Managed cross-Thread union"
    );

    // GET /threads enumerates the primary plus the child, parented to the primary.
    let threads = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/threads"),
        serde_json::Value::Null,
    )
    .await;
    let arr = threads["data"].as_array().unwrap();
    assert_eq!(arr.len(), 2, "M1/E1 primary + one real child Thread");
    let primary_thread_id = arr
        .iter()
        .find(|thread| thread["parent_thread_id"].is_null())
        .and_then(|thread| thread["id"].as_str())
        .expect("M1/E1 primary Thread");
    // Public-ID integration rule: the internal root key is the Session id, but
    // the Thread DTO, parent link, and cross-Thread endpoints must all reuse one
    // stable public `sthr_` projection. This assertion complements the codec's
    // R1-R4 table at the full event/Thread projection boundary.
    assert!(primary_thread_id.starts_with("sthr_"), "M1/E1");
    assert_ne!(primary_thread_id, id, "M1/E1 internal root must not leak");
    assert!(!primary_thread_id.contains(":primary"), "M1/E1");
    let child = arr
        .iter()
        .find(|t| t["id"] == child_thread_id.as_str())
        .unwrap();
    assert_eq!(child["parent_thread_id"], primary_thread_id);
    assert_eq!(child["agent"]["name"], "researcher");
    let status_types_for = |thread_id: &str| {
        list["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["session_thread_id"] == thread_id)
            .filter_map(|event| event["type"].as_str())
            .filter(|kind| kind.starts_with("session.thread_status_"))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        status_types_for(primary_thread_id),
        vec![
            "session.thread_status_running",
            "session.thread_status_idle"
        ],
        "M1/E6 primary lifecycle uses the listed public Thread id"
    );
    assert_eq!(
        status_types_for(&child_thread_id),
        vec![
            "session.thread_status_running",
            "session.thread_status_idle"
        ],
        "M1/E2 child lifecycle remains isolated"
    );

    // The child endpoint is not an alias of the Session log. It projects the
    // same committed facts from the child's perspective: coordinator input is a
    // received message and the child's reply is a sent message.
    let child_events = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/threads/{child_thread_id}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        types(&child_events),
        vec![
            "session.thread_status_running",
            "agent.thread_message_received",
            "agent.thread_message_sent",
            "session.thread_status_idle",
        ],
        "child stream contains only child-owned and cross-posted facts"
    );
    let received = &child_events["data"][1];
    assert_eq!(received["from_session_thread_id"], primary_thread_id);
    assert!(received.get("from_agent_name").is_none());
    assert_eq!(received["content"][0]["text"], "find the docs");
    let sent = &child_events["data"][2];
    assert_eq!(sent["to_session_thread_id"], primary_thread_id);
    assert!(sent.get("to_agent_name").is_none());
    assert_eq!(sent["content"][0]["text"], "here are the docs");

    send_user(&app, &id, "follow up").await;
    reconcile_published_run(&state, &id).await;
    let threads = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/threads"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(threads["data"].as_array().unwrap().len(), 2, "M2/E1");
    let list = list_events(&app, &id).await;
    assert_eq!(
        list["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["type"] == "session.thread_created")
            .count(),
        1,
        "M2/E1 follow-up reuses the Thread"
    );
    assert_eq!(
        list["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["type"] == "agent.thread_message_sent")
            .count(),
        2,
        "M2/E3 each accepted coordination call cross-posts once"
    );
    let session = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(session["usage"]["input_tokens"], 15, "M1-M2/E5");
}

// --- CE: session.updated event -----------------------------------------------

/// Causes: C1 one title+metadata patch targets an active Session. Effects: E1
/// the mutation commits one `session.updated`; E2 its payload carries the new
/// title and full metadata bag so listing clients observe the same mutation as
/// the Session view. Decision rule U1=C1=>E1+E2. Constraint K1: the Session
/// aggregate/repository remains the mutation authority; this event is its
/// projection and owns no second title or metadata state.
#[tokio::test]
async fn updating_a_session_commits_a_session_updated_event() {
    let app = router(Arc::new(ManagedState::new(ScriptFake::new(|| {
        ended(vec![assistant_text("a", "hi")])
    }))));
    let id = create(&app).await;
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}"),
        serde_json::json!({ "title": "renamed", "metadata": { "team": "research" } }),
    )
    .await;
    let list = list_events(&app, &id).await;
    let updated = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "session.updated")
        .unwrap();
    assert_eq!(updated["title"], "renamed");
    assert_eq!(updated["metadata"]["team"], "research");
}

// --- CE: archive commits the terminal event + makes the session read-only ----

/// Causes: C1 an active Session is archived; C2 a write follows that terminal
/// transition; C3 the archive command is replayed. Effects: E1 one
/// `session.status_terminated` is committed; E2 C2 fails with the public 409
/// read-only error; E3 C3 returns the same terminal state without another event.
/// Decision rules: A1=C1=>E1; A2=C1+C2=>E1+E2; A3=C1+C3=>E1+E3.
/// Constraint K1: the archived Session aggregate is the terminal authority;
/// projection and replay may neither reopen it nor mint another terminal fact.
#[tokio::test]
async fn archiving_commits_a_terminal_event_and_fences_writes() {
    let app = router(Arc::new(ManagedState::new(ScriptFake::new(|| {
        ended(vec![assistant_text("a", "hi")])
    }))));
    let id = create(&app).await;

    let archived = json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/archive"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(archived["status"], "terminated");
    assert!(archived["archived_at"].is_string());

    let list = list_events(&app, &id).await;
    assert_eq!(
        types(&list)
            .iter()
            .filter(|t| *t == "session.status_terminated")
            .count(),
        1,
        "one committed terminal event"
    );

    // A write to the archived session is 409 read-only, in the error envelope.
    let (status, body) = raw_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "again" }] }] }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["type"], "invalid_request_error");

    // Re-archive is idempotent: still terminated, and no second terminal event.
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/archive"),
        serde_json::Value::Null,
    )
    .await;
    let list = list_events(&app, &id).await;
    assert_eq!(
        types(&list)
            .iter()
            .filter(|t| *t == "session.status_terminated")
            .count(),
        1,
        "re-archive commits no second terminal event"
    );
}
