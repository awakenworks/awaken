//! Cross-adapter startup proof for the Awaken live-inbox resource: list,
//! queue, replace, reorder, and withdraw over the Managed Session application.
//! the session's in-flight queue, and the error mapping (404 unknown message,
//! 409 stale order, 410 inactive queue, 404 unknown session).

use awaken_protocol_managed_testkit::ManagedStateFixture as _;
use awaken_session_execution_contract::{
    SessionCoordinationReads as _, StreamingRuns as _, ThreadReads as _,
};
use awaken_session_types::SessionId;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_agent_contract::agent::run::EndCause;
use awaken_protocol_awaken::live_inbox_router;
use awaken_protocol_managed::{ManagedState, router};
use awaken_runtime_contract::runs::{RunError, StepOutcome};

use awaken_runtime_contract::runs::scope::WorkspaceScope;
use awaken_session_application_contract::{LiveInboxEntry, LiveInboxError, LiveInboxSnapshot};
use awaken_session_execution_contract::{OutcomeDrive, SessionRuns};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    call_in_scope(app, method, uri, body, Some("default")).await
}

async fn call_in_scope(
    app: &Router,
    method: &str,
    uri: &str,
    body: serde_json::Value,
    workspace: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(if body.is_null() {
            Body::empty()
        } else {
            Body::from(serde_json::to_vec(&body).unwrap())
        })
        .unwrap();
    if let Some(workspace) = workspace {
        req.extensions_mut()
            .insert(WorkspaceScope(workspace.to_string()));
    }
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(&bytes).into()))
    };
    (status, value)
}

async fn create(app: &Router) -> String {
    let (status, s) = call(
        app,
        "POST",
        "/v1/sessions",
        serde_json::json!({
            "agent": "coder",
            "environment_id": awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "Session creation: {s}");
    s["id"].as_str().unwrap().to_string()
}

fn text_content(text: &str) -> serde_json::Value {
    serde_json::json!({ "content": [{ "type": "text", "text": text }] })
}

/// A fake runtime whose live inbox is a plain in-memory queue, so these tests
/// exercise the wire surface (routing, DTOs, status mapping) — the real queue
/// semantics are covered by the runtime-contract and engine tests.
#[derive(Default)]
struct QueueFake {
    active: AtomicBool,
    live_calls: AtomicU64,
    queue: Mutex<FakeQueue>,
    user_runs: Mutex<HashMap<String, awaken_session_execution_contract::AdmitSessionRun>>,
}

#[derive(Default)]
struct FakeQueue {
    next_id: u64,
    version: u64,
    entries: Vec<(u64, Vec<ContentBlock>)>,
}

fn unsupported_runtime_operation() -> RunError {
    RunError::internal("operation is outside the live-inbox test scope")
}

#[async_trait::async_trait]
impl SessionRuns for QueueFake {
    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        Ok(StepOutcome::ended(
            vec![Message::text(Id("a".into()), Role::Assistant, "ok")],
            EndCause::NaturalEnd,
        ))
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: awaken_agent_contract::agent::awaiting::PermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        Err(unsupported_runtime_operation())
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<ContentBlock>,
        _is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        Err(unsupported_runtime_operation())
    }

    fn model(&self) -> String {
        "fake".to_string()
    }
}

awaken_session_execution_contract::neutral_role!(BudgetAccounting for QueueFake);

awaken_session_execution_contract::neutral_role!(StreamingRuns for QueueFake);

awaken_session_execution_contract::neutral_role!(EnvironmentRuntime for QueueFake);

awaken_session_execution_contract::neutral_role!(TerminalCleanup for QueueFake);

awaken_session_execution_contract::neutral_role!(CapabilityRuntime for QueueFake);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for QueueFake {
    fn observe_realization_request(
        &self,
    ) -> Result<awaken_lease_contract::authority::AuthorityProofStarted, RunError> {
        // Creation requires explicit request-relative execution-clock evidence;
        // the canonical test clock supplies that prerequisite for wire tests.
        awaken_protocol_managed_testkit::observe_realization_request()
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reconcile the existing Runtime projections through the given Control
    /// owner. Local and Worker deployment use one slot-based selection and
    /// physical-cutoff refresh algorithm; no durable Session rescan is implied.
    async fn renew_session_realization_projections(
        &self,
        control: &dyn awaken_session_execution_contract::SessionRealizationControl,
        owner: &str,
        runtime_incarnation: &str,
        timing: awaken_lease_contract::authority::AuthorityLeaseTiming,
    ) -> Result<usize, awaken_runtime_contract::runs::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_projections(control, owner, runtime_incarnation, timing)
            .await
    }

    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_execution_contract::FrozenSessionProjection,
        _mode: awaken_session_execution_contract::SessionProjectionInstallMode,
    ) -> Result<(), awaken_session_execution_contract::SessionRealizationEffectFailure> {
        // Test-fixture cause/effect rule: complete thread/workspace/Agent coordinates are
        // accepted so live-inbox cases can reach their subject; any missing coordinate fails
        // before queue state exists. The typed port makes every partial installation impossible.
        if thread.is_empty()
            || projection.workspace_id.is_empty()
            || projection.baseline.agent_id.is_empty()
        {
            return Err((RunError::bad_request(
                "live-inbox test Session projection is incomplete",
            ))
            .into());
        }
        Ok(())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Install a monotonic same-owner/same-incarnation/same-epoch successor of
    /// an already-installed realization fence. This is the only lease-only
    /// Runtime mutation; it deliberately carries no projection or phase action.
    async fn renew_session_realization_lease(
        &self,
        thread: &str,
        lease: awaken_session_execution_contract::ObservedSessionRealizationLease,
    ) -> Result<(), awaken_runtime_contract::runs::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_lease(thread, lease)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Forget one failed realization fence from disposable Runtime state. The
    /// aggregate remains the sole lease authority: this hook only prevents a
    /// later retry from authorizing effects with a lease that Control has
    /// already failed or may have failed before losing its response.
    async fn invalidate_session_realization_lease(
        &self,
        thread: &str,
        lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
    ) -> Result<(), awaken_runtime_contract::runs::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .invalidate_session_realization_lease(thread, lease)
            .await
    }
}

awaken_session_execution_contract::neutral_role!(RealizationEffects for QueueFake);

#[async_trait::async_trait]
impl awaken_session_execution_contract::ChildRunCoordination for QueueFake {
    // Fixture rule P1: live-inbox tests expose completed ordinary User reservations only.
    // A prepared command refuses before activity admission or fixture mutation.
    async fn run_prepared_session_run(
        &self,
        command: awaken_session_execution_contract::AdmitPreparedSessionRun,
        activity: &dyn awaken_session_execution_contract::SessionRunActivityAdmitter,
    ) -> Result<awaken_agent_contract::agent::run::RunState, awaken_runtime_contract::runs::RunError>
    {
        awaken_session_execution_contract::ChildRunCoordination::run_prepared_session_run(
            &awaken_session_execution_contract::NeutralRuntime,
            command,
            activity,
        )
        .await
    }

    async fn reserve_session_run(
        &self,
        command: awaken_session_execution_contract::AdmitSessionRun,
    ) -> Result<
        awaken_session_execution_contract::SessionRunReservation,
        awaken_runtime_contract::runs::RunError,
    > {
        let mut runs = self.user_runs.lock().unwrap();
        if let Some(existing) = runs.get(&command.run_id.0) {
            if existing != &command {
                return Err(RunError::bad_request(
                    "live-inbox test reservation replay changed its command",
                ));
            }
            return Ok(awaken_session_execution_contract::SessionRunReservation::Completed);
        }
        runs.insert(command.run_id.0.clone(), command);
        Ok(awaken_session_execution_contract::SessionRunReservation::Completed)
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Publish one exact reservation only after the Session root committed its
    /// activity epoch. Ordinary Worker claim/execution remains the sole runner.
    async fn activate_session_run(
        &self,
        delivery: awaken_session_execution_contract::SessionRunDelivery,
    ) -> Result<
        awaken_session_execution_contract::SessionRunActivation,
        awaken_runtime_contract::runs::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .activate_session_run(delivery)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
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
        admission: awaken_session_execution_contract::AdmittedSessionRun,
        input_message_ids: Vec<String>,
        sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<awaken_runtime_contract::runs::StepOutcome, awaken_runtime_contract::runs::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .activate_and_observe_session_run(admission, input_message_ids, sink)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Admit one ordinary child Run after the Session application has validated
    /// the frozen roster and lifecycle. Implementations must use their canonical
    /// durable Run ingress; the default fails closed rather than spawning a
    /// process-local task.
    async fn admit_coordinated_run(
        &self,
        command: awaken_session_execution_contract::CoordinatedRunCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionAgentMessageReceipt,
        awaken_runtime_contract::runs::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .admit_coordinated_run(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Subscribe to neutral live observations for one already-admitted root or
    /// child logical Thread. `None` is the fail-closed default: committed events
    /// remain available, but no Runtime without an exact Thread-scoped observer
    /// can accidentally leak previews across Threads. This port is a view over
    /// the Runtime's existing live hub, not another fan-out or replay owner.
    async fn subscribe_session_thread_live(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<
        Option<Box<dyn awaken_session_execution_contract::SessionThreadLiveSubscription>>,
        awaken_runtime_contract::runs::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .subscribe_session_thread_live(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Admit one Session-approved child report as immutable input of the
    /// deterministic primary Run. Implementations must not stage a parallel
    /// Outbox/Inbox message for this internal Agent-to-Agent transfer.
    async fn continue_session_agent_report(
        &self,
        command: awaken_session_execution_contract::SessionAgentReportContinuation,
    ) -> Result<(), awaken_runtime_contract::runs::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .continue_session_agent_report(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Interrupt one logical child through its parent Session's physical commit
    /// and dispatch partition. Implementations must not open child-named storage.
    async fn interrupt_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        child_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<(), awaken_runtime_contract::runs::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .interrupt_session_thread(session_id, child_thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reply to the exact pending tool of a Primary or logical child Thread
    /// through the Session-affine claim-fenced resume/commit boundary.
    async fn reply_session_thread_tool(
        &self,
        delivery: awaken_session_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), awaken_runtime_contract::runs::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .reply_session_thread_tool(delivery)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Foreground form of [`Self::reply_session_thread_tool`]. The durable
    /// reply ingress remains identical; implementations additionally register
    /// before publication and project the next committed Step from Thread
    /// truth. No inline executor is authorized by this observation policy.
    async fn reply_and_observe_session_thread_tool(
        &self,
        delivery: awaken_session_contract::SessionThreadToolReplyDelivery,
        stream_sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<awaken_runtime_contract::runs::StepOutcome, awaken_runtime_contract::runs::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .reply_and_observe_session_thread_tool(delivery, stream_sink)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Stop the parent Run, wait until it can no longer commit a delegation, and
    /// then read the complete child set from durable runtime authority.
    async fn quiesce_terminal_delegations(
        &self,
        thread: &str,
    ) -> Result<
        awaken_session_application_contract::DelegatedRunSnapshot,
        awaken_runtime_contract::runs::RunError,
    > {
        self.interrupt(thread).await?;
        Ok(awaken_session_application_contract::DelegatedRunSnapshot {
            delegated_runs: self.delegated_runs(thread).await?,
            coordinated_thread_ids: Vec::new(),
            watermark: 0,
            runtime_commit_cursor: self
                .session_thread_recovery_snapshot(
                    &awaken_session_types::SessionId::from(thread),
                    thread,
                )
                .await?
                .map(|snapshot| snapshot.store_cursor)
                .unwrap_or_default(),
        })
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Commit the absorbing archive disposition on one logical child Thread
    /// through its parent Session partition. Implementations must not mutate a
    /// protocol cache or open child-named storage.
    async fn archive_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<(), awaken_runtime_contract::runs::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .archive_session_thread(session_id, thread_id)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionCoordinationReads for QueueFake {
    async fn source_dispatch_observation(
        &self,
        _session: &awaken_session_contract::PersistedSession,
    ) -> awaken_session_execution_contract::SessionSourceDispatchObservation {
        Default::default()
    }

    async fn coordinated_admission_receipt(
        &self,
        command: awaken_session_execution_contract::CoordinatedAdmissionRef<'_>,
    ) -> Result<
        Option<awaken_session_execution_contract::SessionAgentMessageReceipt>,
        awaken_runtime_contract::runs::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .coordinated_admission_receipt(command)
            .await
    }

    async fn session_run_state(
        &self,
        _session_id: &SessionId,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<awaken_agent_contract::agent::run::RunState>, RunError> {
        Ok(self
            .user_runs
            .lock()
            .unwrap()
            .contains_key(&run_id.0)
            .then(|| awaken_agent_contract::agent::run::RunState::Ended(EndCause::NaturalEnd)))
    }

    /// Read the runtime-owned, durable child-Run relationships for `thread`.
    /// Protocol adapters use this only to rebuild disposable projections after a
    /// restart; the runtime relationship registry remains the sole authority.
    async fn delegated_runs(
        &self,
        thread: &str,
    ) -> Result<
        Vec<awaken_runtime_contract::runs::DelegatedRun>,
        awaken_runtime_contract::runs::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .delegated_runs(thread)
            .await
    }

    /// Rebuild the Session's coordinated child-Thread links from existing
    /// committed ToolBatch/transcript/dispatch facts. This is a query, not a
    /// relationship repository.
    /// `observed_root` may borrow an atomic prefix already read for this Session
    /// during a larger read-only projection. Implementations must reject a
    /// different root identity and fence enrichment to that prefix. `None`
    /// reads the current prefix; command admission and final validation use it.
    async fn coordinated_threads(
        &self,
        session_id: &awaken_session_types::SessionId,
        observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<
        Vec<awaken_session_execution_contract::CoordinatedThreadLink>,
        awaken_runtime_contract::runs::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .coordinated_threads(session_id, observed_root)
            .await
    }

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
        awaken_runtime_contract::runs::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .session_thread_tool_reply_fence(command)
            .await
    }

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
        command: &awaken_session_execution_contract::SessionAgentMessageCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionAgentMessageCommand,
        awaken_runtime_contract::runs::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_agent_message_source(command)
            .await
    }

    async fn session_run_command_receipt(
        &self,
        command: &awaken_session_execution_contract::AdmitSessionRun,
    ) -> Result<
        Option<awaken_session_execution_contract::SessionRunReservation>,
        awaken_runtime_contract::runs::RunError,
    > {
        awaken_session_execution_contract::SessionCoordinationReads::session_run_command_receipt(
            &awaken_session_execution_contract::NeutralRuntime,
            command,
        )
        .await
    }

    async fn task_callback_source(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &awaken_agent_contract::ThreadId,
        root_agent_id: &str,
    ) -> Result<
        Option<(
            String,
            awaken_agent_contract::agent::delegation::DelegationOrigin,
        )>,
        awaken_runtime_contract::runs::RunError,
    > {
        awaken_session_execution_contract::SessionCoordinationReads::task_callback_source(
            &awaken_session_execution_contract::NeutralRuntime,
            session_id,
            thread_id,
            root_agent_id,
        )
        .await
    }

    async fn task_origin_execution(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &awaken_agent_contract::ThreadId,
        origin_run_id: &awaken_agent_contract::RunId,
    ) -> Result<
        Box<(
            awaken_runtime_contract::RunActivation,
            Vec<awaken_runtime_contract::ExecutableAgentSnapshot>,
        )>,
        awaken_runtime_contract::runs::RunError,
    > {
        awaken_session_execution_contract::SessionCoordinationReads::task_origin_execution(
            &awaken_session_execution_contract::NeutralRuntime,
            session_id,
            thread_id,
            origin_run_id,
        )
        .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::ThreadReads for QueueFake {
    async fn transcript_page(
        &self,
        _session: &::awaken_session_types::SessionId,
        _request: &::awaken_agent_contract::thread::read::transcript::TranscriptPageRequest,
    ) -> Result<
        ::awaken_agent_contract::thread::read::transcript::TranscriptPage,
        ::awaken_runtime_contract::runs::RunError,
    > {
        Err(::awaken_runtime_contract::runs::RunError::unavailable(
            "transcript pagination is unsupported",
        ))
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// True when durable truth already exists for `thread`. Session-id minting
    /// consults this to skip ids a previous process persisted; implementations
    /// MUST answer without materializing any per-thread state (no context
    /// build, no cache entry) — probing must be free of side effects. The
    /// default reports nothing, so an ephemeral host mints densely from 0.
    async fn owns_thread(
        &self,
        thread: &str,
    ) -> Result<bool, awaken_runtime_contract::runs::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .owns_thread(thread)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
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
        awaken_runtime_contract::runs::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .committed_messages(thread)
            .await
    }

    async fn session_thread_recovery_snapshot(
        &self,
        session_id: &SessionId,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        // Fake boundary rule F2/R1: `Completed` and `Ended` are observable only
        // together with the committed message and its exact cursor. Returning a
        // partial terminal view would construct an impossible Runtime state and
        // leave the Session event permanently accepted but unprocessed.
        let runs = self.user_runs.lock().unwrap();
        let Some(command) = runs
            .values()
            .find(|command| command.session_id == session_id)
        else {
            return Ok(None);
        };
        let run_id = command.run_id.clone();
        let thread_id = awaken_agent_contract::agent::thread::Id(thread_id.to_string());
        Ok(Some(
            awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
                commit_timestamps: Default::default(),
                thread_id: thread_id.clone(),
                claimed_run_id: run_id.clone(),
                runs: vec![awaken_agent_contract::agent::run::Record {
                    id: run_id.clone(),
                    thread_id,
                    state: awaken_agent_contract::agent::run::RunState::Ended(EndCause::NaturalEnd),
                }],
                latest_run_id: Some(run_id),
                message_commit_cursors: vec![1; command.messages.len()],
                messages: command.messages.clone(),
                state: Vec::new(),
                state_commit_cursors: Vec::new(),
                events: Vec::new(),
                resume_tickets: Vec::new(),
                thread_version: 1,
                store_cursor: 1,
                next_commit_ordinal: 1,
            },
        ))
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
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
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<
        Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
        awaken_runtime_contract::runs::RunError,
    > {
        awaken_session_execution_contract::select_recovery_snapshot_run(
            self.session_thread_recovery_snapshot(session_id, thread_id)
                .await?,
            run_id,
        )
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read the durable disposition of one logical child Thread through its
    /// parent Session's physical commit partition. Legacy Threads without the
    /// state cell remain Active.
    async fn session_thread_disposition(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<awaken_agent_contract::ThreadDisposition, awaken_runtime_contract::runs::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .session_thread_disposition(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact terminal Outcome projection from committed Thread truth.
    /// `None` means the id is absent or still active. Implementations must not
    /// resume the Outcome or reconstruct a terminal from an in-process
    /// [`awaken_runtime_contract::runs::StepOutcome`].
    async fn committed_outcome_projection(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<
        Option<awaken_session_execution_contract::CommittedOutcomeProjection>,
        awaken_runtime_contract::runs::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .committed_outcome_projection(thread, outcome_id)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::Outcomes for QueueFake {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
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
    ) -> Result<u64, awaken_runtime_contract::runs::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .prepare_outcome(thread, outcome_id, description, rubric, max_iterations)
            .await
    }

    async fn define_outcome(
        &self,
        _thread: &str,
        _description: &str,
        _rubric: &str,
        _max_iterations: u32,
        _executor: &dyn awaken_session_execution_contract::PreparedSessionRunAdmission,
    ) -> Result<OutcomeDrive, RunError> {
        Err(unsupported_runtime_operation())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Continue the active Outcome after the ordinary Run resume has committed.
    /// `None` proves there is no active aggregate; implementations must rebuild
    /// continuation exclusively from the Thread-owned Outcome state.
    async fn continue_outcome(
        &self,
        thread: &str,
        executor: &dyn awaken_session_execution_contract::PreparedSessionRunAdmission,
    ) -> Result<
        Option<awaken_session_execution_contract::OutcomeDrive>,
        awaken_runtime_contract::runs::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .continue_outcome(thread, executor)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::LiveInboxAccess for QueueFake {
    async fn live_inbox_snapshot(
        &self,
        _thread: &str,
    ) -> Result<LiveInboxSnapshot, LiveInboxError> {
        self.live_calls.fetch_add(1, Ordering::SeqCst);
        if !self.active.load(Ordering::SeqCst) {
            return Ok(LiveInboxSnapshot::inactive());
        }
        let queue = self.queue.lock().unwrap();
        Ok(LiveInboxSnapshot {
            active: true,
            version: queue.version,
            messages: queue
                .entries
                .iter()
                .map(|(id, content)| LiveInboxEntry {
                    id: *id,
                    content: content.clone(),
                })
                .collect(),
        })
    }

    async fn live_inbox_queue(
        &self,
        _thread: &str,
        content: Vec<ContentBlock>,
    ) -> Result<u64, LiveInboxError> {
        self.live_calls.fetch_add(1, Ordering::SeqCst);
        if !self.active.load(Ordering::SeqCst) {
            return Err(LiveInboxError::Inactive);
        }
        let mut queue = self.queue.lock().unwrap();
        queue.next_id += 1;
        queue.version += 1;
        let id = queue.next_id;
        queue.entries.push((id, content));
        Ok(id)
    }

    async fn live_inbox_remove(&self, _thread: &str, id: u64) -> Result<(), LiveInboxError> {
        self.live_calls.fetch_add(1, Ordering::SeqCst);
        if !self.active.load(Ordering::SeqCst) {
            return Err(LiveInboxError::Inactive);
        }
        let mut queue = self.queue.lock().unwrap();
        let index = queue
            .entries
            .iter()
            .position(|(entry_id, _)| *entry_id == id)
            .ok_or(LiveInboxError::UnknownMessage)?;
        queue.entries.remove(index);
        queue.version += 1;
        Ok(())
    }

    async fn live_inbox_replace(
        &self,
        _thread: &str,
        id: u64,
        content: Vec<ContentBlock>,
    ) -> Result<(), LiveInboxError> {
        self.live_calls.fetch_add(1, Ordering::SeqCst);
        if !self.active.load(Ordering::SeqCst) {
            return Err(LiveInboxError::Inactive);
        }
        let mut queue = self.queue.lock().unwrap();
        let entry = queue
            .entries
            .iter_mut()
            .find(|(entry_id, _)| *entry_id == id)
            .ok_or(LiveInboxError::UnknownMessage)?;
        entry.1 = content;
        queue.version += 1;
        Ok(())
    }

    async fn live_inbox_reorder(
        &self,
        _thread: &str,
        order: Vec<u64>,
    ) -> Result<(), LiveInboxError> {
        self.live_calls.fetch_add(1, Ordering::SeqCst);
        if !self.active.load(Ordering::SeqCst) {
            return Err(LiveInboxError::Inactive);
        }
        let mut queue = self.queue.lock().unwrap();
        if order.len() != queue.entries.len() {
            return Err(LiveInboxError::StaleOrder);
        }
        let mut reordered = Vec::with_capacity(order.len());
        for id in &order {
            let index = queue
                .entries
                .iter()
                .position(|(entry_id, _)| entry_id == id)
                .ok_or(LiveInboxError::StaleOrder)?;
            reordered.push(queue.entries.remove(index));
        }
        queue.entries = reordered;
        queue.version += 1;
        Ok(())
    }
}

async fn app_with_session_application(
    fake: Arc<QueueFake>,
) -> (Router, Arc<awaken_session_application::SessionApplication>) {
    struct Shared(Arc<QueueFake>);
    #[async_trait::async_trait]
    impl SessionRuns for Shared {
        async fn run(
            &self,
            a: &str,
            t: &str,
            c: Vec<ContentBlock>,
        ) -> Result<StepOutcome, RunError> {
            self.0.run(a, t, c).await
        }
        async fn resume(
            &self,
            t: &str,
            i: &str,
            d: awaken_agent_contract::agent::awaiting::PermissionDecision,
        ) -> Result<StepOutcome, RunError> {
            self.0.resume(t, i, d).await
        }
        async fn resume_custom(
            &self,
            t: &str,
            i: &str,
            c: Vec<ContentBlock>,
            e: bool,
        ) -> Result<StepOutcome, RunError> {
            self.0.resume_custom(t, i, c, e).await
        }
        fn model(&self) -> String {
            self.0.model()
        }
    }

    awaken_session_execution_contract::neutral_role!(BudgetAccounting for Shared);

    awaken_session_execution_contract::neutral_role!(StreamingRuns for Shared);

    awaken_session_execution_contract::neutral_role!(EnvironmentRuntime for Shared);

    awaken_session_execution_contract::neutral_role!(TerminalCleanup for Shared);

    awaken_session_execution_contract::neutral_role!(CapabilityRuntime for Shared);

    #[async_trait::async_trait]
    impl awaken_session_execution_contract::RealizationRuntime for Shared {
        fn observe_realization_request(
            &self,
        ) -> Result<awaken_lease_contract::authority::AuthorityProofStarted, RunError> {
            self.0.observe_realization_request()
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.
        /// Reconcile the existing Runtime projections through the given Control
        /// owner. Local and Worker deployment use one slot-based selection and
        /// physical-cutoff refresh algorithm; no durable Session rescan is implied.
        async fn renew_session_realization_projections(
            &self,
            control: &dyn awaken_session_execution_contract::SessionRealizationControl,
            owner: &str,
            runtime_incarnation: &str,
            timing: awaken_lease_contract::authority::AuthorityLeaseTiming,
        ) -> Result<usize, awaken_runtime_contract::runs::RunError> {
            awaken_session_execution_contract::NeutralRuntime
                .renew_session_realization_projections(control, owner, runtime_incarnation, timing)
                .await
        }

        async fn install_session_projection(
            &self,
            thread: &str,
            projection: awaken_session_execution_contract::FrozenSessionProjection,
            mode: awaken_session_execution_contract::SessionProjectionInstallMode,
        ) -> Result<(), awaken_session_execution_contract::SessionRealizationEffectFailure>
        {
            self.0
                .install_session_projection(thread, projection, mode)
                .await
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.
        /// Install a monotonic same-owner/same-incarnation/same-epoch successor of
        /// an already-installed realization fence. This is the only lease-only
        /// Runtime mutation; it deliberately carries no projection or phase action.
        async fn renew_session_realization_lease(
            &self,
            thread: &str,
            lease: awaken_session_execution_contract::ObservedSessionRealizationLease,
        ) -> Result<(), awaken_runtime_contract::runs::RunError> {
            awaken_session_execution_contract::NeutralRuntime
                .renew_session_realization_lease(thread, lease)
                .await
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.
        /// Forget one failed realization fence from disposable Runtime state. The
        /// aggregate remains the sole lease authority: this hook only prevents a
        /// later retry from authorizing effects with a lease that Control has
        /// already failed or may have failed before losing its response.
        async fn invalidate_session_realization_lease(
            &self,
            thread: &str,
            lease: &awaken_session_configuration_contract::resource::SessionRealizationLease,
        ) -> Result<(), awaken_runtime_contract::runs::RunError> {
            awaken_session_execution_contract::NeutralRuntime
                .invalidate_session_realization_lease(thread, lease)
                .await
        }
    }

    awaken_session_execution_contract::neutral_role!(RealizationEffects for Shared);

    #[async_trait::async_trait]
    impl awaken_session_execution_contract::ChildRunCoordination for Shared {
        // Fixture rule P1: the shared live-inbox wrapper retains its inner fixture refusal.
        // A prepared command refuses before activity admission or fixture mutation.
        async fn run_prepared_session_run(
            &self,
            command: awaken_session_execution_contract::AdmitPreparedSessionRun,
            activity: &dyn awaken_session_execution_contract::SessionRunActivityAdmitter,
        ) -> Result<
            awaken_agent_contract::agent::run::RunState,
            awaken_runtime_contract::runs::RunError,
        > {
            awaken_session_execution_contract::ChildRunCoordination::run_prepared_session_run(
                self.0.as_ref(),
                command,
                activity,
            )
            .await
        }

        async fn reserve_session_run(
            &self,
            command: awaken_session_execution_contract::AdmitSessionRun,
        ) -> Result<
            awaken_session_execution_contract::SessionRunReservation,
            awaken_runtime_contract::runs::RunError,
        > {
            self.0.reserve_session_run(command).await
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.
        /// Publish one exact reservation only after the Session root committed its
        /// activity epoch. Ordinary Worker claim/execution remains the sole runner.
        async fn activate_session_run(
            &self,
            delivery: awaken_session_execution_contract::SessionRunDelivery,
        ) -> Result<
            awaken_session_execution_contract::SessionRunActivation,
            awaken_runtime_contract::runs::RunError,
        > {
            awaken_session_execution_contract::NeutralRuntime
                .activate_session_run(delivery)
                .await
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
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
            admission: awaken_session_execution_contract::AdmittedSessionRun,
            input_message_ids: Vec<String>,
            sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
        ) -> Result<
            awaken_runtime_contract::runs::StepOutcome,
            awaken_runtime_contract::runs::RunError,
        > {
            awaken_session_execution_contract::NeutralRuntime
                .activate_and_observe_session_run(admission, input_message_ids, sink)
                .await
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.
        /// Admit one ordinary child Run after the Session application has validated
        /// the frozen roster and lifecycle. Implementations must use their canonical
        /// durable Run ingress; the default fails closed rather than spawning a
        /// process-local task.
        async fn admit_coordinated_run(
            &self,
            command: awaken_session_execution_contract::CoordinatedRunCommand,
        ) -> Result<
            awaken_session_execution_contract::SessionAgentMessageReceipt,
            awaken_runtime_contract::runs::RunError,
        > {
            awaken_session_execution_contract::NeutralRuntime
                .admit_coordinated_run(command)
                .await
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.
        /// Subscribe to neutral live observations for one already-admitted root or
        /// child logical Thread. `None` is the fail-closed default: committed events
        /// remain available, but no Runtime without an exact Thread-scoped observer
        /// can accidentally leak previews across Threads. This port is a view over
        /// the Runtime's existing live hub, not another fan-out or replay owner.
        async fn subscribe_session_thread_live(
            &self,
            session_id: &awaken_session_types::SessionId,
            thread_id: &str,
        ) -> Result<
            Option<Box<dyn awaken_session_execution_contract::SessionThreadLiveSubscription>>,
            awaken_runtime_contract::runs::RunError,
        > {
            awaken_session_execution_contract::NeutralRuntime
                .subscribe_session_thread_live(session_id, thread_id)
                .await
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.
        /// Admit one Session-approved child report as immutable input of the
        /// deterministic primary Run. Implementations must not stage a parallel
        /// Outbox/Inbox message for this internal Agent-to-Agent transfer.
        async fn continue_session_agent_report(
            &self,
            command: awaken_session_execution_contract::SessionAgentReportContinuation,
        ) -> Result<(), awaken_runtime_contract::runs::RunError> {
            awaken_session_execution_contract::NeutralRuntime
                .continue_session_agent_report(command)
                .await
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.
        /// Interrupt one logical child through its parent Session's physical commit
        /// and dispatch partition. Implementations must not open child-named storage.
        async fn interrupt_session_thread(
            &self,
            session_id: &awaken_session_types::SessionId,
            child_thread_id: &awaken_agent_contract::agent::thread::Id,
        ) -> Result<(), awaken_runtime_contract::runs::RunError> {
            awaken_session_execution_contract::NeutralRuntime
                .interrupt_session_thread(session_id, child_thread_id)
                .await
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.
        /// Reply to the exact pending tool of a Primary or logical child Thread
        /// through the Session-affine claim-fenced resume/commit boundary.
        async fn reply_session_thread_tool(
            &self,
            delivery: awaken_session_contract::SessionThreadToolReplyDelivery,
        ) -> Result<(), awaken_runtime_contract::runs::RunError> {
            awaken_session_execution_contract::NeutralRuntime
                .reply_session_thread_tool(delivery)
                .await
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.
        /// Foreground form of [`Self::reply_session_thread_tool`]. The durable
        /// reply ingress remains identical; implementations additionally register
        /// before publication and project the next committed Step from Thread
        /// truth. No inline executor is authorized by this observation policy.
        async fn reply_and_observe_session_thread_tool(
            &self,
            delivery: awaken_session_contract::SessionThreadToolReplyDelivery,
            stream_sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
        ) -> Result<
            awaken_runtime_contract::runs::StepOutcome,
            awaken_runtime_contract::runs::RunError,
        > {
            awaken_session_execution_contract::NeutralRuntime
                .reply_and_observe_session_thread_tool(delivery, stream_sink)
                .await
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.
        /// Stop the parent Run, wait until it can no longer commit a delegation, and
        /// then read the complete child set from durable runtime authority.
        async fn quiesce_terminal_delegations(
            &self,
            thread: &str,
        ) -> Result<
            awaken_session_application_contract::DelegatedRunSnapshot,
            awaken_runtime_contract::runs::RunError,
        > {
            self.interrupt(thread).await?;
            Ok(awaken_session_application_contract::DelegatedRunSnapshot {
                delegated_runs: self.delegated_runs(thread).await?,
                coordinated_thread_ids: Vec::new(),
                watermark: 0,
                runtime_commit_cursor: self
                    .session_thread_recovery_snapshot(
                        &awaken_session_types::SessionId::from(thread),
                        thread,
                    )
                    .await?
                    .map(|snapshot| snapshot.store_cursor)
                    .unwrap_or_default(),
            })
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.

        /// Commit the absorbing archive disposition on one logical child Thread
        /// through its parent Session partition. Implementations must not mutate a
        /// protocol cache or open child-named storage.
        async fn archive_session_thread(
            &self,
            session_id: &awaken_session_types::SessionId,
            thread_id: &str,
        ) -> Result<(), awaken_runtime_contract::runs::RunError> {
            awaken_session_execution_contract::NeutralRuntime
                .archive_session_thread(session_id, thread_id)
                .await
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_execution_contract::SessionCoordinationReads for Shared {
        async fn source_dispatch_observation(
            &self,
            _session: &awaken_session_contract::PersistedSession,
        ) -> awaken_session_execution_contract::SessionSourceDispatchObservation {
            Default::default()
        }

        async fn coordinated_admission_receipt(
            &self,
            command: awaken_session_execution_contract::CoordinatedAdmissionRef<'_>,
        ) -> Result<
            Option<awaken_session_execution_contract::SessionAgentMessageReceipt>,
            awaken_runtime_contract::runs::RunError,
        > {
            awaken_session_execution_contract::NeutralRuntime
                .coordinated_admission_receipt(command)
                .await
        }

        async fn session_run_state(
            &self,
            session_id: &SessionId,
            run_id: &awaken_agent_contract::agent::run::Id,
        ) -> Result<Option<awaken_agent_contract::agent::run::RunState>, RunError> {
            self.0.session_run_state(session_id, run_id).await
        }

        /// Read the runtime-owned, durable child-Run relationships for `thread`.
        /// Protocol adapters use this only to rebuild disposable projections after a
        /// restart; the runtime relationship registry remains the sole authority.
        async fn delegated_runs(
            &self,
            thread: &str,
        ) -> Result<
            Vec<awaken_runtime_contract::runs::DelegatedRun>,
            awaken_runtime_contract::runs::RunError,
        > {
            awaken_session_execution_contract::NeutralRuntime
                .delegated_runs(thread)
                .await
        }

        /// Rebuild the Session's coordinated child-Thread links from existing
        /// committed ToolBatch/transcript/dispatch facts. This is a query, not a
        /// relationship repository.
        /// `observed_root` may borrow an atomic prefix already read for this Session
        /// during a larger read-only projection. Implementations must reject a
        /// different root identity and fence enrichment to that prefix. `None`
        /// reads the current prefix; command admission and final validation use it.
        async fn coordinated_threads(
            &self,
            session_id: &awaken_session_types::SessionId,
            observed_root: Option<
                &awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot,
            >,
        ) -> Result<
            Vec<awaken_session_execution_contract::CoordinatedThreadLink>,
            awaken_runtime_contract::runs::RunError,
        > {
            awaken_session_execution_contract::NeutralRuntime
                .coordinated_threads(session_id, observed_root)
                .await
        }

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
            awaken_runtime_contract::runs::RunError,
        > {
            awaken_session_execution_contract::NeutralRuntime
                .session_thread_tool_reply_fence(command)
                .await
        }

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
            command: &awaken_session_execution_contract::SessionAgentMessageCommand,
        ) -> Result<
            awaken_session_execution_contract::SessionAgentMessageCommand,
            awaken_runtime_contract::runs::RunError,
        > {
            awaken_session_execution_contract::NeutralRuntime
                .validate_session_agent_message_source(command)
                .await
        }

        async fn session_run_command_receipt(
            &self,
            command: &awaken_session_execution_contract::AdmitSessionRun,
        ) -> Result<
            Option<awaken_session_execution_contract::SessionRunReservation>,
            awaken_runtime_contract::runs::RunError,
        > {
            awaken_session_execution_contract::SessionCoordinationReads::session_run_command_receipt(
                self.0.as_ref(),
                command,
            )
            .await
        }

        async fn task_callback_source(
            &self,
            session_id: &awaken_session_types::SessionId,
            thread_id: &awaken_agent_contract::ThreadId,
            root_agent_id: &str,
        ) -> Result<
            Option<(
                String,
                awaken_agent_contract::agent::delegation::DelegationOrigin,
            )>,
            awaken_runtime_contract::runs::RunError,
        > {
            awaken_session_execution_contract::SessionCoordinationReads::task_callback_source(
                self.0.as_ref(),
                session_id,
                thread_id,
                root_agent_id,
            )
            .await
        }

        async fn task_origin_execution(
            &self,
            session_id: &awaken_session_types::SessionId,
            thread_id: &awaken_agent_contract::ThreadId,
            origin_run_id: &awaken_agent_contract::RunId,
        ) -> Result<
            Box<(
                awaken_runtime_contract::RunActivation,
                Vec<awaken_runtime_contract::ExecutableAgentSnapshot>,
            )>,
            awaken_runtime_contract::runs::RunError,
        > {
            awaken_session_execution_contract::SessionCoordinationReads::task_origin_execution(
                self.0.as_ref(),
                session_id,
                thread_id,
                origin_run_id,
            )
            .await
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_execution_contract::ThreadReads for Shared {
        async fn transcript_page(
            &self,
            _session: &::awaken_session_types::SessionId,
            _request: &::awaken_agent_contract::thread::read::transcript::TranscriptPageRequest,
        ) -> Result<
            ::awaken_agent_contract::thread::read::transcript::TranscriptPage,
            ::awaken_runtime_contract::runs::RunError,
        > {
            Err(::awaken_runtime_contract::runs::RunError::unavailable(
                "transcript pagination is unsupported",
            ))
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.
        /// True when durable truth already exists for `thread`. Session-id minting
        /// consults this to skip ids a previous process persisted; implementations
        /// MUST answer without materializing any per-thread state (no context
        /// build, no cache entry) — probing must be free of side effects. The
        /// default reports nothing, so an ephemeral host mints densely from 0.
        async fn owns_thread(
            &self,
            thread: &str,
        ) -> Result<bool, awaken_runtime_contract::runs::RunError> {
            awaken_session_execution_contract::NeutralRuntime
                .owns_thread(thread)
                .await
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
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
            awaken_runtime_contract::runs::RunError,
        > {
            awaken_session_execution_contract::NeutralRuntime
                .committed_messages(thread)
                .await
        }

        async fn session_thread_recovery_snapshot(
            &self,
            session_id: &SessionId,
            thread_id: &str,
        ) -> Result<
            Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
            RunError,
        > {
            self.0
                .session_thread_recovery_snapshot(session_id, thread_id)
                .await
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
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
            session_id: &awaken_session_types::SessionId,
            thread_id: &str,
            run_id: &awaken_agent_contract::agent::run::Id,
        ) -> Result<
            Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
            awaken_runtime_contract::runs::RunError,
        > {
            awaken_session_execution_contract::select_recovery_snapshot_run(
                self.session_thread_recovery_snapshot(session_id, thread_id)
                    .await?,
                run_id,
            )
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.
        /// Read the durable disposition of one logical child Thread through its
        /// parent Session's physical commit partition. Legacy Threads without the
        /// state cell remain Active.
        async fn session_thread_disposition(
            &self,
            session_id: &awaken_session_types::SessionId,
            thread_id: &str,
        ) -> Result<awaken_agent_contract::ThreadDisposition, awaken_runtime_contract::runs::RunError>
        {
            awaken_session_execution_contract::NeutralRuntime
                .session_thread_disposition(session_id, thread_id)
                .await
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.
        /// Read one exact terminal Outcome projection from committed Thread truth.
        /// `None` means the id is absent or still active. Implementations must not
        /// resume the Outcome or reconstruct a terminal from an in-process
        /// [`awaken_runtime_contract::runs::StepOutcome`].
        async fn committed_outcome_projection(
            &self,
            thread: &str,
            outcome_id: &str,
        ) -> Result<
            Option<awaken_session_execution_contract::CommittedOutcomeProjection>,
            awaken_runtime_contract::runs::RunError,
        > {
            awaken_session_execution_contract::NeutralRuntime
                .committed_outcome_projection(thread, outcome_id)
                .await
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_execution_contract::Outcomes for Shared {
        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
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
        ) -> Result<u64, awaken_runtime_contract::runs::RunError> {
            awaken_session_execution_contract::NeutralRuntime
                .prepare_outcome(thread, outcome_id, description, rubric, max_iterations)
                .await
        }

        async fn define_outcome(
            &self,
            t: &str,
            d: &str,
            r: &str,
            m: u32,
            executor: &dyn awaken_session_execution_contract::PreparedSessionRunAdmission,
        ) -> Result<OutcomeDrive, RunError> {
            self.0.define_outcome(t, d, r, m, executor).await
        }

        // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
        // answered this role method with anything but the refusal below.
        /// Continue the active Outcome after the ordinary Run resume has committed.
        /// `None` proves there is no active aggregate; implementations must rebuild
        /// continuation exclusively from the Thread-owned Outcome state.
        async fn continue_outcome(
            &self,
            thread: &str,
            executor: &dyn awaken_session_execution_contract::PreparedSessionRunAdmission,
        ) -> Result<
            Option<awaken_session_execution_contract::OutcomeDrive>,
            awaken_runtime_contract::runs::RunError,
        > {
            awaken_session_execution_contract::NeutralRuntime
                .continue_outcome(thread, executor)
                .await
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_execution_contract::LiveInboxAccess for Shared {
        async fn live_inbox_snapshot(&self, t: &str) -> Result<LiveInboxSnapshot, LiveInboxError> {
            self.0.live_inbox_snapshot(t).await
        }

        async fn live_inbox_queue(
            &self,
            t: &str,
            c: Vec<ContentBlock>,
        ) -> Result<u64, LiveInboxError> {
            self.0.live_inbox_queue(t, c).await
        }

        async fn live_inbox_remove(&self, t: &str, id: u64) -> Result<(), LiveInboxError> {
            self.0.live_inbox_remove(t, id).await
        }

        async fn live_inbox_replace(
            &self,
            t: &str,
            id: u64,
            c: Vec<ContentBlock>,
        ) -> Result<(), LiveInboxError> {
            self.0.live_inbox_replace(t, id, c).await
        }

        async fn live_inbox_reorder(&self, t: &str, o: Vec<u64>) -> Result<(), LiveInboxError> {
            self.0.live_inbox_reorder(t, o).await
        }
    }
    let state = Arc::new(ManagedState::new(Shared(fake)).await);
    let application = state.session_application();
    (
        router(state).merge(live_inbox_router(application.inputs())),
        application,
    )
}

async fn app(fake: Arc<QueueFake>) -> Router {
    app_with_session_application(fake).await.0
}

#[tokio::test]
async fn workspace_scope_fences_every_live_inbox_operation() {
    // FMECA cause/effect graph:
    // C1 Session exists; C2 request scope is missing, foreign, or owning; C3
    // operation is read or mutation. Effects: E1 missing/foreign always returns
    // indistinguishable 404; E2 owning reads/mutations reach the queue; E3 a
    // rejected request causes no queue side effect.
    //
    // | Rule | Session | scope | verb | effect |
    // |---|---|---|---|---|
    // | S1 | exists | missing | GET | 404, no disclosure |
    // | S2 | exists | foreign | POST | 404, no mutation |
    // | S3 | exists | owner | POST | accepted by active queue |
    let fake = Arc::new(QueueFake::default());
    fake.active.store(true, Ordering::SeqCst);
    let app = app(fake.clone()).await;
    let session = create(&app).await;
    let base = format!("/v1/awaken/sessions/{session}/live-inbox");

    let (status, _) = call_in_scope(&app, "GET", &base, serde_json::Value::Null, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "S1");

    let (status, _) = call_in_scope(
        &app,
        "POST",
        &base,
        text_content("foreign"),
        Some("workspace-b"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "S2");

    // S4 crosses all five verbs with missing/foreign scopes. Every refused
    // request must stop before polling the selected Inbox port, including reads
    // and well-formed edit commands. The owner round trip below
    // still uses the same queue and confirms rejection consumed no id.
    let before_calls = fake.live_calls.load(Ordering::SeqCst);
    for scope in [None, Some("workspace-b")] {
        for (method, suffix, body) in [
            ("GET", "", serde_json::Value::Null),
            ("POST", "", text_content("rejected")),
            ("PUT", "/1", text_content("rejected-edit")),
            ("DELETE", "/1", serde_json::Value::Null),
            ("PUT", "/order", serde_json::json!({ "order": [] })),
        ] {
            let (status, _) =
                call_in_scope(&app, method, &format!("{base}{suffix}"), body, scope).await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "S4 {scope:?} {method} {suffix}"
            );
            assert_eq!(
                fake.live_calls.load(Ordering::SeqCst),
                before_calls,
                "S4 no Inbox poll"
            );
        }
    }

    let (status, queued) = call(&app, "POST", &base, text_content("owner")).await;
    assert_eq!(status, StatusCode::OK, "S3");
    assert_eq!(queued["id"], 1, "S2 must not consume a queue id");
}

#[tokio::test]
async fn malformed_live_inbox_commands_are_rejected_before_application_mutation() {
    // FMECA parser-boundary graph: C1 request has the required content/order;
    // C2 it contains an unknown field (version skew or injection); C3 the queue
    // is active. Effects: E1 exact command mutates once; E2 malformed command is
    // a 4xx and never reaches the application. Rule P1 C1+C3 -> E1; P2 C2 -> E2.
    let fake = Arc::new(QueueFake::default());
    fake.active.store(true, Ordering::SeqCst);
    let app = app(fake).await;
    let session = create(&app).await;
    let base = format!("/v1/awaken/sessions/{session}/live-inbox");

    let (status, _) = call(
        &app,
        "POST",
        &base,
        serde_json::json!({
            "content": [{"type": "text", "text": "must-not-land"}],
            "unexpected": true
        }),
    )
    .await;
    assert!(status.is_client_error(), "P2: {status}");

    let (status, snapshot) = call(&app, "GET", &base, serde_json::Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(snapshot["messages"].as_array().unwrap().len(), 0, "P2/E2");
}

#[tokio::test]
async fn unknown_session_is_404_before_any_queue_logic() {
    // Decision rule R1: C1 the neutral application reports an unknown Session
    // -> E1 the Awaken adapter returns 404 and never consults queue state.
    let app = app(Arc::new(QueueFake::default())).await;
    let (status, body) = call(
        &app,
        "GET",
        "/v1/awaken/sessions/sess_missing/live-inbox",
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["type"], "not_found_error");
}

#[tokio::test]
async fn inactive_queue_lists_empty_and_refuses_mutations_with_410() {
    // Decision rules R2/R3: C1 known Session + inactive queue + read -> E1 an
    // empty inactive snapshot; the same state + mutation -> E2 410 with no edit.
    let fake = Arc::new(QueueFake::default());
    let app = app(fake).await;
    let session = create(&app).await;

    let (status, body) = call(
        &app,
        "GET",
        &format!("/v1/awaken/sessions/{session}/live-inbox"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["active"], false);
    assert_eq!(body["messages"].as_array().unwrap().len(), 0);

    let (status, _) = call(
        &app,
        "POST",
        &format!("/v1/awaken/sessions/{session}/live-inbox"),
        text_content("late"),
    )
    .await;
    assert_eq!(status, StatusCode::GONE);

    let (status, _) = call(
        &app,
        "DELETE",
        &format!("/v1/awaken/sessions/{session}/live-inbox/1"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::GONE);
}

#[tokio::test]
async fn inactive_live_steer_has_the_existing_durable_event_fallback() {
    // Causes: the fixtures below establish `inactive live steer has the existing durable event
    // fallback` with the concrete inputs, state, dependencies, and failure triggers used by this
    // case.
    // Constraints/invariants: the Coordinator routes one neutral Session/Run lifecycle; live
    // delivery is best-effort and cannot replace committed replay truth.
    // Decision rule: evaluate every labeled cause partition in this test; each matching rule
    // selects only its stated effect and preserves the authority constraint.
    // End-to-end FMECA rule: C1 no locally reachable active attempt; C2 caller
    // first tries best-effort steer; C3 caller retries the content through the
    // ordinary Session event ingress. Effects: E1 live steer returns 410 without
    // consuming content; E2 the existing event command accepts and processes the
    // message exactly once; E3 committed event history, not LiveInbox, owns truth.
    //
    // | Rule | live active | selected ingress | effect |
    // |---|---|---|---|
    // | F1 | no | LiveInbox | 410, no queue entry |
    // | F2 | no | Session events | 200, one committed user.message |
    let (app, session_application) =
        app_with_session_application(Arc::new(QueueFake::default())).await;
    let session = create(&app).await;
    let live = format!("/v1/awaken/sessions/{session}/live-inbox");

    let (status, _) = call(&app, "POST", &live, text_content("reliable")).await;
    assert_eq!(status, StatusCode::GONE, "F1");

    let (status, body) = call(
        &app,
        "POST",
        &format!("/v1/sessions/{session}/events"),
        serde_json::json!({
            "events": [{
                "type": "user.message",
                "content": [{"type": "text", "text": "reliable"}]
            }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "F2: {body}");
    session_application
        .event_batches()
        .drive_session_event_batches(&SessionId::from(&session), None)
        .await
        .expect("F2 lifecycle drive");

    let (status, events) = call(
        &app,
        "GET",
        &format!("/v1/sessions/{session}/events"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let committed = events["data"]
        .as_array()
        .expect("event page")
        .iter()
        .filter(|event| event["type"] == "user.message")
        .count();
    assert_eq!(committed, 1, "F2/E3");
}

#[tokio::test]
async fn queue_edit_reorder_withdraw_round_trip() {
    // Cause/effect graph: active queue -> append stable ids -> replace preserves
    // identity/position -> full reorder commits -> stale reorder is 409 -> first
    // remove is terminal success and repeated remove is 404. These rules cover
    // all five methods through one neutral LiveInboxApplication owner.
    let fake = Arc::new(QueueFake::default());
    fake.active.store(true, Ordering::SeqCst);
    let app = app(fake).await;
    let session = create(&app).await;
    let base = format!("/v1/awaken/sessions/{session}/live-inbox");

    // Queue two messages.
    let (status, first) = call(&app, "POST", &base, text_content("one")).await;
    assert_eq!(status, StatusCode::OK);
    let first = first["id"].as_u64().unwrap();
    let (_, second) = call(&app, "POST", &base, text_content("two")).await;
    let second = second["id"].as_u64().unwrap();

    // Snapshot shows both, in order.
    let (_, snap) = call(&app, "GET", &base, serde_json::Value::Null).await;
    assert_eq!(snap["active"], true);
    let listed: Vec<u64> = snap["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_u64().unwrap())
        .collect();
    assert_eq!(listed, [first, second]);

    // Replace keeps id and position.
    let (status, _) = call(
        &app,
        "PUT",
        &format!("{base}/{first}"),
        text_content("one-edited"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, snap) = call(&app, "GET", &base, serde_json::Value::Null).await;
    assert_eq!(
        snap["messages"][0]["content"][0]["text"], "one-edited",
        "edit landed in place"
    );

    // Reorder: full permutation applies; a stale one is 409.
    let (status, _) = call(
        &app,
        "PUT",
        &format!("{base}/order"),
        serde_json::json!({ "order": [second, first] }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &app,
        "PUT",
        &format!("{base}/order"),
        serde_json::json!({ "order": [second] }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Withdraw one; withdrawing it again is 404.
    let (status, _) = call(
        &app,
        "DELETE",
        &format!("{base}/{first}"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &app,
        "DELETE",
        &format!("{base}/{first}"),
        serde_json::Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (_, snap) = call(&app, "GET", &base, serde_json::Value::Null).await;
    let remaining: Vec<u64> = snap["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_u64().unwrap())
        .collect();
    assert_eq!(
        remaining,
        [second],
        "reorder then withdraw left `two` alone"
    );
}
