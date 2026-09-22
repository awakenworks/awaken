//! Cause-effect / decision-table coverage for the event → wire projection and the
//! session-lifecycle accounting that the happy-path `adapter.rs` suite leaves
//! uncovered: a terminal run *fault* projected as `session.error` (never swallowed
//! into a success idle), the compaction marker, cumulative usage accounting, an
//! all-empty-text assistant message dropped (matching the shared agent-contract
//! projection), the MCP tool-call events, the `retries_exhausted` idle, the
//! subagent-delegate child-thread lifecycle, the `session.updated` event, and the
//! archived-session read-only fence + committed terminal event.

mod support;

use awaken_session_contract::ThreadReadRuntime;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, Failure, Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_protocol_managed::ManagedState;
use awaken_protocol_managed::test_support::CoordinatedRuntimeFake;

/// The Managed router as a real deployment mounts it — behind the trusted edge
/// stamp that the composition root installs. Handlers are fail-closed on an
/// absent Workspace, so an unstamped router is not a representative fixture.
fn router(state: std::sync::Arc<ManagedState>) -> axum::Router {
    support::edge_stamped(awaken_protocol_managed::router(state))
}
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
) -> support::Call<serde_json::Value> {
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
    support::Call::new(status, json)
}

async fn json_call(
    app: &Router,
    method: &str,
    uri: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    Box::pin(raw_call(app, method, uri, body))
        .await
        .expect_for(StatusCode::OK, &format!("{method} {uri}"))
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

type CompletedDispatches = Arc<Mutex<Vec<awaken_session_contract::SessionAgentBoundaryCommand>>>;

fn app_with_runtime(runtime: ScriptFake) -> (Arc<ManagedState>, Router, CompletedDispatches) {
    let completions = runtime.completions.clone();
    let state = Arc::new(ManagedState::new(runtime));
    let app = router(state.clone());
    (state, app, completions)
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
    completions: CompletedDispatches,
    make: Box<dyn Fn() -> StepOutcome + Send + Sync>,
    usage: SessionUsage,
    evidence_profile: Option<CommittedEvidence>,
    durable: Mutex<ScriptDurableState>,
}

struct CommittedRunEvidence {
    snapshot: awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
}

#[derive(Default)]
struct ScriptDurableState {
    reservations: HashMap<String, awaken_session_contract::AdmitSessionRun>,
    run_states: HashMap<String, RunState>,
    committed_run: Option<CommittedRunEvidence>,
}

#[derive(Debug, Clone, Copy)]
enum CommittedEvidence {
    Terminal,
    Compaction,
    Reschedule,
    /// A terminal run whose committed audit also records one permission
    /// decision, as the exact label the gate wrote for that exact call id.
    PermissionDecision {
        call_id: &'static str,
        decision: &'static str,
    },
    /// A terminal run whose committed audit records one permission decision
    /// per row, each as the raw payload the audit table holds: the decision
    /// label plus the optional `policy` / `reason_code` labels. Raw on
    /// purpose, so a historical row (no policy), a policy outside the closed
    /// vocabulary and a reason code outside the named registry are all
    /// representable exactly as they would be on disk.
    PermissionDecisions(&'static [AuditedPermissionRow]),
}

/// One raw `PermissionDecided` audit payload:
/// `(call_id, decision, decided_by, policy, reason_code)`.
type AuditedPermissionRow = (
    &'static str,
    &'static str,
    Option<&'static str>,
    Option<&'static str>,
    Option<&'static str>,
);

fn permission_decided_payload(row: AuditedPermissionRow) -> serde_json::Value {
    let (call_id, decision, decided_by, policy, reason_code) = row;
    let mut payload = serde_json::json!({
        "tool_id": "bash",
        "call_id": call_id,
        "decision": decision,
    });
    if let Some(decided_by) = decided_by {
        payload["decided_by"] = serde_json::Value::String(decided_by.into());
    }
    if let Some(policy) = policy {
        payload["policy"] = serde_json::Value::String(policy.into());
    }
    if let Some(reason_code) = reason_code {
        payload["reason_code"] = serde_json::Value::String(reason_code.into());
    }
    payload
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
                await_target: None,
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
        if let Self::PermissionDecision { call_id, decision } = self {
            events.push((
                2,
                awaken_agent_contract::audit::run_event::RunEvent::PermissionDecided {
                    tool_id: "bash".into(),
                    call_id: call_id.into(),
                    decision: decision.into(),
                    decided_by:
                        awaken_agent_contract::audit::permission::PermissionDecider::PluginGate,
                    policy: None,
                    reason_code: None,
                },
            ));
        }
        events.push((
            3,
            awaken_agent_contract::audit::run_event::RunEvent::RunStateChanged {
                state: terminal_state.clone(),
                await_reason: None,
                await_target: None,
            },
        ));
        let mut records = events
            .into_iter()
            .map(|(sequence, event)| {
                let draft: awaken_agent_contract::audit::draft::Draft = event.into();
                awaken_agent_contract::audit::record::Record {
                    sequence: awaken_agent_contract::encode_run_lifecycle_cursor(sequence, 0)
                        .unwrap()
                        .0,
                    run_id: run_id.clone(),
                    kind: draft.kind,
                    payload: draft.payload,
                }
            })
            .collect::<Vec<_>>();
        if let Self::PermissionDecisions(rows) = self {
            // Raw rows sit between the Running and terminal facts, exactly
            // where production commits a gate decision.
            let terminal = records.pop().expect("the terminal fact is last");
            records.extend(rows.iter().enumerate().map(|(offset, row)| {
                awaken_agent_contract::audit::record::Record {
                    sequence: awaken_agent_contract::encode_run_lifecycle_cursor(
                        2 + offset as u64,
                        0,
                    )
                    .unwrap()
                    .0,
                    run_id: run_id.clone(),
                    kind: awaken_agent_contract::audit::kind::Kind::PermissionDecided,
                    payload: permission_decided_payload(*row),
                }
            }));
            let mut terminal = terminal;
            terminal.sequence =
                awaken_agent_contract::encode_run_lifecycle_cursor(2 + rows.len() as u64, 0)
                    .unwrap()
                    .0;
            records.push(terminal);
        }
        records
    }

    fn snapshot(
        self,
        thread_id: &str,
        run_id: RunId,
        command: &awaken_session_contract::AdmitSessionRun,
        outcome: StepOutcome,
        usage: &SessionUsage,
    ) -> awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
        let terminal_state = outcome.state().clone();
        // The Thread commit owns the exact input Message and its coordinate even
        // though Managed reconstructs the public DTO from Session-root
        // provenance. This keeps the fake faithful to production without using
        // the transcript as the public payload authority.
        let events = self.audit_records(&run_id, &terminal_state);
        // The terminal fact is the last audit record; the Thread commit that
        // carried it (and the Step's new Messages) sits at that same cursor.
        let terminal_cursor = events
            .last()
            .map(|record| {
                awaken_agent_contract::decode_run_lifecycle_cursor(
                    awaken_agent_contract::RunLifecycleCursor(record.sequence),
                )
                .0
            })
            .expect("the fixture always commits a terminal fact");
        let mut messages = command.messages.clone();
        let mut message_commit_cursors = vec![1; messages.len()];
        message_commit_cursors.extend(std::iter::repeat_n(
            terminal_cursor,
            outcome.new_messages.len(),
        ));
        messages.extend(outcome.new_messages);
        let mut state = if matches!(self, Self::Compaction) {
            vec![
                awaken_runtime_contract::compaction::RunCompactionMarker::command("other-run"),
                awaken_runtime_contract::compaction::RunCompactionMarker::command(&run_id.0),
            ]
        } else {
            Vec::new()
        };
        // C3 usage belongs to the same terminal commit as messages/lifecycle.
        // Both Budget and recovery observations must describe this one tally;
        // keeping it only on the old Budget port would contradict Host truth.
        use awaken_agent_contract::agent::state::StateKey as _;
        let mut tally = awaken_runtime_contract::llm::ThreadUsage::default();
        tally.record(
            "test-model",
            awaken_runtime_contract::llm::TokenUsage {
                prompt_tokens: usage.input_tokens,
                completion_tokens: usage.output_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                cache_creation_tokens: usage.cache_creation_tokens,
            },
        );
        state.push(awaken_runtime_contract::llm::ThreadUsageKey::write(&tally));
        let state_commit_cursors = vec![terminal_cursor; state.len()];
        awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
            commit_timestamps: Default::default(),
            thread_id: ThreadId(thread_id.into()),
            claimed_run_id: run_id.clone(),
            runs: vec![awaken_agent_contract::agent::run::Record {
                id: run_id.clone(),
                thread_id: ThreadId(thread_id.into()),
                state: terminal_state,
            }],
            latest_run_id: Some(run_id),
            messages,
            message_commit_cursors,
            state,
            state_commit_cursors,
            events,
            resume_tickets: Vec::new(),
            thread_version: terminal_cursor,
            store_cursor: terminal_cursor,
            next_commit_ordinal: 1,
        }
    }
}

impl ScriptFake {
    fn new(make: impl Fn() -> StepOutcome + Send + Sync + 'static) -> Self {
        Self {
            completions: Default::default(),
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
    fn model(&self) -> String {
        "test-model".into()
    }

    fn realization(&self) -> Option<&dyn awaken_session_contract::RealizationRuntime> {
        Some(self)
    }

    fn coordination(&self) -> Option<&dyn awaken_session_contract::CoordinationRuntime> {
        Some(self)
    }

    fn budget(&self) -> Option<&dyn awaken_session_contract::BudgetRuntime> {
        Some(self)
    }

    fn thread_reads(&self) -> Option<&dyn awaken_session_contract::ThreadReadRuntime> {
        Some(self)
    }

    fn terminal_cleanup(&self) -> Option<&dyn awaken_session_contract::TerminalCleanupRuntime> {
        Some(self)
    }

    fn outcomes(&self) -> Option<&dyn awaken_session_contract::OutcomeRuntime> {
        Some(self)
    }

    fn streaming_runs(&self) -> Option<&dyn awaken_session_contract::StreamingRunRuntime> {
        Some(self)
    }

    fn environment(&self) -> Option<&dyn awaken_session_contract::EnvironmentRuntime> {
        Some(self)
    }

    fn live_inbox(&self) -> Option<&dyn awaken_session_contract::LiveInboxRuntime> {
        Some(self)
    }

    fn capability(&self) -> Option<&dyn awaken_session_contract::CapabilityRuntime> {
        Some(self)
    }
}

awaken_session_contract::neutral_role!(StreamingRunRuntime for ScriptFake);

awaken_session_contract::neutral_role!(EnvironmentRuntime for ScriptFake);

awaken_session_contract::neutral_role!(LiveInboxRuntime for ScriptFake);

awaken_session_contract::neutral_role!(CapabilityRuntime for ScriptFake);

#[async_trait::async_trait]
impl awaken_session_contract::RealizationRuntime for ScriptFake {
    fn observe_realization_request(
        &self,
    ) -> Result<
        awaken_runtime_contract::authority_lease::AuthorityProofStarted,
        awaken_session_contract::RunError,
    > {
        awaken_protocol_managed::test_support::observe_realization_request()
    }

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reconcile the existing Runtime projections through the given Control
    /// owner. Local and Worker deployment use one slot-based selection and
    /// physical-cutoff refresh algorithm; no durable Session rescan is implied.
    async fn renew_session_realization_projections(
        &self,
        control: &dyn awaken_session_contract::SessionRealizationControl,
        owner: &str,
        runtime_incarnation: &str,
        timing: awaken_runtime_contract::authority_lease::AuthorityLeaseTiming,
    ) -> Result<usize, awaken_session_contract::RunError> {
        awaken_session_contract::NeutralRuntime
            .renew_session_realization_projections(control, owner, runtime_incarnation, timing)
            .await
    }

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Validate the exact provider-effective Sandbox layout without publishing
    /// process-local state or performing provider, cache, Skill, File, Vault,
    /// Repository, or Git I/O. Session creation and Resource mutation call this
    /// before their durable/effect boundaries; Runtime repeats it at the final
    /// realization edge as defense in depth.
    fn validate_session_sandbox_layout(
        &self,
        thread: &str,
        layout: &awaken_session_contract::SessionSandboxLayout,
    ) -> Result<(), awaken_session_contract::RunError> {
        awaken_session_contract::NeutralRuntime.validate_session_sandbox_layout(thread, layout)
    }

    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_contract::FrozenSessionProjection,
        mode: awaken_session_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        awaken_protocol_managed::test_support::complete_session_projection_init(
            thread,
            &projection,
            &mode,
        )?;
        Ok(())
    }

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Install a monotonic same-owner/same-incarnation/same-epoch successor of
    /// an already-installed realization fence. This is the only lease-only
    /// Runtime mutation; it deliberately carries no projection or phase action.
    async fn renew_session_realization_lease(
        &self,
        thread: &str,
        lease: awaken_session_contract::ObservedSessionRealizationLease,
    ) -> Result<(), awaken_session_contract::RunError> {
        awaken_session_contract::NeutralRuntime
            .renew_session_realization_lease(thread, lease)
            .await
    }

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Forget one failed realization fence from disposable Runtime state. The
    /// aggregate remains the sole lease authority: this hook only prevents a
    /// later retry from authorizing effects with a lease that Control has
    /// already failed or may have failed before losing its response.
    async fn invalidate_session_realization_lease(
        &self,
        thread: &str,
        lease: &awaken_session_contract::SessionRealizationLease,
    ) -> Result<(), awaken_session_contract::RunError> {
        awaken_session_contract::NeutralRuntime
            .invalidate_session_realization_lease(thread, lease)
            .await
    }

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Install the durable callback used at the exact sandbox creation boundary.
    fn install_environment_binding_sink(
        &self,
        sink: std::sync::Arc<dyn awaken_session_contract::SessionEnvironmentBindingSink>,
    ) {
        awaken_session_contract::NeutralRuntime.install_environment_binding_sink(sink)
    }
}

#[async_trait::async_trait]
impl awaken_session_contract::CoordinationRuntime for ScriptFake {
    async fn reserve_session_run(
        &self,
        command: awaken_session_contract::AdmitSessionRun,
    ) -> Result<awaken_session_contract::SessionRunReservation, awaken_session_contract::RunError>
    {
        let mut durable = self.durable.lock().unwrap();
        if durable.run_states.contains_key(&command.run_id.0) {
            return Ok(awaken_session_contract::SessionRunReservation::Completed);
        }
        if let Some(existing) = durable.reservations.get(&command.run_id.0) {
            if existing != &command {
                return Err(RunError::bad_request(
                    "scripted Run id was reused with different input",
                ));
            }
            return Ok(awaken_session_contract::SessionRunReservation::AlreadyReserved);
        }
        durable
            .reservations
            .insert(command.run_id.0.clone(), command);
        Ok(awaken_session_contract::SessionRunReservation::Reserved)
    }

    async fn activate_session_run(
        &self,
        delivery: awaken_session_contract::SessionRunDelivery,
    ) -> Result<awaken_session_contract::SessionRunActivation, RunError> {
        let mut durable = self.durable.lock().unwrap();
        if durable.run_states.contains_key(&delivery.run_id.0) {
            return Ok(awaken_session_contract::SessionRunActivation::Completed);
        }
        let command = durable
            .reservations
            .get(&delivery.run_id.0)
            .cloned()
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
                    delivery.session_id.as_str(),
                    delivery.run_id.clone(),
                    &command,
                    outcome.clone(),
                    &self.usage,
                ),
            });
        }
        self.completions.lock().unwrap().push(
            awaken_session_contract::SessionAgentBoundaryCommand {
                session_id: delivery.session_id.clone(),
                source_thread_id: ThreadId(delivery.session_id.into_string()),
                source_run_id: delivery.run_id.clone(),
                source_agent_id: command.agent_id,
                session_activity_epoch: delivery.session_activity_epoch,
                cancellation_requested: false,
            },
        );
        durable
            .run_states
            .insert(delivery.run_id.0, outcome.state().clone());
        Ok(awaken_session_contract::SessionRunActivation::Activated)
    }

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Register a foreground observer before publishing a delivery, then wait
    /// for the exact committed Run to become `Awaiting` or `Ended`.
    ///
    /// The admission plan is produced only after the Session application has
    /// committed its exact activity receipt. Recovery-only plans still wait on
    /// committed Thread truth and never create another reservation/activity.
    /// Public queued Event acceptance has its own durable command boundary and
    /// does not wait through this foreground observation port.
    async fn activate_and_observe_session_run(
        &self,
        admission: awaken_session_contract::AdmittedSessionRun,
        input_message_ids: Vec<String>,
        sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<awaken_session_contract::StepOutcome, awaken_session_contract::RunError> {
        awaken_session_contract::NeutralRuntime
            .activate_and_observe_session_run(admission, input_message_ids, sink)
            .await
    }

    async fn session_run_state(
        &self,
        _session_id: &awaken_session_contract::SessionId,
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

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read the runtime-owned, durable child-Run relationships for `thread`.
    /// Protocol adapters use this only to rebuild disposable projections after a
    /// restart; the runtime relationship registry remains the sole authority.
    async fn delegated_runs(
        &self,
        thread: &str,
    ) -> Result<Vec<awaken_session_contract::DelegatedRun>, awaken_session_contract::RunError> {
        awaken_session_contract::NeutralRuntime
            .delegated_runs(thread)
            .await
    }

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Admit one ordinary child Run after the Session application has validated
    /// the frozen roster and lifecycle. Implementations must use their canonical
    /// durable Run ingress; the default fails closed rather than spawning a
    /// process-local task.
    async fn admit_coordinated_run(
        &self,
        command: awaken_session_contract::CoordinatedRunCommand,
    ) -> Result<
        awaken_session_contract::SessionAgentMessageReceipt,
        awaken_session_contract::RunError,
    > {
        awaken_session_contract::NeutralRuntime
            .admit_coordinated_run(command)
            .await
    }

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Rebuild the Session's coordinated child-Thread links from existing
    /// committed ToolBatch/transcript/dispatch facts. This is a query, not a
    /// relationship repository.
    /// `observed_root` may borrow an atomic prefix already read for this Session
    /// during a larger read-only projection. Implementations must reject a
    /// different root identity and fence enrichment to that prefix. `None`
    /// reads the current prefix; command admission and final validation use it.
    async fn coordinated_threads(
        &self,
        session_id: &awaken_session_contract::SessionId,
        observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<
        Vec<awaken_session_contract::CoordinatedThreadLink>,
        awaken_session_contract::RunError,
    > {
        awaken_session_contract::NeutralRuntime
            .coordinated_threads(session_id, observed_root)
            .await
    }

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Subscribe to neutral live observations for one already-admitted root or
    /// child logical Thread. `None` is the fail-closed default: committed events
    /// remain available, but no Runtime without an exact Thread-scoped observer
    /// can accidentally leak previews across Threads. This port is a view over
    /// the Runtime's existing live hub, not another fan-out or replay owner.
    async fn subscribe_session_thread_live(
        &self,
        session_id: &awaken_session_contract::SessionId,
        thread_id: &str,
    ) -> Result<
        Option<Box<dyn awaken_session_contract::SessionThreadLiveSubscription>>,
        awaken_session_contract::RunError,
    > {
        awaken_session_contract::NeutralRuntime
            .subscribe_session_thread_live(session_id, thread_id)
            .await
    }

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Admit one Session-approved child report as immutable input of the
    /// deterministic primary Run. Implementations must not stage a parallel
    /// Outbox/Inbox message for this internal Agent-to-Agent transfer.
    async fn continue_session_agent_report(
        &self,
        command: awaken_session_contract::SessionAgentReportContinuation,
    ) -> Result<(), awaken_session_contract::RunError> {
        awaken_session_contract::NeutralRuntime
            .continue_session_agent_report(command)
            .await
    }

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Interrupt one logical child through its parent Session's physical commit
    /// and dispatch partition. Implementations must not open child-named storage.
    async fn interrupt_session_thread(
        &self,
        session_id: &awaken_session_contract::SessionId,
        child_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<(), awaken_session_contract::RunError> {
        awaken_session_contract::NeutralRuntime
            .interrupt_session_thread(session_id, child_thread_id)
            .await
    }

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read and validate the exact committed Awaiting coordinate that a Primary
    /// or child Thread tool reply intends to answer. The command already carries
    /// its admission-time Run/correlation identity; this returns only the current
    /// dispatch activity coordinate. Implementations revalidate the command when
    /// applying the reply so a concurrent resume or later Run fails closed.
    async fn session_thread_tool_reply_fence(
        &self,
        command: &awaken_session_contract::SessionThreadToolReplyCommand,
    ) -> Result<
        awaken_session_contract::SessionThreadToolReplyFence,
        awaken_session_contract::RunError,
    > {
        awaken_session_contract::NeutralRuntime
            .session_thread_tool_reply_fence(command)
            .await
    }

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reply to the exact pending tool of a Primary or logical child Thread
    /// through the Session-affine claim-fenced resume/commit boundary.
    async fn reply_session_thread_tool(
        &self,
        delivery: awaken_session_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), awaken_session_contract::RunError> {
        awaken_session_contract::NeutralRuntime
            .reply_session_thread_tool(delivery)
            .await
    }

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Foreground form of [`Self::reply_session_thread_tool`]. The durable
    /// reply ingress remains identical; implementations additionally register
    /// before publication and project the next committed Step from Thread
    /// truth. No inline executor is authorized by this observation policy.
    async fn reply_and_observe_session_thread_tool(
        &self,
        delivery: awaken_session_contract::SessionThreadToolReplyDelivery,
    ) -> Result<awaken_session_contract::StepOutcome, awaken_session_contract::RunError> {
        awaken_session_contract::NeutralRuntime
            .reply_and_observe_session_thread_tool(delivery)
            .await
    }

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Stop the parent Run, wait until it can no longer commit a delegation, and
    /// then read the complete child set from durable runtime authority.
    async fn quiesce_terminal_delegations(
        &self,
        thread: &str,
    ) -> Result<awaken_session_contract::DelegatedRunSnapshot, awaken_session_contract::RunError>
    {
        self.streaming_runs()
            .ok_or_else(|| {
                awaken_session_contract::RunError::unavailable(
                    "StreamingRunRuntime is unsupported by this runtime",
                )
            })?
            .interrupt(thread)
            .await?;
        Ok(awaken_session_contract::DelegatedRunSnapshot {
            delegated_runs: self.delegated_runs(thread).await?,
            coordinated_thread_ids: Vec::new(),
            watermark: 0,
            runtime_commit_cursor: self
                .session_thread_recovery_snapshot(
                    &awaken_session_contract::SessionId::from(thread),
                    thread,
                )
                .await?
                .map(|snapshot| snapshot.store_cursor)
                .unwrap_or_default(),
        })
    }

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Validate one model-facing Agent command against the source Run's exact
    /// committed `ActiveToolBatch`. Implementations must use the same atomic
    /// Thread recovery prefix as ordinary crash recovery; a process-local tool
    /// context or authenticated Worker claim alone is not sufficient proof of
    /// call identity or payload. Success returns the command reconstructed from
    /// that committed prefix; Session policy consumes that value instead of
    /// trusting a second copy supplied by the caller. The fail-closed default
    /// keeps lightweight runtimes from silently becoming a second trusted
    /// command source.
    async fn validate_session_agent_message_source(
        &self,
        command: &awaken_session_contract::SessionAgentMessageCommand,
    ) -> Result<
        awaken_session_contract::SessionAgentMessageCommand,
        awaken_session_contract::RunError,
    > {
        awaken_session_contract::NeutralRuntime
            .validate_session_agent_message_source(command)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_contract::BudgetRuntime for ScriptFake {
    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Resume an exact `BudgetReached` ticket without injecting transcript
    /// content. Implementations reuse the foreground Run driver or the existing
    /// durable dispatch row; they must not enqueue a replacement Run.
    async fn resume_budget_reached(
        &self,
        delivery: awaken_session_contract::SessionBudgetResumeDelivery,
    ) -> Result<
        awaken_session_contract::SessionBudgetResumeDisposition,
        awaken_session_contract::RunError,
    > {
        awaken_session_contract::NeutralRuntime
            .resume_budget_reached(delivery)
            .await
    }

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Discover current budget-paused continuations from committed Run tickets
    /// and existing Session-affine dispatch rows. This is a recovery query over
    /// those authorities, never a pause registry.
    async fn session_budget_resume_tickets(
        &self,
        session_id: &awaken_session_contract::SessionId,
    ) -> Result<
        Vec<awaken_session_contract::SessionBudgetResumeTicket>,
        awaken_session_contract::RunError,
    > {
        awaken_session_contract::NeutralRuntime
            .session_budget_resume_tickets(session_id)
            .await
    }

    async fn session_usage(&self, _t: &str) -> Result<SessionUsage, RunError> {
        Ok(self.usage.clone())
    }

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read usage for one logical Thread from its parent Session partition.
    /// Global stores may use `thread_id` directly; filesystem/SQLite adapters
    /// must retain `session_id` as the physical read owner.
    async fn session_thread_usage(
        &self,
        _session_id: &awaken_session_contract::SessionId,
        thread_id: &str,
    ) -> Result<awaken_session_contract::SessionUsage, awaken_session_contract::RunError> {
        self.session_usage(thread_id).await
    }
}

#[async_trait::async_trait]
impl awaken_session_contract::ThreadReadRuntime for ScriptFake {
    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// True when durable truth already exists for `thread`. Session-id minting
    /// consults this to skip ids a previous process persisted; implementations
    /// MUST answer without materializing any per-thread state (no context
    /// build, no cache entry) — probing must be free of side effects. The
    /// default reports nothing, so an ephemeral host mints densely from 0.
    async fn owns_thread(&self, thread: &str) -> Result<bool, awaken_session_contract::RunError> {
        awaken_session_contract::NeutralRuntime
            .owns_thread(thread)
            .await
    }

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// The committed transcript for `thread`, in commit order. Used to rehydrate a
    /// session whose in-memory record was lost (e.g. after a process restart) from
    /// durable truth: a non-empty result means the thread exists in the store. The
    /// default reports nothing, so an ephemeral host never rehydrates.
    async fn committed_messages(
        &self,
        thread: &str,
    ) -> Result<
        Vec<awaken_agent_contract::agent::message::Message>,
        awaken_session_contract::RunError,
    > {
        awaken_session_contract::NeutralRuntime
            .committed_messages(thread)
            .await
    }

    async fn session_thread_recovery_snapshot(
        &self,
        session_id: &awaken_session_contract::SessionId,
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

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact committed Run through the same internally consistent
    /// logical-Thread prefix. Settlement already owns the immutable Run id from
    /// its guarded dispatch and must not rediscover it through a process-local
    /// "latest Run" projection.
    ///
    /// The compatibility default filters the existing Thread snapshot. Shared
    /// durable adapters override this method so a cold replica can query its
    /// authoritative [`RunRecoverySource`](awaken_agent_contract::thread::read::recovery::RunRecoverySource)
    /// directly by `run_id`.
    async fn session_thread_run_recovery_snapshot(
        &self,
        session_id: &awaken_session_contract::SessionId,
        thread_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<
        Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
        awaken_session_contract::RunError,
    > {
        let Some(mut snapshot) = self
            .session_thread_recovery_snapshot(session_id, thread_id)
            .await?
        else {
            return Ok(None);
        };
        if !snapshot.runs.iter().any(|run| &run.id == run_id) {
            return Ok(None);
        }
        snapshot.claimed_run_id = run_id.clone();
        Ok(Some(snapshot))
    }

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read the durable disposition of one logical child Thread through its
    /// parent Session's physical commit partition. Legacy Threads without the
    /// state cell remain Active.
    async fn session_thread_disposition(
        &self,
        session_id: &awaken_session_contract::SessionId,
        thread_id: &str,
    ) -> Result<awaken_agent_contract::ThreadDisposition, awaken_session_contract::RunError> {
        awaken_session_contract::NeutralRuntime
            .session_thread_disposition(session_id, thread_id)
            .await
    }

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Commit the absorbing archive disposition on one logical child Thread
    /// through its parent Session partition. Implementations must not mutate a
    /// protocol cache or open child-named storage.
    async fn archive_session_thread(
        &self,
        session_id: &awaken_session_contract::SessionId,
        thread_id: &str,
    ) -> Result<(), awaken_session_contract::RunError> {
        awaken_session_contract::NeutralRuntime
            .archive_session_thread(session_id, thread_id)
            .await
    }

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact terminal Outcome projection from committed Thread truth.
    /// `None` means the id is absent or still active. Implementations must not
    /// resume the Outcome or reconstruct a terminal from an in-process
    /// [`awaken_session_contract::StepOutcome`].
    async fn committed_outcome_projection(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<
        Option<awaken_session_contract::CommittedOutcomeProjection>,
        awaken_session_contract::RunError,
    > {
        awaken_session_contract::NeutralRuntime
            .committed_outcome_projection(thread, outcome_id)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_contract::TerminalCleanupRuntime for ScriptFake {
    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Execute terminal Repository publication under the same realization
    /// generation that owns the cleanup command set and return either its
    /// secret-free receipt or one permanent compare-and-swap rejection. The
    /// caller verifies and durably records that outcome before ordinary root
    /// cleanup may dispose the shared environment. Runtimes must override this
    /// edge and carry the lease to the Repository transport boundary; falling
    /// back would erase the terminal ownership proof.
    async fn execute_terminal_repository_publication_for_lease(
        &self,
        command: awaken_session_contract::SessionRepositoryPublicationCommand,
        lease: &awaken_session_contract::SessionRealizationLease,
    ) -> Result<
        awaken_session_contract::SessionRepositoryPublicationEffect,
        awaken_session_contract::RunError,
    > {
        awaken_session_contract::NeutralRuntime
            .execute_terminal_repository_publication_for_lease(command, lease)
            .await
    }

    async fn prepare_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_contract::SessionTerminalCleanupEffect,
        authorization: awaken_session_contract::SessionTerminalCleanupPreparationAuthorization,
    ) -> Result<awaken_session_contract::SessionCleanupPreparation, RunError> {
        awaken_protocol_managed::test_support::complete_terminal_cleanup_preparation(
            &effect,
            &authorization,
        )
    }

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Retire only a prepared child projection after Control durably accepts
    /// its exact preparation. The shared root Environment must remain until the
    /// aggregate-wide physical disposal is complete.
    async fn acknowledge_terminal_cleanup_preparation(
        &self,
        effect: &awaken_session_contract::SessionTerminalCleanupEffect,
    ) {
        awaken_session_contract::NeutralRuntime
            .acknowledge_terminal_cleanup_preparation(effect)
            .await
    }

    async fn dispose_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<awaken_session_contract::SessionCleanupDisposalReceipt, RunError> {
        Ok(awaken_protocol_managed::test_support::complete_terminal_cleanup_disposal(&effect))
    }

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Retire only the process-local teardown projection after Control has
    /// durably accepted the exact disposal receipt. No provider or Resource
    /// effect is permitted at this edge.
    async fn acknowledge_terminal_cleanup_disposal(
        &self,
        effect: &awaken_session_contract::SessionTerminalCleanupDisposalEffect,
    ) {
        awaken_session_contract::NeutralRuntime
            .acknowledge_terminal_cleanup_disposal(effect)
            .await
    }

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Retire a process-local terminal projection after Control reports that
    /// the aggregate has no remaining cleanup work. This closes final receipt
    /// response loss without replaying provider I/O or inventing another
    /// completion registry.
    async fn acknowledge_completed_terminal_cleanup(
        &self,
        session_id: &awaken_session_contract::SessionId,
        lease: &awaken_session_contract::SessionRealizationLease,
    ) {
        awaken_session_contract::NeutralRuntime
            .acknowledge_completed_terminal_cleanup(session_id, lease)
            .await
    }

    async fn install_terminal_cleanup_assignment(
        &self,
        _assignment: &awaken_session_contract::SessionTerminalCleanupAssignment,
        _proof: awaken_session_contract::ObservedSessionRealizationLease,
    ) -> Result<(), RunError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_session_contract::OutcomeRuntime for ScriptFake {
    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Persist one Outcome aggregate without executing a Worker or Grader Run.
    /// The opaque id is supplied by the application command so crash replay and
    /// activity admission share one stable operation identity.
    async fn prepare_outcome(
        &self,
        thread: &str,
        outcome_id: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<u64, awaken_session_contract::RunError> {
        awaken_session_contract::NeutralRuntime
            .prepare_outcome(thread, outcome_id, description, rubric, max_iterations)
            .await
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

    // Former `awaken_session_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Continue the active Outcome after the ordinary Run resume has committed.
    /// `None` proves there is no active aggregate; implementations must rebuild
    /// continuation exclusively from the Thread-owned Outcome state.
    async fn continue_outcome(
        &self,
        thread: &str,
    ) -> Result<Option<awaken_session_contract::OutcomeDrive>, awaken_session_contract::RunError>
    {
        awaken_session_contract::NeutralRuntime
            .continue_outcome(thread)
            .await
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
    let (state, app, completions) = app_with_runtime(
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
    support::drive_completed_user_dispatches(&state, &id, &completions).await;
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
/// code projects `model_rate_limited_error`; hard model faults (context,
/// authorization, expired login and exhausted billing) project
/// `model_request_failed_error` with a `terminal` retry status. An internal
/// failure mentioning HTTP 402 does not become a model billing diagnosis.
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
    for (code, kind, retry, message) in [
        (
            "rate_limited",
            "model_rate_limited_error",
            "exhausted",
            "HTTP 429 Too Many Requests",
        ),
        (
            "context_overflow",
            "model_request_failed_error",
            "terminal",
            "Context length exceeded",
        ),
        (
            "usage_limit",
            "model_request_failed_error",
            "terminal",
            r#"402 Payment Required: {"error":{"message":"Insufficient Balance","type":"unknown_error","code":"invalid_request_error"}}"#,
        ),
        (
            "login_required",
            "model_request_failed_error",
            "terminal",
            "token has expired",
        ),
        (
            "unauthorized",
            "model_request_failed_error",
            "terminal",
            "401 Unauthorized",
        ),
        (
            "internal",
            "unknown_error",
            "exhausted",
            "HTTP 402: tool service requires payment",
        ),
    ] {
        let (state, app, completions) = app_with_runtime(
            ScriptFake::new(move || failed(vec![assistant_text("a", "partial")], code, message))
                .with_committed_evidence(CommittedEvidence::Terminal),
        );
        let id = create(&app).await;
        send_user(&app, &id, "go").await;
        support::drive_completed_user_dispatches(&state, &id, &completions).await;
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
        assert_eq!(err["error"]["message"], message);
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
    let (state, app, completions) = app_with_runtime(runtime);
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    support::drive_completed_user_dispatches(&state, &id, &completions).await;
    let persisted = state
        .session_application()
        .session(&awaken_session_contract::SessionId::from(&id))
        .await
        .unwrap();
    assert_eq!(
        persisted.event_batches[0].events[0]
            .projection_anchor
            .map(|anchor| anchor.source_commit_cursor),
        Some(3),
        "R3/C1 no intermediate input observation: the first public anchor is the terminal fallback"
    );
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
        "R3/E1-E3: {list}"
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
    let (state, app, completions) = app_with_runtime(runtime);
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    support::drive_completed_user_dispatches(&state, &id, &completions).await;
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
    let (state, app, completions) = app_with_runtime(
        ScriptFake::new(|| ended(vec![assistant_text("a", "hi")]))
            .with_usage(usage)
            .with_committed_evidence(CommittedEvidence::Terminal),
    );
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    support::drive_completed_user_dispatches(&state, &id, &completions).await;
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
    let (state, app, completions) = app_with_runtime(
        ScriptFake::new(|| ended(vec![assistant_text("a", "unmetered")]))
            .with_committed_evidence(CommittedEvidence::Terminal),
    );
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    support::drive_completed_user_dispatches(&state, &id, &completions).await;
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
    let (state, app, completions) = app_with_runtime(
        ScriptFake::new(|| ended(vec![assistant_text("a", "")]))
            .with_committed_evidence(CommittedEvidence::Terminal),
    );
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    support::drive_completed_user_dispatches(&state, &id, &completions).await;
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
    let (state, app, completions) = app_with_runtime(
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
        // Production commits a gate decision for every governed call, so the
        // fixture records the one this call received instead of leaving the
        // wire to infer a permission from the presence of a result.
        .with_committed_evidence(CommittedEvidence::PermissionDecision {
            call_id: "mc1",
            decision: "allow",
        }),
    );
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    support::drive_completed_user_dispatches(&state, &id, &completions).await;
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
    let (state, app, completions) = app_with_runtime(
        ScriptFake::new(|| {
            StepOutcome::ended(vec![assistant_text("a", "gave up")], EndCause::MaxSteps)
        })
        .with_committed_evidence(CommittedEvidence::Terminal),
    );
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    support::drive_completed_user_dispatches(&state, &id, &completions).await;
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
/// `send_message` creates a real stable Thread; C3 its ordinary child Run
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
// Test design: a_delegation_projects_the_child_thread_lifecycle
// Cause/effect graph: delegated child facts produce one isolated Thread list/retrieve/event history.
// Decision table: known child=typed lifecycle; root/sibling excluded; unknown=404; replay=stable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delegation_projects_the_child_thread_lifecycle() {
    // Constraints/invariants: the Managed edge owns wire validation/projection only; Session/Run
    // stores and committed facts remain the single behavior authority.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    let runtime = CoordinatedRuntimeFake::default();
    let completions = runtime.completed_user_dispatches();
    let state = Arc::new(ManagedState::new(runtime));
    let app = router(state.clone());
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    support::drive_completed_user_dispatches(&state, &id, &completions).await;
    // Direct-read cause/effect rule M0: C0 the Runtime committed a child
    // lifecycle after the disposable HTTP cache was last touched; C1 the first
    // observer is GET /threads rather than GET /events. Effect E0 is the same
    // completed primary+child projection. Decision M0=C0+C1=>E0; a stale cache
    // that needs an Event read first would return only the primary Thread.
    let first_threads = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/threads"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        first_threads["data"].as_array().unwrap().len(),
        2,
        "M0/E0 direct Thread list refreshes committed child lifecycle"
    );
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
    let threads = first_threads;
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
    let retrieved_child = json_call(
        &app,
        "GET",
        &format!("/v1/sessions/{id}/threads/{child_thread_id}"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(
        retrieved_child, *child,
        "M1/E1 retrieve equals list projection"
    );
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
    support::drive_completed_user_dispatches(&state, &id, &completions).await;
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

/// Causes: C1 a field actually changes; C2 title changes to text/null; C3
/// metadata changes to non-empty/empty; C4 an unchanged field remains non-empty
/// in durable state; C5 a command is a no-op. Effects: E1 one `session.updated`
/// is emitted; E2 changed title is present as text/null; E3 changed non-empty
/// metadata is present; E4 unchanged or cleared-empty fields are absent; E5 no
/// event is emitted. Decision rules: U1=C1+C2(text)=>E1+E2(text)+E4(metadata);
/// U2=C1+C2(null)=>E1+E2(null)+E4(metadata); U3=C1+C3(non-empty)=>E1+E3+
/// E4(title); U4=C1+C3(empty)=>E1+E4(title,metadata); U5=C5=>E5. The initial
/// combined update covers both changed fields in one event. Constraint K1: the
/// Session aggregate and its `SessionUpdateOutcome.changes` remain the mutation
/// and change-mask authorities; this event owns no second field state.
// Test design: updating_a_session_commits_a_session_updated_event
// Cause/effect graph: accepted patch updates the aggregate and commits one matching session.updated fact.
// Decision table: omitted=preserve; null=clear where allowed; value=replace; invalid/terminal=reject.
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
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}"),
        serde_json::json!({ "metadata": { "team": "delivery" } }),
    )
    .await;
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}"),
        serde_json::json!({ "title": "retitled" }),
    )
    .await;
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}"),
        serde_json::json!({ "title": null }),
    )
    .await;
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}"),
        serde_json::json!({ "metadata": null }),
    )
    .await;
    json_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}"),
        serde_json::json!({ "title": null }),
    )
    .await;
    let list = list_events(&app, &id).await;
    let updated = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["type"] == "session.updated")
        .collect::<Vec<_>>();
    assert_eq!(updated.len(), 5, "U1-U4 emit once; U5 emits nothing");
    assert_eq!(updated[0]["title"], "renamed", "combined update title");
    assert_eq!(
        updated[0]["metadata"]["team"], "research",
        "combined metadata"
    );
    assert_eq!(updated[1]["metadata"]["team"], "delivery", "U3/E3");
    assert!(updated[1].get("title").is_none(), "U3/E4");
    assert_eq!(updated[2]["title"], "retitled", "U1/E2");
    assert!(updated[2].get("metadata").is_none(), "U1/E4");
    assert_eq!(
        updated[3].get("title"),
        Some(&serde_json::Value::Null),
        "U2/E2"
    );
    assert!(updated[3].get("metadata").is_none(), "U2/E4");
    assert!(updated[4].get("title").is_none(), "U4/E4");
    assert!(updated[4].get("metadata").is_none(), "U4/E4");
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
    let body = raw_call(
        &app,
        "POST",
        &format!("/v1/sessions/{id}/events"),
        serde_json::json!({ "events": [{ "type": "user.message", "content": [{ "type": "text", "text": "again" }] }] }),
    )
    .await.expect(StatusCode::CONFLICT);
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

#[tokio::test]
async fn a_call_the_gate_refused_is_reported_as_denied_not_allowed() {
    // Cause/effect graph: C1 the model asked for a tool; C2 the permission gate
    // refused it, so the run committed a blocked result and an audit record
    // naming that decision. E1 the wire reports `deny` for that call.
    //
    // Without the audit join the projection can only see that a result exists
    // and reports `allow` — telling a client the call it refused was permitted.
    // That is the regression this pins.
    let (state, app, completions) = app_with_runtime(
        ScriptFake::new(|| {
            ended(vec![
                Message::new(
                    Id("a".into()),
                    Role::Assistant,
                    vec![ContentBlock::ToolUse {
                        id: "blocked-1".into(),
                        name: "bash".into(),
                        input: serde_json::json!({ "command": "rm -rf /" }),
                    }],
                ),
                Message::new(
                    Id("t".into()),
                    Role::Tool,
                    vec![ContentBlock::ToolResult {
                        tool_use_id: "blocked-1".into(),
                        content: vec![ContentBlock::text("blocked: denied by policy")],
                        is_error: true,
                    }],
                ),
            ])
        })
        .with_committed_evidence(CommittedEvidence::PermissionDecision {
            call_id: "blocked-1",
            decision: "deny",
        }),
    );
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    support::drive_completed_user_dispatches(&state, &id, &completions).await;
    let list = list_events(&app, &id).await;
    let use_ev = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "agent.tool_use")
        .expect("the refused call is still visible on the stream");
    assert_eq!(
        use_ev["evaluated_permission"], "deny",
        "a refused call must never be reported as permitted"
    );
}

#[tokio::test]
async fn a_decision_label_this_version_does_not_know_is_never_read_as_permitted() {
    // Cause/effect graph: C1 the committed audit names a decision label this
    // build does not recognize (an older or newer gate vocabulary); C2 the call
    // has a committed result. E1 report NO evaluated permission at all, so a
    // client sees the field absent rather than a value we invented.
    //
    // The wire contract allows the field to be absent, and absence is the honest
    // rendering of "we hold no decision we understand". Folding an unknown label
    // into `allow` would be the exact fail-open this join exists to remove.
    let (state, app, completions) = app_with_runtime(
        ScriptFake::new(|| {
            ended(vec![
                Message::new(
                    Id("a".into()),
                    Role::Assistant,
                    vec![ContentBlock::ToolUse {
                        id: "future-1".into(),
                        name: "bash".into(),
                        input: serde_json::json!({ "command": "ls" }),
                    }],
                ),
                Message::new(
                    Id("t".into()),
                    Role::Tool,
                    vec![ContentBlock::ToolResult {
                        tool_use_id: "future-1".into(),
                        content: vec![ContentBlock::text("ok")],
                        is_error: false,
                    }],
                ),
            ])
        })
        .with_committed_evidence(CommittedEvidence::PermissionDecision {
            call_id: "future-1",
            decision: "escalate_to_owner",
        }),
    );
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    support::drive_completed_user_dispatches(&state, &id, &completions).await;
    let list = list_events(&app, &id).await;
    let use_ev = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "agent.tool_use")
        .expect("the call is still visible");
    assert!(
        use_ev.get("evaluated_permission").is_none() || use_ev["evaluated_permission"].is_null(),
        "an unrecognized decision label must not render as a permission: {use_ev}"
    );
}

#[tokio::test]
async fn a_recorded_ask_is_reported_even_though_the_transcript_shows_a_result() {
    // The deny case proved the join replaces a wrong inference. This proves the
    // join is not deny-specific: a call the gate put to a person, which then ran
    // and returned, still reports `ask` — the transcript alone would say `allow`
    // because it only sees a completed call.
    //
    // | Rule | recorded | transcript | Effect |
    // |---|---|---|---|
    // | K1 | ask | call + result | ask, not allow |
    let (state, app, completions) = app_with_runtime(
        ScriptFake::new(|| {
            ended(vec![
                Message::new(
                    Id("a".into()),
                    Role::Assistant,
                    vec![ContentBlock::ToolUse {
                        id: "asked-1".into(),
                        name: "bash".into(),
                        input: serde_json::json!({ "command": "ls" }),
                    }],
                ),
                Message::new(
                    Id("t".into()),
                    Role::Tool,
                    vec![ContentBlock::ToolResult {
                        tool_use_id: "asked-1".into(),
                        content: vec![ContentBlock::text("ok")],
                        is_error: false,
                    }],
                ),
            ])
        })
        .with_committed_evidence(CommittedEvidence::PermissionDecision {
            call_id: "asked-1",
            decision: "ask",
        }),
    );
    let id = create(&app).await;
    send_user(&app, &id, "go").await;
    support::drive_completed_user_dispatches(&state, &id, &completions).await;
    let list = list_events(&app, &id).await;
    let use_ev = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "agent.tool_use")
        .expect("the call is visible");
    assert_eq!(use_ev["evaluated_permission"], "ask", "K1");
}

// --- CE: recorded policy → SDK `evaluation` on both tool-use events -----------

/// Causes: C1 the committed audit row names the policy that decided (and, for
/// `auto`, its grounds); C2 the row predates the policy vocabulary or names a
/// policy outside the SDK's `permission_policy` set; C3 the call is a built-in
/// (`agent.tool_use`) or an MCP tool (`agent.mcp_tool_use`). Effects: E1 the
/// event carries `evaluation` in exactly the SDK 0.125.0 shape beside the flat
/// `evaluated_permission`; E2 `evaluation` is absent while the flat decision
/// is still reported; E3 both event families project identically.
///
/// | Rule | policy | decision | reason_code | evaluation |
/// |---|---|---|---|---|
/// | P1 | auto | ask | indeterminate | {"type":"auto","evaluated_permission":{"type":"ask","reason_code":"indeterminate"}} |
/// | P2 | auto | deny | high_risk | {"type":"auto","evaluated_permission":{"type":"deny","reason_code":"high_risk"}} |
/// | P3 | auto | allow | - | {"type":"auto","evaluated_permission":{"type":"allow"}} |
/// | P4 | always_allow | allow | - | {"type":"always_allow"} |
/// | P5 | always_ask | ask | - | {"type":"always_ask"} |
/// | P6 | rule | allow | - | absent (E2) |
/// | P7 | none (historical row) | allow | - | absent (E2) |
/// | P8 | bypass (outside the closed vocabulary) | deny | - | absent, decision still deny (E2) |
/// | P9 | auto | ask | none (inconsistent row) | absent (E2) |
/// | P10 | auto | ask | a registry code this version does not name | {"type":"auto","evaluated_permission":{"type":"ask","reason_code":"rate_limited"}} (E1) |
/// | P11 | none, `decided_by: run_narrowing` | deny | - | absent (E2) |
/// | P12 | none, `decided_by: plugin_gate` | ask | - | absent (E2) |
/// | P13 | always_allow beside `decided_by: run_narrowing` (inconsistent row) | allow | - | absent (E2) |
/// | P14 | always_allow beside an unknown `decided_by` label | allow | - | absent (E2) |
/// | P15 | always_allow, historical row without `decided_by` | allow | - | {"type":"always_allow"} (E1) |
///
/// Rows P1-P6 and P8-P10 carry `decided_by: policy`, as every current row a
/// policy decided does; P7 is a historical row without `decided_by`. The SDK
/// `evaluation` object only has policy vocabulary, so a decision a per-Run
/// narrowing or a plugin gate made projects the flat decision alone and no
/// wire field is invented for its decider.
///
/// P10 is the open-registry case the SDK names explicitly: `reason_code` is a
/// `string` whose documented set is not exhaustive and which clients must
/// tolerate, so another producer — a newer replica writing the same durable
/// audit, a platform evaluator — may record grounds this version has no name
/// for. Reading that row reports the decision and the grounds exactly as
/// recorded; dropping the `evaluation` because the code is unfamiliar would
/// hide an answer the SDK obliges every client to accept.
#[tokio::test]
async fn a_recorded_policy_projects_the_sdk_evaluation_on_both_tool_use_events() {
    const ROWS: &[(&str, &[AuditedPermissionRow], &str, Option<&str>)] = &[
        (
            "P1",
            &[
                (
                    "builtin-1",
                    "ask",
                    Some("policy"),
                    Some("auto"),
                    Some("indeterminate"),
                ),
                (
                    "mc1",
                    "ask",
                    Some("policy"),
                    Some("auto"),
                    Some("indeterminate"),
                ),
            ],
            "ask",
            Some(
                r#"{"type":"auto","evaluated_permission":{"type":"ask","reason_code":"indeterminate"}}"#,
            ),
        ),
        (
            "P2",
            &[
                (
                    "builtin-1",
                    "deny",
                    Some("policy"),
                    Some("auto"),
                    Some("high_risk"),
                ),
                (
                    "mc1",
                    "deny",
                    Some("policy"),
                    Some("auto"),
                    Some("high_risk"),
                ),
            ],
            "deny",
            Some(
                r#"{"type":"auto","evaluated_permission":{"type":"deny","reason_code":"high_risk"}}"#,
            ),
        ),
        (
            "P3",
            &[
                ("builtin-1", "allow", Some("policy"), Some("auto"), None),
                ("mc1", "allow", Some("policy"), Some("auto"), None),
            ],
            "allow",
            Some(r#"{"type":"auto","evaluated_permission":{"type":"allow"}}"#),
        ),
        (
            "P4",
            &[
                (
                    "builtin-1",
                    "allow",
                    Some("policy"),
                    Some("always_allow"),
                    None,
                ),
                ("mc1", "allow", Some("policy"), Some("always_allow"), None),
            ],
            "allow",
            Some(r#"{"type":"always_allow"}"#),
        ),
        (
            "P5",
            &[
                ("builtin-1", "ask", Some("policy"), Some("always_ask"), None),
                ("mc1", "ask", Some("policy"), Some("always_ask"), None),
            ],
            "ask",
            Some(r#"{"type":"always_ask"}"#),
        ),
        (
            "P6",
            &[
                ("builtin-1", "allow", Some("policy"), Some("rule"), None),
                ("mc1", "allow", Some("policy"), Some("rule"), None),
            ],
            "allow",
            None,
        ),
        (
            "P7",
            &[
                ("builtin-1", "allow", None, None, None),
                ("mc1", "allow", None, None, None),
            ],
            "allow",
            None,
        ),
        (
            "P8",
            &[
                ("builtin-1", "deny", Some("policy"), Some("bypass"), None),
                ("mc1", "deny", Some("policy"), Some("bypass"), None),
            ],
            "deny",
            None,
        ),
        (
            "P9",
            &[
                ("builtin-1", "ask", Some("policy"), Some("auto"), None),
                ("mc1", "ask", Some("policy"), Some("auto"), None),
            ],
            "ask",
            None,
        ),
        (
            "P11",
            &[
                ("builtin-1", "deny", Some("run_narrowing"), None, None),
                ("mc1", "deny", Some("run_narrowing"), None, None),
            ],
            "deny",
            None,
        ),
        (
            "P12",
            &[
                ("builtin-1", "ask", Some("plugin_gate"), None, None),
                ("mc1", "ask", Some("plugin_gate"), None, None),
            ],
            "ask",
            None,
        ),
        (
            "P13",
            &[
                (
                    "builtin-1",
                    "allow",
                    Some("run_narrowing"),
                    Some("always_allow"),
                    None,
                ),
                (
                    "mc1",
                    "allow",
                    Some("run_narrowing"),
                    Some("always_allow"),
                    None,
                ),
            ],
            "allow",
            None,
        ),
        (
            "P14",
            &[
                (
                    "builtin-1",
                    "allow",
                    Some("operator"),
                    Some("always_allow"),
                    None,
                ),
                ("mc1", "allow", Some("operator"), Some("always_allow"), None),
            ],
            "allow",
            None,
        ),
        (
            "P15",
            &[
                ("builtin-1", "allow", None, Some("always_allow"), None),
                ("mc1", "allow", None, Some("always_allow"), None),
            ],
            "allow",
            Some(r#"{"type":"always_allow"}"#),
        ),
        (
            "P10",
            &[
                (
                    "builtin-1",
                    "ask",
                    Some("policy"),
                    Some("auto"),
                    Some("rate_limited"),
                ),
                (
                    "mc1",
                    "ask",
                    Some("policy"),
                    Some("auto"),
                    Some("rate_limited"),
                ),
            ],
            "ask",
            Some(
                r#"{"type":"auto","evaluated_permission":{"type":"ask","reason_code":"rate_limited"}}"#,
            ),
        ),
    ];
    for (rule, rows, expected_decision, expected_evaluation) in ROWS {
        let (state, app, completions) = app_with_runtime(
            ScriptFake::new(|| {
                ended(vec![
                    Message::new(
                        Id("a".into()),
                        Role::Assistant,
                        vec![
                            ContentBlock::ToolUse {
                                id: "builtin-1".into(),
                                name: "bash".into(),
                                input: serde_json::json!({ "command": "ls" }),
                            },
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
                        vec![
                            ContentBlock::ToolResult {
                                tool_use_id: "builtin-1".into(),
                                content: vec![ContentBlock::text("ok")],
                                is_error: false,
                            },
                            ContentBlock::ToolResult {
                                tool_use_id: "mc1".into(),
                                content: vec![ContentBlock::text("3 hits")],
                                is_error: false,
                            },
                        ],
                    ),
                ])
            })
            .with_committed_evidence(CommittedEvidence::PermissionDecisions(rows)),
        );
        let id = create(&app).await;
        send_user(&app, &id, "go").await;
        support::drive_completed_user_dispatches(&state, &id, &completions).await;
        let list = list_events(&app, &id).await;
        let expected_evaluation = expected_evaluation
            .map(|json| serde_json::from_str::<serde_json::Value>(json).unwrap());
        for event_type in ["agent.tool_use", "agent.mcp_tool_use"] {
            let use_ev = list["data"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["type"] == event_type)
                .unwrap_or_else(|| panic!("{rule}: {event_type} is visible"));
            assert_eq!(
                use_ev["evaluated_permission"], *expected_decision,
                "{rule}/{event_type}: the flat decision is always the recorded one"
            );
            match &expected_evaluation {
                Some(evaluation) => assert_eq!(
                    use_ev["evaluation"], *evaluation,
                    "{rule}/{event_type}/E1 the SDK evaluation shape, byte for byte"
                ),
                None => assert!(
                    use_ev.get("evaluation").is_none(),
                    "{rule}/{event_type}/E2 no evaluation is projected: {use_ev}"
                ),
            }
        }
    }
}
