//! Original committed-event fixture; all existing flags and role bodies stay together.
use async_trait::async_trait;
use awaken_agent_contract::agent::awaiting::PermissionDecision;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::{Id as RunId, RunState};
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::{RunLifecycleCursor, RunLifecycleEvent};
use awaken_session_application_contract::{Pending, StepOutcome};
use awaken_session_execution_contract::{CoordinatedThreadLink, OutcomeDrive, SessionRuns};
use awaken_session_execution_contract::{
    SessionCoordinationReads as _, StreamingRuns as _, ThreadReads as _,
};
use awaken_session_types::{RunError, SessionId};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub(super) struct LifecycleRuntime {
    pub(super) messages: Arc<Mutex<Vec<Message>>>,
    pub(super) message_cursors: Arc<Mutex<Vec<u64>>>,
    pub(super) audit_events: Arc<Mutex<Vec<awaken_agent_contract::audit::record::Record>>>,
    pub(super) lifecycle: Arc<Mutex<Vec<RunLifecycleEvent>>>,
    pub(super) state: Arc<Mutex<Vec<awaken_agent_contract::agent::state::Command>>>,
    pub(super) state_cursors: Arc<Mutex<Vec<u64>>>,
    pub(super) outcome_projections: Arc<
        Mutex<
            std::collections::HashMap<
                String,
                awaken_session_execution_contract::CommittedOutcomeProjection,
            >,
        >,
    >,
    /// Causal test hook: expose one complete Outcome commit when its terminal
    /// projection is observed, modeling a commit between the projection and
    /// root-snapshot reads without manufacturing a partial transaction.
    pub(super) outcome_commit_on_read:
        Arc<Mutex<Vec<(awaken_agent_contract::agent::state::Command, u64)>>>,
    /// Pause one existing Runtime read so a real root CAS can win while the
    /// original transport has already begun cold recovery. No receipt is faked.
    pub(super) reads: crate::state::test_support::ReadControl,
    pub(super) pending: Arc<Mutex<Option<Pending>>>,
    /// The audited target of every pause a live ticket opened (see
    /// `awaken_protocol_managed_testkit::remember_ticket_await_target`).
    pub(super) await_targets: Arc<Mutex<awaken_protocol_managed_testkit::AwaitTargets>>,
    /// Awaiting commits that model a legacy audit row without a target.
    pub(super) unaudited_awaits: Arc<Mutex<std::collections::HashSet<RunLifecycleCursor>>>,
    /// Advance only the committed Thread coordinate for a finite/infinite burst
    /// at the existing snapshot port; zero preserves every ordinary fixture.
    pub(super) projection_version_changes: Arc<AtomicUsize>,
    pub(super) split_message_reads: Arc<AtomicUsize>,
    pub(super) split_pending_reads: Arc<AtomicUsize>,
    pub(super) coordinated_roots: Arc<Mutex<Vec<Option<u64>>>>,
}

impl LifecycleRuntime {
    /// Install one atomic committed-message prefix with the exact Runtime
    /// commit coordinates consumed by the production projector. Keeping the
    /// two vectors together prevents a test from inventing a transcript that
    /// no production recovery backend can return.
    pub(super) fn replace_committed_messages(&self, entries: Vec<(u64, Message)>) {
        let (cursors, messages): (Vec<_>, Vec<_>) = entries.into_iter().unzip();
        *self.messages.lock().unwrap() = messages;
        *self.message_cursors.lock().unwrap() = cursors;
    }

    pub(super) fn push_committed_message(&self, cursor: u64, message: Message) {
        let mut messages = self.messages.lock().unwrap();
        let mut cursors = self.message_cursors.lock().unwrap();
        assert_eq!(
            messages.len(),
            cursors.len(),
            "fixture transcript and commit coordinates must stay aligned"
        );
        messages.push(message);
        cursors.push(cursor);
    }
}

#[async_trait]
impl SessionRuns for LifecycleRuntime {
    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        unreachable!()
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: PermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        unreachable!()
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<ContentBlock>,
        _is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        unreachable!()
    }

    fn model(&self) -> String {
        "test-model".into()
    }
}

awaken_session_execution_contract::neutral_role!(BudgetAccounting for LifecycleRuntime);

awaken_session_execution_contract::neutral_role!(EnvironmentRuntime for LifecycleRuntime);

awaken_session_execution_contract::neutral_role!(TerminalCleanup for LifecycleRuntime);

awaken_session_execution_contract::neutral_role!(LiveInboxAccess for LifecycleRuntime);

awaken_session_execution_contract::neutral_role!(CapabilityRuntime for LifecycleRuntime);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for LifecycleRuntime {
    fn observe_realization_request(
        &self,
    ) -> Result<
        awaken_lease_contract::authority::AuthorityProofStarted,
        awaken_session_types::RunError,
    > {
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
    ) -> Result<usize, awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_projections(control, owner, runtime_incarnation, timing)
            .await
    }

    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_execution_contract::FrozenSessionProjection,
        mode: awaken_session_execution_contract::SessionProjectionInstallMode,
    ) -> Result<(), awaken_session_execution_contract::SessionRealizationEffectFailure> {
        awaken_protocol_managed_testkit::complete_session_projection_init(
            thread,
            &projection,
            &mode,
        )?;
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
    ) -> Result<(), awaken_session_types::RunError> {
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
        lease: &crate::session_resource::SessionRealizationLease,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .invalidate_session_realization_lease(thread, lease)
            .await
    }
}

awaken_session_execution_contract::neutral_role!(RealizationEffects for LifecycleRuntime);

#[async_trait::async_trait]
impl awaken_session_execution_contract::ChildRunCoordination for LifecycleRuntime {
    // Fixture rule P1: lifecycle scripts observe public Event admission without prepared execution.
    // A prepared command refuses before activity admission or fixture mutation.
    async fn run_prepared_session_run(
        &self,
        command: awaken_session_execution_contract::AdmitPreparedSessionRun,
        activity: &dyn awaken_session_execution_contract::SessionRunActivityAdmitter,
    ) -> Result<awaken_agent_contract::agent::run::RunState, awaken_session_types::RunError> {
        awaken_session_execution_contract::ChildRunCoordination::run_prepared_session_run(
            &awaken_session_execution_contract::NeutralRuntime,
            command,
            activity,
        )
        .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Persist one complete self-affine Session Run as an unclaimable reservation.
    /// Implementations must use their existing durable Run dispatch authority;
    /// the default fails closed rather than executing inline.
    async fn reserve_session_run(
        &self,
        command: awaken_session_execution_contract::AdmitSessionRun,
    ) -> Result<
        awaken_session_execution_contract::SessionRunReservation,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .reserve_session_run(command)
            .await
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
        awaken_session_types::RunError,
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
    ) -> Result<awaken_session_application_contract::StepOutcome, awaken_session_types::RunError>
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
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .admit_coordinated_run(command)
            .await
    }

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
        awaken_session_types::RunError,
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
    ) -> Result<(), awaken_session_types::RunError> {
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
    ) -> Result<(), awaken_session_types::RunError> {
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
    ) -> Result<(), awaken_session_types::RunError> {
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
    ) -> Result<awaken_session_application_contract::StepOutcome, awaken_session_types::RunError>
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
        awaken_session_types::RunError,
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
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .archive_session_thread(session_id, thread_id)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionCoordinationReads for LifecycleRuntime {
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
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .coordinated_admission_receipt(command)
            .await
    }

    async fn session_run_state(
        &self,
        session_id: &SessionId,
        run_id: &RunId,
    ) -> Result<Option<RunState>, RunError> {
        Ok(self
            .lifecycle
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|event| event.thread_id.0 == session_id && &event.run_id == run_id)
            .map(|event| event.state.clone()))
    }

    /// Read the runtime-owned, durable child-Run relationships for `thread`.
    /// Protocol adapters use this only to rebuild disposable projections after a
    /// restart; the runtime relationship registry remains the sole authority.
    async fn delegated_runs(
        &self,
        thread: &str,
    ) -> Result<
        Vec<awaken_session_application_contract::DelegatedRun>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .delegated_runs(thread)
            .await
    }

    async fn coordinated_threads(
        &self,
        session_id: &SessionId,
        observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<Vec<CoordinatedThreadLink>, RunError> {
        if let Some(root) = observed_root {
            assert_eq!(root.thread_id.0, session_id, "query reuses its own root");
        }
        self.coordinated_roots
            .lock()
            .unwrap()
            .push(observed_root.map(|root| root.thread_version));
        Ok(Vec::new())
    }

    /// Read and validate the exact committed Awaiting coordinate that a Primary
    /// or child Thread tool reply intends to answer. The command already carries
    /// its admission-time Run/correlation identity; this returns only the current
    /// dispatch activity coordinate. Implementations revalidate the command when
    /// applying the reply so a concurrent resume or later Run fails closed.
    async fn session_thread_tool_reply_fence(
        &self,
        command: &awaken_session_contract::SessionThreadToolReplyCommand,
    ) -> Result<awaken_session_contract::SessionThreadToolReplyFence, awaken_session_types::RunError>
    {
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
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_agent_message_source(command)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::StreamingRuns for LifecycleRuntime {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Run one request with its protocol-projected, backend-neutral content
    /// owner. Adapters that do not support attributed capture may use the
    /// compatibility default; runtime hosts override it and persist attribution
    /// on the durable activation.
    async fn run_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _data_subject_id: Option<String>,
    ) -> Result<awaken_session_application_contract::StepOutcome, awaken_session_types::RunError>
    {
        self.run(agent, thread, content).await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Run one user request, installing `sink` as the Run's best-effort live-progress
    /// channel so the adapter can project in-flight `stream::Kind` into
    /// `event_start`/`event_delta` previews. The committed [`awaken_session_application_contract::StepOutcome`] is
    /// identical to [`run`](Self::run) — the sink only mirrors in-flight events. The
    /// default ignores the sink and delegates to `run`, so a host without a
    /// streaming path (or a test double) is unaffected.
    async fn run_streaming(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _sink: std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<awaken_session_application_contract::StepOutcome, awaken_session_types::RunError>
    {
        self.run(agent, thread, content).await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Streaming counterpart of [`run_attributed`](Self::run_attributed).
    async fn run_streaming_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _data_subject_id: Option<String>,
        sink: std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<awaken_session_application_contract::StepOutcome, awaken_session_types::RunError>
    {
        self.run_streaming(agent, thread, content, sink).await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Interrupt the run in flight on `thread` (a `user.interrupt`): cancel it so
    /// an in-progress outcome ends `interrupted`. A no-op when nothing is running.
    async fn interrupt(&self, thread: &str) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .interrupt(thread)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Rebind `thread` to `model` for its subsequent Runs (R5, per-Run override).
    /// The default is a no-op, so a host without per-thread model routing is
    /// unaffected; the server impl re-stages the thread's model and evicts the
    /// cached context so the next Run resolves the new executor.
    async fn rebind_model(
        &self,
        thread: &str,
        model: &str,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .rebind_model(thread, model)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Whether the thread's selected model accepts a system message after the
    /// conversation has started. Runtimes whose models do not support this can
    /// fail admission before the Managed adapter persists the inbound event.
    async fn supports_mid_conversation_system(
        &self,
        thread: &str,
    ) -> Result<bool, awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .supports_mid_conversation_system(thread)
            .await
    }

    async fn pending_tool(&self, _thread: &str) -> Result<Option<Pending>, RunError> {
        self.split_pending_reads.fetch_add(1, Ordering::SeqCst);
        Ok(self.pending.lock().unwrap().clone())
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::ThreadReads for LifecycleRuntime {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// True when durable truth already exists for `thread`. Session-id minting
    /// consults this to skip ids a previous process persisted; implementations
    /// MUST answer without materializing any per-thread state (no context
    /// build, no cache entry) — probing must be free of side effects. The
    /// default reports nothing, so an ephemeral host mints densely from 0.
    async fn owns_thread(&self, thread: &str) -> Result<bool, awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .owns_thread(thread)
            .await
    }

    async fn committed_messages(&self, _thread: &str) -> Result<Vec<Message>, RunError> {
        self.split_message_reads.fetch_add(1, Ordering::SeqCst);
        Ok(self.messages.lock().unwrap().clone())
    }

    async fn session_thread_recovery_snapshot(
        &self,
        _session_id: &SessionId,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        let read_index = self
            .reads
            .on_read("recovery")
            .await
            .map_err(|_| RunError::unavailable("scripted cold recovery snapshot failure"))?;
        let lifecycle = self.lifecycle.lock().unwrap().clone();
        let Some(run_id) = lifecycle
            .iter()
            .rev()
            .find(|event| event.thread_id.0 == thread_id)
            .map(|event| event.run_id.clone())
        else {
            return Ok(None);
        };
        let resume_tickets: Vec<awaken_agent_contract::thread::read::recovery::RunResumeTicket> = self
            .pending
            .lock()
            .unwrap()
            .clone()
            .map(
                |pending| awaken_agent_contract::thread::read::recovery::RunResumeTicket {
                    run_id: run_id.clone(),
                    ticket: awaken_agent_contract::agent::awaiting::ResumeTicket::new(
                        format!("ticket:{}", pending.tool_use_id),
                        run_id.clone(),
                        ThreadId(thread_id.to_string()),
                        "test-snapshot",
                        "test-catalog",
                        awaken_agent_contract::agent::awaiting::AwaitTarget::ToolCall {
                            reason: if pending.client_executed {
                                awaken_agent_contract::agent::awaiting::ToolAwaitReason::ClientExecution
                            } else {
                                awaken_agent_contract::agent::awaiting::ToolAwaitReason::Permission
                            },
                            call_id: pending.tool_use_id,
                            tool: awaken_agent_contract::agent::awaiting::PendingTool {
                                tool_id: pending.name,
                                arguments: pending.input,
                            },
                            authority: awaken_agent_contract::agent::awaiting::ToolCallAuthority::RuntimeToolBatch,
                        },
                    ),
                },
            )
            .into_iter()
            .collect();
        let runs = {
            let mut runs = Vec::<awaken_agent_contract::agent::run::Record>::new();
            for event in lifecycle
                .iter()
                .filter(|event| event.thread_id.0 == thread_id)
            {
                if let Some(run) = runs.iter_mut().find(|run| run.id == event.run_id) {
                    run.state = event.state.clone();
                } else {
                    runs.push(awaken_agent_contract::agent::run::Record {
                        id: event.run_id.clone(),
                        thread_id: event.thread_id.clone(),
                        state: event.state.clone(),
                    });
                }
            }
            runs
        };
        let messages = self.messages.lock().unwrap().clone();
        let message_commit_cursors = self.message_cursors.lock().unwrap().clone();
        let state = self.state.lock().unwrap().clone();
        let state_commit_cursors = self.state_cursors.lock().unwrap().clone();
        let store_cursor = lifecycle
            .iter()
            .map(|event| event.source_commit_cursor)
            .chain(message_commit_cursors.iter().copied())
            .chain(state_commit_cursors.iter().copied())
            .max()
            .unwrap_or_default();
        // One fixture commit owns transitions and enriched audit payloads.
        // An explicit row supplies details such as an historical await target
        // at that exact coordinate; it cannot exist only on a second feed.
        let mut audit = awaken_protocol_managed_testkit::fixture_thread_audit(
            &lifecycle,
            thread_id,
            u64::MAX,
            resume_tickets.first().map(|ticket| ticket.ticket.target()),
            &self.unaudited_awaits.lock().unwrap(),
            &mut self.await_targets.lock().unwrap(),
        );
        for explicit in self.audit_events.lock().unwrap().iter().cloned() {
            if let Some(existing) = audit.iter_mut().find(|record| {
                record.sequence == explicit.sequence && record.run_id == explicit.run_id
            }) {
                *existing = explicit;
            } else {
                audit.push(explicit);
            }
        }
        audit.sort_by_key(|record| record.sequence);
        Ok(Some(
            awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot {
                commit_timestamps: Default::default(),
                thread_id: ThreadId(thread_id.to_string()),
                claimed_run_id: run_id.clone(),
                runs,
                latest_run_id: Some(run_id),
                messages,
                message_commit_cursors,
                state,
                state_commit_cursors,
                events: audit,
                resume_tickets,
                thread_version: lifecycle.len() as u64
                    + read_index.min(self.projection_version_changes.load(Ordering::SeqCst)) as u64,
                store_cursor,
                next_commit_ordinal: 0,
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
        awaken_session_types::RunError,
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
    ) -> Result<awaken_agent_contract::ThreadDisposition, awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .session_thread_disposition(session_id, thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.

    async fn committed_outcome_projection(
        &self,
        _thread: &str,
        outcome_id: &str,
    ) -> Result<Option<awaken_session_execution_contract::CommittedOutcomeProjection>, RunError>
    {
        let projection = self
            .outcome_projections
            .lock()
            .unwrap()
            .get(outcome_id)
            .cloned();
        if projection.is_some() {
            let committed = std::mem::take(&mut *self.outcome_commit_on_read.lock().unwrap());
            let (commands, cursors): (Vec<_>, Vec<_>) = committed.into_iter().unzip();
            self.state.lock().unwrap().extend(commands);
            self.state_cursors.lock().unwrap().extend(cursors);
        }
        Ok(projection)
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::Outcomes for LifecycleRuntime {
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
    ) -> Result<u64, awaken_session_types::RunError> {
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
        unreachable!()
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
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .continue_outcome(thread, executor)
            .await
    }
}
