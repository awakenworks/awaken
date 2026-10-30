use super::test_support::{RehydrateFake, create_session_fixture, ephemeral_session_repo};
use super::*;
use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;
use awaken_service_lifecycle::run_composed_async_test;
use awaken_session_execution_contract::ToolPermissionDecision;
use std::collections::BTreeSet;

async fn ephemeral_resource_registry() -> awaken_resource_application::RegistryApplication {
    let storage = Arc::new(
        awaken_resource_store::SqliteResourceStore::in_memory()
            .await
            .expect("open ephemeral Resource Registry"),
    );
    awaken_resource_application::RegistryApplication::new(storage, awaken_time::system_clock())
}

/// A runtime that records every `end_session` thread it is asked to tear down,
/// so a test can prove the terminal edges (delete/archive) reach the host's
/// sandbox disposal rather than leaking it. Every driving method is unused.
#[derive(Clone, Default)]
struct EndSessionRecorder {
    ended: Arc<std::sync::Mutex<Vec<String>>>,
    interrupted: Arc<std::sync::Mutex<Vec<String>>>,
    prepared: Arc<std::sync::Mutex<Vec<String>>>,
    block_prepare: Arc<std::sync::atomic::AtomicBool>,
    delegated: Arc<std::sync::Mutex<Vec<awaken_session_application_contract::DelegatedRun>>>,
    block_quiesce: Arc<std::sync::atomic::AtomicBool>,
    quiesce_entered: Arc<tokio::sync::Notify>,
    quiesce_release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl SessionRuntime for EndSessionRecorder {
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
        _decision: ToolPermissionDecision,
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
        "host-default-model".to_string()
    }
}

awaken_session_execution_contract::neutral_role!(BudgetRuntime for EndSessionRecorder);

awaken_session_execution_contract::neutral_role!(EnvironmentRuntime for EndSessionRecorder);

awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for EndSessionRecorder);

awaken_session_execution_contract::neutral_role!(CapabilityRuntime for EndSessionRecorder);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for EndSessionRecorder {
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
        control: &dyn awaken_session_realization_contract::SessionRealizationControl,
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
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        mode: awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), awaken_session_realization_contract::SessionRealizationEffectFailure> {
        while self.block_prepare.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        if let Some(init) = awaken_protocol_managed_testkit::complete_session_projection_init(
            thread,
            &projection,
            &mode,
        )? {
            let _ = init;
            self.prepared.lock().unwrap().push(thread.to_string());
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
        lease: awaken_session_realization_contract::ObservedSessionRealizationLease,
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

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationEffects for EndSessionRecorder {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Validate the exact provider-effective Sandbox layout without publishing
    /// process-local state or performing provider, cache, Skill, File, Vault,
    /// Repository, or Git I/O. Session creation and Resource mutation call this
    /// before their durable/effect boundaries; Runtime repeats it at the final
    /// realization edge as defense in depth.
    fn validate_session_sandbox_layout(
        &self,
        thread: &str,
        layout: &awaken_session_configuration_contract::SessionSandboxLayout,
    ) -> Result<(), awaken_session_types::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_sandbox_layout(thread, layout)
    }

    async fn checkpoint_session_environment(
        &self,
        thread: &str,
        request: awaken_session_execution_contract::__macro_support::SandboxCheckpointRequest,
    ) -> Result<
        awaken_session_execution_contract::__macro_support::CheckpointReceipt,
        awaken_session_execution_contract::__macro_support::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .checkpoint_session_environment(thread, request)
            .await
    }

    async fn prepare_checkpoint_source_disposal(
        &self,
        thread: &str,
        preparation: &awaken_session_execution_contract::__macro_support::SourceReleasePreparationEffect,
        generation: &awaken_session_execution_contract::__macro_support::SandboxGeneration,
        source_binding: &str,
    ) -> Result<
        awaken_session_execution_contract::__macro_support::SourceReleasePreparedReceipt,
        awaken_session_execution_contract::__macro_support::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .prepare_checkpoint_source_disposal(thread, preparation, generation, source_binding)
            .await
    }

    async fn dispose_prepared_checkpoint_source(
        &self,
        thread: &str,
        disposal: &awaken_session_execution_contract::__macro_support::SourceReleaseDisposal,
    ) -> Result<
        awaken_session_execution_contract::__macro_support::SourceDisposedReceipt,
        awaken_session_execution_contract::__macro_support::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .dispose_prepared_checkpoint_source(thread, disposal)
            .await
    }

    async fn restore_checkpointed_session_environment(
        &self,
        request: awaken_session_execution_contract::__macro_support::SandboxRestoreRequest,
    ) -> Result<
        awaken_session_execution_contract::__macro_support::RestoreReceipt,
        awaken_session_execution_contract::__macro_support::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .restore_checkpointed_session_environment(request)
            .await
    }

    async fn delete_session_checkpoint(
        &self,
        thread: &str,
        checkpoint: &awaken_session_execution_contract::__macro_support::SandboxCheckpointRef,
    ) -> Result<(), awaken_session_execution_contract::__macro_support::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .delete_session_checkpoint(thread, checkpoint)
            .await
    }

    async fn apply_session_inputs(
        &self,
        thread: &str,
        transition: &awaken_session_execution_contract::__macro_support::SessionResourceTransition,
    ) -> Result<(), awaken_session_execution_contract::__macro_support::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .apply_session_inputs(thread, transition)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
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
        command: awaken_session_state_contract::SessionRepositoryPublicationCommand,
        lease: &crate::session_resource::SessionRealizationLease,
    ) -> Result<
        awaken_session_state_contract::SessionRepositoryPublicationEffect,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .execute_terminal_repository_publication_for_lease(command, lease)
            .await
    }

    async fn dispose_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) -> Result<awaken_session_state_contract::SessionCleanupDisposalReceipt, RunError> {
        Ok(awaken_protocol_managed_testkit::complete_terminal_cleanup_disposal(&effect))
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::CoordinationRuntime for EndSessionRecorder {
    // Fixture rule P1: terminal tests record end-session effects without prepared execution.
    // A prepared command refuses before activity admission or fixture mutation.
    async fn run_prepared_session_run(
        &self,
        command: awaken_session_execution_contract::AdmitPreparedSessionRun,
        activity: &dyn awaken_session_execution_contract::SessionRunActivityAdmitter,
    ) -> Result<awaken_agent_contract::agent::run::RunState, awaken_session_types::RunError> {
        awaken_session_execution_contract::CoordinationRuntime::run_prepared_session_run(
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
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
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
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
        stream_sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<awaken_session_application_contract::StepOutcome, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .reply_and_observe_session_thread_tool(delivery, stream_sink)
            .await
    }

    async fn quiesce_terminal_delegations(
        &self,
        _thread: &str,
    ) -> Result<awaken_session_application_contract::DelegatedRunSnapshot, RunError> {
        if self.block_quiesce.load(std::sync::atomic::Ordering::SeqCst) {
            self.quiesce_entered.notify_one();
            self.quiesce_release.notified().await;
        }
        Ok(awaken_session_application_contract::DelegatedRunSnapshot {
            delegated_runs: self.delegated.lock().unwrap().clone(),
            coordinated_thread_ids: Vec::new(),
            watermark: 0,
            runtime_commit_cursor: 0,
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
impl awaken_session_execution_contract::SessionCoordinationReads for EndSessionRecorder {
    async fn source_dispatch_observation(
        &self,
        _session: &awaken_session_state_contract::PersistedSession,
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

    /// Read the exact Run lifecycle from existing committed Thread truth. This
    /// is the recovery observation for batch advancement, not a completion
    /// registry or protocol event cache.
    async fn session_run_state(
        &self,
        session_id: &awaken_session_types::SessionId,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<awaken_agent_contract::agent::run::RunState>, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .session_run_state(session_id, run_id)
            .await
    }

    async fn delegated_runs(
        &self,
        _thread: &str,
    ) -> Result<Vec<awaken_session_application_contract::DelegatedRun>, RunError> {
        Ok(self.delegated.lock().unwrap().clone())
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
        awaken_session_types::RunError,
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
        command: &awaken_session_execution_contract::SessionThreadToolReplyCommand,
    ) -> Result<
        awaken_session_execution_contract::SessionThreadToolReplyFence,
        awaken_session_types::RunError,
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
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_agent_message_source(command)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::StreamingRunRuntime for EndSessionRecorder {
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

    async fn interrupt(&self, thread: &str) -> Result<(), RunError> {
        self.interrupted.lock().unwrap().push(thread.to_string());
        Ok(())
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

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// The tool currently blocking `thread`, if any. Admission uses this
    /// read-only projection to reject events that cannot legally precede the
    /// matching result; resume remains the sole mutating authority.
    async fn pending_tool(
        &self,
        thread: &str,
    ) -> Result<Option<awaken_session_application_contract::Pending>, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .pending_tool(thread)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::ThreadReadRuntime for EndSessionRecorder {
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

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// The committed transcript for `thread`, in commit order. Used to rehydrate a
    /// session whose in-memory record was lost (e.g. after a process restart) from
    /// durable truth: a non-empty result means the thread exists in the store. The
    /// default reports nothing, so an ephemeral host never rehydrates.
    async fn committed_messages(
        &self,
        thread: &str,
    ) -> Result<Vec<awaken_agent_contract::agent::message::Message>, awaken_session_types::RunError>
    {
        awaken_session_execution_contract::NeutralRuntime
            .committed_messages(thread)
            .await
    }

    async fn session_thread_recovery_snapshot(
        &self,
        _session_id: &awaken_session_types::SessionId,
        _thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        // Terminal fake rule T0: these tests never commit a Run, so the one
        // consistency owner truthfully returns no snapshot. The production
        // default remains fail-closed and no split messages/ticket fallback is
        // reintroduced merely to make archive/delete tests executable.
        Ok(None)
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

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read one exact terminal Outcome projection from committed Thread truth.
    /// `None` means the id is absent or still active. Implementations must not
    /// resume the Outcome or reconstruct a terminal from an in-process
    /// [`awaken_session_application_contract::StepOutcome`].
    async fn committed_outcome_projection(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<
        Option<awaken_session_execution_contract::CommittedOutcomeProjection>,
        awaken_session_types::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .committed_outcome_projection(thread, outcome_id)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::TerminalCleanupRuntime for EndSessionRecorder {
    async fn prepare_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_state_contract::SessionTerminalCleanupEffect,
        authorization: awaken_session_realization_contract::SessionTerminalCleanupPreparationAuthorization,
    ) -> Result<awaken_session_state_contract::SessionCleanupPreparation, RunError> {
        self.ended
            .lock()
            .unwrap()
            .push(effect.command.thread_id.clone());
        awaken_protocol_managed_testkit::complete_terminal_cleanup_preparation(
            &effect,
            &authorization,
        )
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Retire only a prepared child projection after Control durably accepts
    /// its exact preparation. The shared root Environment must remain until the
    /// aggregate-wide physical disposal is complete.
    async fn acknowledge_terminal_cleanup_preparation(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupEffect,
    ) {
        awaken_session_execution_contract::NeutralRuntime
            .acknowledge_terminal_cleanup_preparation(effect)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Retire only the process-local teardown projection after Control has
    /// durably accepted the exact disposal receipt. No provider or Resource
    /// effect is permitted at this edge.
    async fn acknowledge_terminal_cleanup_disposal(
        &self,
        effect: &awaken_session_state_contract::SessionTerminalCleanupDisposalEffect,
    ) {
        awaken_session_execution_contract::NeutralRuntime
            .acknowledge_terminal_cleanup_disposal(effect)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Retire a process-local terminal projection after Control reports that
    /// the aggregate has no remaining cleanup work. This closes final receipt
    /// response loss without replaying provider I/O or inventing another
    /// completion registry.
    async fn acknowledge_completed_terminal_cleanup(
        &self,
        session_id: &awaken_session_types::SessionId,
        lease: &crate::session_resource::SessionRealizationLease,
    ) {
        awaken_session_execution_contract::NeutralRuntime
            .acknowledge_completed_terminal_cleanup(session_id, lease)
            .await
    }

    async fn install_terminal_cleanup_assignment(
        &self,
        _assignment: &awaken_session_realization_contract::SessionTerminalCleanupAssignment,
        _proof: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), RunError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::OutcomeRuntime for EndSessionRecorder {
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

/// Cause/effect graph: C1 deployment disables the local pool; C2 the Session's
/// Environment definition is otherwise local. C1 freezes the selected Runtime placement fact,
/// which causes E1 one dispatch-only Runtime projection, E2 no local
/// realization lease, and E3 no resident environment binding. Without C1,
/// the canonical local phase driver owns realization (covered by the
/// existing create/realization tests).
///
/// | Rule | No local pool | Environment | Projection | Local lease/binding |
/// |---|---|---|---|---|
/// | P1 | yes | local | once | none |
/// | P2 | no | local | local phase driver | local |
mod binding_recovery;
mod dream_source;
mod external_identity;
mod terminal;

pub(in crate::state) use binding_recovery::sample_inputs;
pub(in crate::state) use external_identity::bare_create_params;
pub(in crate::state) use terminal::sample_persisted;
