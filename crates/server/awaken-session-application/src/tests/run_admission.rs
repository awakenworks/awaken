use super::*;
use awaken_session_application_contract::StepOutcome;
use awaken_session_execution_contract::{
    AdmitSessionRun, AdmittedSessionRun, SessionRunActivation, SessionRunDelivery,
    SessionRunReservation,
};
use awaken_session_types::SessionId;

struct ProtocolProjectionRuntime;

#[derive(Default)]
struct ActivationOnlySessionRunRuntime {
    reservations: Mutex<Vec<AdmitSessionRun>>,
    deliveries: Mutex<Vec<SessionRunDelivery>>,
    restores: AtomicUsize,
    events: Mutex<Vec<&'static str>>,
    projection_installs: Mutex<
        Vec<(
            awaken_session_realization_contract::FrozenSessionProjection,
            awaken_session_realization_contract::SessionProjectionInstallMode,
        )>,
    >,
    adoptions: Mutex<Vec<(String, String, String)>>,
    already_reserved: AtomicBool,
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionRuntime for ActivationOnlySessionRunRuntime {
    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        unreachable!("activation-only test never executes inline")
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: awaken_session_execution_contract::ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        unreachable!("activation-only test never resumes")
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        unreachable!("activation-only test never resumes custom input")
    }

    fn model(&self) -> String {
        "activation-only-session-run".into()
    }

    fn realization(&self) -> Option<&dyn awaken_session_execution_contract::RealizationRuntime> {
        Some(self)
    }

    fn coordination(&self) -> Option<&dyn awaken_session_execution_contract::CoordinationRuntime> {
        Some(self)
    }

    fn environment(&self) -> Option<&dyn awaken_session_execution_contract::EnvironmentRuntime> {
        Some(self)
    }

    fn budget(&self) -> Option<&dyn awaken_session_execution_contract::BudgetRuntime> {
        Some(self)
    }

    fn streaming_runs(&self) -> Option<&dyn awaken_session_execution_contract::StreamingRunRuntime> {
        Some(self)
    }

    fn thread_reads(&self) -> Option<&dyn awaken_session_execution_contract::ThreadReadRuntime> {
        Some(self)
    }

    fn terminal_cleanup(
        &self,
    ) -> Option<&dyn awaken_session_execution_contract::TerminalCleanupRuntime> {
        Some(self)
    }

    fn outcomes(&self) -> Option<&dyn awaken_session_execution_contract::OutcomeRuntime> {
        Some(self)
    }

    fn live_inbox(&self) -> Option<&dyn awaken_session_execution_contract::LiveInboxRuntime> {
        Some(self)
    }

    fn capability(&self) -> Option<&dyn awaken_session_execution_contract::CapabilityRuntime> {
        Some(self)
    }
}

awaken_session_execution_contract::neutral_role!(BudgetRuntime for ActivationOnlySessionRunRuntime);

awaken_session_execution_contract::neutral_role!(StreamingRunRuntime for ActivationOnlySessionRunRuntime);

awaken_session_execution_contract::neutral_role!(ThreadReadRuntime for ActivationOnlySessionRunRuntime);

awaken_session_execution_contract::neutral_role!(TerminalCleanupRuntime for ActivationOnlySessionRunRuntime);

awaken_session_execution_contract::neutral_role!(OutcomeRuntime for ActivationOnlySessionRunRuntime);

awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for ActivationOnlySessionRunRuntime);

awaken_session_execution_contract::neutral_role!(CapabilityRuntime for ActivationOnlySessionRunRuntime);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for ActivationOnlySessionRunRuntime {
    fn observe_realization_request(
        &self,
    ) -> Result<awaken_runtime_contract::authority_lease::AuthorityProofStarted, RunError> {
        crate::tests::test_realization_request_started()
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
        timing: awaken_runtime_contract::authority_lease::AuthorityLeaseTiming,
    ) -> Result<usize, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_projections(control, owner, runtime_incarnation, timing)
            .await
    }

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
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_sandbox_layout(thread, layout)
    }

    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        mode: awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        self.events.lock().unwrap().push(match &mode {
            awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch => {
                "dispatch_projection"
            }
            awaken_session_realization_contract::SessionProjectionInstallMode::Realization {
                prepare_session: true,
                ..
            } if projection.environment.binding().is_some() => "resident_projection",
            awaken_session_realization_contract::SessionProjectionInstallMode::Realization {
                ..
            } => "realization_projection",
        });
        self.projection_installs
            .lock()
            .unwrap()
            .push((projection.clone(), mode.clone()));
        install_complete_test_projection(self, thread, projection, mode).await
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
    ) -> Result<(), awaken_session_error_contract::RunError> {
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
        lease: &awaken_session_resource_contract::SessionRealizationLease,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .invalidate_session_realization_lease(thread, lease)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Install the durable callback used at the exact sandbox creation boundary.
    fn install_environment_binding_sink(
        &self,
        sink: std::sync::Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
    ) {
        awaken_session_execution_contract::NeutralRuntime.install_environment_binding_sink(sink)
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::CoordinationRuntime for ActivationOnlySessionRunRuntime {
    async fn reserve_session_run(
        &self,
        command: awaken_session_execution_contract::AdmitSessionRun,
    ) -> Result<
        awaken_session_execution_contract::SessionRunReservation,
        awaken_session_error_contract::RunError,
    > {
        self.events.lock().unwrap().push("reserve");
        self.reservations.lock().unwrap().push(command);
        Ok(if self.already_reserved.load(Ordering::SeqCst) {
            SessionRunReservation::AlreadyReserved
        } else {
            SessionRunReservation::Reserved
        })
    }

    async fn activate_session_run(
        &self,
        delivery: SessionRunDelivery,
    ) -> Result<SessionRunActivation, RunError> {
        self.events.lock().unwrap().push("activate");
        self.deliveries.lock().unwrap().push(delivery);
        Ok(SessionRunActivation::Activated)
    }

    async fn activate_and_observe_session_run(
        &self,
        admission: AdmittedSessionRun,
        _input_message_ids: Vec<String>,
        _sink: Option<Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<StepOutcome, RunError> {
        self.events.lock().unwrap().push("activate");
        self.deliveries.lock().unwrap().push(
            admission
                .delivery()
                .expect("fresh foreground admission carries a delivery")
                .clone(),
        );
        Ok(StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read the exact Run lifecycle from existing committed Thread truth. This
    /// is the recovery observation for batch advancement, not a completion
    /// registry or protocol event cache.
    async fn session_run_state(
        &self,
        session_id: &awaken_session_types::SessionId,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<
        Option<awaken_agent_contract::agent::run::RunState>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .session_run_state(session_id, run_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read the runtime-owned, durable child-Run relationships for `thread`.
    /// Protocol adapters use this only to rebuild disposable projections after a
    /// restart; the runtime relationship registry remains the sole authority.
    async fn delegated_runs(
        &self,
        thread: &str,
    ) -> Result<
        Vec<awaken_session_application_contract::DelegatedRun>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .delegated_runs(thread)
            .await
    }

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
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .admit_coordinated_run(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
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
        session_id: &awaken_session_types::SessionId,
        observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<
        Vec<awaken_session_execution_contract::CoordinatedThreadLink>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .coordinated_threads(session_id, observed_root)
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
        awaken_session_error_contract::RunError,
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
    ) -> Result<(), awaken_session_error_contract::RunError> {
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
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .interrupt_session_thread(session_id, child_thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
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
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .session_thread_tool_reply_fence(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reply to the exact pending tool of a Primary or logical child Thread
    /// through the Session-affine claim-fenced resume/commit boundary.
    async fn reply_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), awaken_session_error_contract::RunError> {
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
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .reply_and_observe_session_thread_tool(delivery)
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
        awaken_session_error_contract::RunError,
    > {
        self.streaming_runs()
            .ok_or_else(|| {
                awaken_session_error_contract::RunError::unavailable(
                    "StreamingRunRuntime is unsupported by this runtime",
                )
            })?
            .interrupt(thread)
            .await?;
        Ok(awaken_session_application_contract::DelegatedRunSnapshot {
            delegated_runs: self.delegated_runs(thread).await?,
            coordinated_thread_ids: Vec::new(),
            watermark: 0,
            runtime_commit_cursor: self
                .thread_reads()
                .ok_or_else(|| {
                    awaken_session_error_contract::RunError::unavailable(
                        "ThreadReadRuntime is unsupported by this runtime",
                    )
                })?
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
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_agent_message_source(command)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::EnvironmentRuntime for ActivationOnlySessionRunRuntime {
    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Replace the complete Session-local tool configuration after an
    /// idle-session update.
    /// Implementations rebuild the disposable runtime context; durable desired
    /// state has already committed before this projection call.
    async fn replace_session_tools(
        &self,
        thread: &str,
        tools: awaken_session_configuration_contract::SessionToolConfiguration,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .replace_session_tools(thread, tools)
            .await
    }

    async fn adopt_session_environment(
        &self,
        agent: &str,
        thread: &str,
        binding: &str,
    ) -> Result<(), RunError> {
        self.events.lock().unwrap().push("adopt");
        self.adoptions.lock().unwrap().push((
            agent.to_string(),
            thread.to_string(),
            binding.to_string(),
        ));
        Ok(())
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Close Runtime admission and prove that no primary, delegated,
    /// shared-background, MCP, tool, Hand, or child-process effect can mutate
    /// this exact environment generation.
    async fn quiesce_session_environment(
        &self,
        thread: &str,
        operation: &awaken_session_state_contract::SessionEnvironmentOperation,
        source_effect_id: &str,
        source_binding: &str,
        generation: &awaken_session_state_contract::SandboxGeneration,
        expected_mcp_generations: &[awaken_session_mcp_contract::McpGenerationRef],
    ) -> Result<
        awaken_session_state_contract::QuiescenceReceipt,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .quiesce_session_environment(
                thread,
                operation,
                source_effect_id,
                source_binding,
                generation,
                expected_mcp_generations,
            )
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Persist one verified filesystem checkpoint. Replays with the same
    /// operation id must return the same authoritative object receipt.
    async fn checkpoint_session_environment(
        &self,
        thread: &str,
        request: awaken_session_state_contract::SandboxCheckpointRequest,
    ) -> Result<
        awaken_session_state_contract::CheckpointReceipt,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .checkpoint_session_environment(thread, request)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Complete every live source-durability effect without deleting the
    /// source. The aggregate must durably admit the returned receipt before it
    /// invokes physical disposal.
    async fn prepare_checkpoint_source_disposal(
        &self,
        thread: &str,
        preparation: &awaken_session_state_contract::SourceReleasePreparationEffect,
        generation: &awaken_session_state_contract::SandboxGeneration,
        source_binding: &str,
    ) -> Result<
        awaken_session_state_contract::SourceReleasePreparedReceipt,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .prepare_checkpoint_source_disposal(thread, preparation, generation, source_binding)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Physically dispose the source only after the aggregate has durably
    /// committed the exact source-release preparation receipt.
    async fn dispose_prepared_checkpoint_source(
        &self,
        thread: &str,
        disposal: &awaken_session_state_contract::SourceReleaseDisposal,
    ) -> Result<
        awaken_session_state_contract::SourceDisposedReceipt,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .dispose_prepared_checkpoint_source(thread, disposal)
            .await
    }

    async fn restore_checkpointed_session_environment(
        &self,
        request: awaken_session_state_contract::SandboxRestoreRequest,
    ) -> Result<awaken_session_state_contract::RestoreReceipt, RunError> {
        self.events.lock().unwrap().push("restore");
        self.restores.fetch_add(1, Ordering::SeqCst);
        Ok(awaken_session_state_contract::RestoreReceipt {
            effect_id: request.effect_id,
            generation_id: request.generation_id,
            checkpoint_id: request.checkpoint.id,
            binding: "restored-binding".into(),
        })
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Idempotently delete checkpoint bytes at a terminal edge. It never
    /// restores the Environment merely to clean it up.
    async fn delete_session_checkpoint(
        &self,
        thread: &str,
        checkpoint: &awaken_session_state_contract::SandboxCheckpointRef,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .delete_session_checkpoint(thread, checkpoint)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Resolve the current versions of already-authorized Skill resource ids once
    /// at Session creation. The implementation receives only a trusted Workspace
    /// and resource ids; it performs no principal/role/policy decision.
    async fn resolve_session_skills(
        &self,
        workspace_id: &str,
        skills: &[awaken_agent_contract::AgentSkillBinding],
    ) -> Result<
        Vec<awaken_session_resource_contract::ResolvedSkillBinding>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .resolve_session_skills(workspace_id, skills)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Apply one complete, aggregate-authored Resource transition. Live
    /// add/update/delete, rollback, and restart restoration converge here;
    /// Runtime never guesses the prior generation from a process-local cache or
    /// re-resolves Agent defaults/current Resource configuration.
    async fn apply_session_inputs(
        &self,
        thread: &str,
        transition: &awaken_session_resource_contract::SessionResourceTransition,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .apply_session_inputs(thread, transition)
            .await
    }
}

#[tokio::test]
async fn cold_replica_recovers_projection_before_reusing_exact_activity_receipt() {
    // Multi-replica response-loss cause/effect graph: C1 the Session and exact
    // Run activity epoch are durable; C2 the shared Run reservation already
    // exists; C3 this Coordinator has no process-local Session projection; C4
    // placement is Worker-owned, so recovery is projection-only. Effects: E1
    // install the complete Dispatch projection before reserve; E2 reuse the
    // exact activity epoch; E3 create no second activity or epoch; E4 do not
    // perform a local physical realization. Constraint K1: a durable activity
    // receipt proves admission history, never process-local cache residency.
    //
    // | Rule | exact activity | reservation | local projection | Effect |
    // | R1   | present        | existing    | cold             | E1-E4  |
    // | R2   | absent         | fresh       | cold             | existing fresh-admission tests |
    // | R3   | present        | existing    | warm             | idempotent E1-E3 |
    let repository: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let mut session = persisted("cold-replica-activity", false, "idle");
    let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
        &mut session.baseline
    else {
        unreachable!("fixture is frozen")
    };
    baseline.runtime_placement = SessionRuntimePlacement::Worker;
    create(repository.as_ref(), session).await;

    let runtime = Arc::new(ActivationOnlySessionRunRuntime::default());
    runtime.already_reserved.store(true, Ordering::SeqCst);
    let application = application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let run_id = awaken_session_execution_contract::session_run_id(
        &awaken_session_types::SessionId::from("cold-replica-activity"),
        "cold-replica-operation",
    );
    let activity_operation = awaken_session_execution_contract::session_run_activity_operation_id(
        &awaken_session_types::SessionId::from("cold-replica-activity"),
        &run_id,
    );
    let (_, original_epoch) = application
        .begin_activity_for_operation(
            &SessionId::from("cold-replica-activity"),
            &activity_operation,
        )
        .await
        .expect("C1 exact durable activity receipt");

    let admitted = application
        .admit_session_run(AdmitSessionRun {
            session_id: "cold-replica-activity".into(),
            agent_id: "agent".into(),
            operation_id: "cold-replica-operation".into(),
            run_id,
            messages: Vec::new(),
            data_subject_id: None,
            traceparent: None,
            execution_requirements: Default::default(),
            replacement: Default::default(),
        })
        .await
        .expect("R1 cold-replica response-loss recovery");

    let AdmittedSessionRun::AlreadyReserved(delivery) = admitted else {
        panic!("R1/E2 must retain the existing reservation outcome");
    };
    assert_eq!(delivery.session_activity_epoch, original_epoch, "R1/E2");
    assert_eq!(
        runtime.events.lock().unwrap().as_slice(),
        ["dispatch_projection", "reserve"],
        "R1/E1+E4 projection precedes reservation without local realization",
    );
    assert_eq!(
        runtime.projection_installs.lock().unwrap().len(),
        1,
        "R1/E1"
    );
    let persisted = repository
        .get(&SessionId::from("cold-replica-activity"))
        .await
        .expect("R1 durable Session");
    assert_eq!(persisted.activity_epoch, original_epoch, "R1/E3");
    assert_eq!(persisted.active_activity_epochs.len(), 1, "R1/E3");
}

#[tokio::test]
async fn authenticated_protocol_scope_survives_a_cold_replica_and_replays_one_activity() {
    // Cause/effect graph: C1 the profiled Session is durable in `workspace`;
    // C2 this Coordinator has no Session slot, so its local fallback is wrong;
    // C3 the authenticated request carries `workspace`; C4 the same message is
    // replayed; C5 a request asserts another owner, for Run or tool resume.
    // Effects: E1 C1+C2+C3
    // recovers and admits through the existing Session application; E2 C4
    // reuses the exact activity epoch; E3 C5 or no request scope rejects before
    // another reservation. The scope is an assertion, not a cache mutation.
    //
    // | Rule | durable owner | local fallback | request owner | operation | Effect |
    // | R1   | workspace     | wrong          | workspace     | fresh     | E1     |
    // | R2   | workspace     | wrong          | workspace     | replay    | E2     |
    // | R3   | workspace     | wrong          | foreign       | replay    | E3     |
    // | R4   | workspace     | wrong          | absent        | replay    | E3     |
    // | R5   | workspace     | wrong          | foreign       | resume    | E3     |
    let repository: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(
        repository.as_ref(),
        persisted("cold-protocol-scope", false, "idle"),
    )
    .await;
    let runtime = Arc::new(ActivationOnlySessionRunRuntime::default());
    let sessions = Arc::new(application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    let protocol = SessionRunApplication::new(
        Arc::new(ProtocolProjectionRuntime),
        sessions,
        |_| "wrong-local-workspace".into(),
        |_| Some("agent".into()),
    );
    let message = || {
        vec![awaken_agent_contract::agent::message::Message::text(
            awaken_agent_contract::agent::message::Id("cold-input".into()),
            awaken_agent_contract::agent::message::Role::User,
            "continue",
        )]
    };
    let sink = || {
        Arc::new(awaken_session_application_contract::EventForwardingSink::new(|_| Ok(())))
            as Arc<dyn awaken_agent_contract::stream::sink::Sink>
    };
    awaken_session_application_contract::OwnerScopedRunApplication::run_streaming_for_owner(
        &protocol,
        "workspace",
        "cold-operation",
        "cold-protocol-scope",
        None,
        message(),
        sink(),
    )
    .await
    .expect("R1/E1 authenticated cold admission");
    let first_epoch = repository
        .get(&SessionId::from("cold-protocol-scope"))
        .await
        .expect("R1 durable Session")
        .activity_epoch;
    assert_eq!(runtime.reservations.lock().unwrap().len(), 1, "R1/E1");

    runtime.already_reserved.store(true, Ordering::SeqCst);
    awaken_session_application_contract::OwnerScopedRunApplication::run_streaming_for_owner(
        &protocol,
        "workspace",
        "cold-operation",
        "cold-protocol-scope",
        None,
        message(),
        sink(),
    )
    .await
    .expect("R2/E2 same operation replay");
    assert_eq!(
        repository
            .get(&SessionId::from("cold-protocol-scope"))
            .await
            .expect("R2 durable Session")
            .activity_epoch,
        first_epoch,
        "R2/E2",
    );
    let reservations_before_rejection = runtime.reservations.lock().unwrap().len();

    assert!(
        awaken_session_application_contract::OwnerScopedRunApplication::run_streaming_for_owner(
            &protocol,
            "foreign-workspace",
            "cold-operation",
            "cold-protocol-scope",
            None,
            message(),
            sink(),
        )
        .await
        .is_err(),
        "R3/E3",
    );
    assert!(
        awaken_session_application_contract::RunApplication::run_streaming(
            &protocol,
            "cold-operation",
            "cold-protocol-scope",
            None,
            message(),
            sink(),
        )
        .await
        .is_err(),
        "R4/E3",
    );
    assert!(
        awaken_session_application_contract::OwnerScopedRunApplication::resume_for_owner(
            &protocol,
            "foreign-workspace",
            "cold-resume",
            "cold-protocol-scope",
            "tool-call",
            awaken_session_application_contract::RunResume::ClientResult {
                content: Vec::new(),
                is_error: false,
            },
        )
        .await
        .is_err(),
        "R5/E3",
    );
    assert_eq!(
        runtime.reservations.lock().unwrap().len(),
        reservations_before_rejection,
        "R3-R5/E3 no reservation under a wrong owner",
    );
}

#[tokio::test]
async fn background_attention_uses_canonical_session_admission_without_observing() {
    // Cause/effect decision table: C1 a durable Session exists; C2 the caller
    // requests background observation; C3 reservation succeeds; C4 the exact
    // Session activity receipt commits. Effects: E1 one PreservePrior command
    // is reserved; E2 the same Run id and non-zero epoch are activated once;
    // E3 the caller can return while the Session remains Running; E4 a
    // BackgroundTask reminder remains ordinary Role::System Run input rather
    // than Session state or a special notification payload. Failure rule B2:
    // C3 or C4 false => no executable delivery. Constraint K1: neither Host
    // enqueue nor a second background lifecycle is available to this port.
    let repository: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(
        repository.as_ref(),
        persisted("background-protocol", false, "idle"),
    )
    .await;
    let runtime = Arc::new(ActivationOnlySessionRunRuntime::default());
    let sessions = Arc::new(application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    let protocol = SessionRunApplication::new(
        Arc::new(ProtocolProjectionRuntime),
        sessions,
        |_| "workspace".into(),
        |_| Some("agent".into()),
    );
    let run_id =
        awaken_session_application_contract::SessionRunBackgroundApplication::submit_session_run_background(
            &protocol,
            "background-operation",
            "background-protocol",
            None,
            vec![awaken_agent_contract::agent::message::Message::text(
                awaken_agent_contract::agent::message::Id("background-input".into()),
                awaken_agent_contract::agent::message::Role::System,
                "inspect the completed background task",
            )],
            None,
        )
        .await
        .expect("B1 background admission");

    // B1/E1-E2 readback is synchronous evidence only. Keep each recording
    // lock in its own lexical scope so the durable repository read below can
    // never await while holding a test-side projection lock.
    {
        let reservations = runtime.reservations.lock().unwrap();
        assert_eq!(reservations.len(), 1, "B1/E1");
        assert_eq!(reservations[0].run_id, run_id, "B1/E1 exact Run");
        assert_eq!(
            reservations[0].messages[0].role,
            awaken_agent_contract::agent::message::Role::System,
            "B1/E4 system attention input",
        );
        assert_eq!(
            reservations[0].replacement,
            awaken_session_execution_contract::SessionRunReplacement::PreservePrior,
            "B1/E1 append semantics",
        );
    }
    {
        let deliveries = runtime.deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 1, "B1/E2");
        assert_eq!(deliveries[0].run_id, run_id, "B1/E2 exact Run");
        assert!(deliveries[0].session_activity_epoch > 0, "B1/E2 receipt");
    }
    assert_eq!(
        repository
            .get(&SessionId::from("background-protocol"))
            .await
            .expect("B1 durable Session")
            .execution,
        SessionExecutionState::Running,
        "B1/E3 background caller did not wait for settlement",
    );
}

#[tokio::test]
async fn public_run_reserves_then_restores_and_projects_before_activation() {
    // Hibernated public-admission cause/effect graph: C1 a nonterminal local
    // Session carries one live Hibernated checkpoint; C2 realization rejects
    // continuation phases as physical effects; C3 the durable Run reservation
    // succeeds; C4 restore commits Resident; C5 activation follows. Effects:
    // E1 install only the existing non-physical Dispatch projection before the
    // reservation; E2 reserve before restore; E3 after the Resident CAS install
    // the complete Realization(prepare=true) projection with the current exact
    // lease and adopt its exact binding; E4 activate only after E3; E5 one
    // restore and no parallel queue.
    //
    // | Rule | phase at recovery | pre-reserve projection | restore | Effect |
    // | H1 | Hibernated | Dispatch only | succeeds | E1-E5 |
    // | H2 | Unmaterialized/Resident | canonical realization | n/a | adjacent realization tests |
    // | H3 | terminal/invalid | none | none | reject through existing gates |
    let repository: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(
        repository.as_ref(),
        super::continuation::hibernated_session("public-restore", u64::MAX),
    )
    .await;
    let runtime = Arc::new(ActivationOnlySessionRunRuntime::default());
    let sessions = Arc::new(application_with_runtime(
        runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    let protocol = SessionRunApplication::new(
        Arc::new(ProtocolProjectionRuntime),
        sessions,
        |_| "workspace".into(),
        |_| Some("agent".into()),
    );

    awaken_session_application_contract::RunApplication::run(
        &protocol,
        "public-restore-operation",
        "public-restore",
        None,
        vec![awaken_agent_contract::agent::message::Message::text(
            awaken_agent_contract::agent::message::Id("public-restore-input".into()),
            awaken_agent_contract::agent::message::Role::User,
            "resume",
        )],
    )
    .await
    .expect("H1 public driving Run");

    assert_eq!(
        runtime.events.lock().unwrap().as_slice(),
        [
            "dispatch_projection",
            "reserve",
            "restore",
            "resident_projection",
            "adopt",
            "activate",
        ],
        "H1/E1-E4",
    );
    assert_eq!(runtime.restores.load(Ordering::SeqCst), 1, "H1/E5");
    let resident = repository
        .get(&SessionId::from("public-restore"))
        .await
        .expect("H1 root");
    assert!(
        matches!(
            resident.environment,
            awaken_session_state_contract::SessionEnvironmentState::Resident { .. }
        ),
        "H1/E3",
    );
    let projections = runtime.projection_installs.lock().unwrap();
    assert_eq!(projections.len(), 2, "H1/E1+E3");
    assert!(
        matches!(
            projections[0].1,
            awaken_session_realization_contract::SessionProjectionInstallMode::Dispatch
        ),
        "H1/E1",
    );
    assert!(
        matches!(
            &projections[1].1,
            awaken_session_realization_contract::SessionProjectionInstallMode::Realization {
                lease,
                prepare_session: true,
            } if Some(&lease.lease) == resident.realization.as_ref()
        ),
        "H1/E3",
    );
    assert_eq!(projections[1].0.environment, resident.environment, "H1/E3");
    drop(projections);
    assert_eq!(
        runtime.adoptions.lock().unwrap().as_slice(),
        [(
            "agent".into(),
            "public-restore".into(),
            "restored-binding".into(),
        )],
        "H1/E3 exact adoption precedes activation",
    );
}

struct BlockingSessionRunRuntime {
    entered: Arc<tokio::sync::Semaphore>,
    release: Arc<tokio::sync::Semaphore>,
    application: std::sync::OnceLock<std::sync::Weak<SessionApplication>>,
}

#[async_trait::async_trait]
impl awaken_session_application_contract::RunApplication for ProtocolProjectionRuntime {
    async fn run(
        &self,
        _operation_id: &str,
        _thread: &str,
        _agent: Option<String>,
        _messages: Vec<awaken_agent_contract::agent::message::Message>,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        unreachable!("Session Run execution is owned by SessionRuntime")
    }

    async fn resume(
        &self,
        _operation_id: &str,
        _thread: &str,
        _tool_use_id: &str,
        _resume: awaken_session_application_contract::RunResume,
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        unreachable!("protocol activity test never resumes")
    }

    async fn pending(
        &self,
        _thread: &str,
    ) -> Result<
        Option<awaken_session_application_contract::Pending>,
        awaken_session_error_contract::RunError,
    > {
        Ok(None)
    }

    async fn history(
        &self,
        _thread: &str,
    ) -> Result<
        Vec<awaken_agent_contract::agent::message::Message>,
        awaken_session_error_contract::RunError,
    > {
        Ok(Vec::new())
    }

    fn model(&self) -> String {
        "protocol-projection".into()
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionRuntime for BlockingSessionRunRuntime {
    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        unreachable!("legacy inline execution is not an admission path")
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: awaken_session_execution_contract::ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        unreachable!("test never resumes")
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        unreachable!("test never resumes a custom tool")
    }

    fn model(&self) -> String {
        "blocking-session-run".into()
    }

    fn realization(&self) -> Option<&dyn awaken_session_execution_contract::RealizationRuntime> {
        Some(self)
    }

    fn coordination(&self) -> Option<&dyn awaken_session_execution_contract::CoordinationRuntime> {
        Some(self)
    }

    fn budget(&self) -> Option<&dyn awaken_session_execution_contract::BudgetRuntime> {
        Some(self)
    }

    fn streaming_runs(&self) -> Option<&dyn awaken_session_execution_contract::StreamingRunRuntime> {
        Some(self)
    }

    fn environment(&self) -> Option<&dyn awaken_session_execution_contract::EnvironmentRuntime> {
        Some(self)
    }

    fn thread_reads(&self) -> Option<&dyn awaken_session_execution_contract::ThreadReadRuntime> {
        Some(self)
    }

    fn terminal_cleanup(
        &self,
    ) -> Option<&dyn awaken_session_execution_contract::TerminalCleanupRuntime> {
        Some(self)
    }

    fn outcomes(&self) -> Option<&dyn awaken_session_execution_contract::OutcomeRuntime> {
        Some(self)
    }

    fn live_inbox(&self) -> Option<&dyn awaken_session_execution_contract::LiveInboxRuntime> {
        Some(self)
    }

    fn capability(&self) -> Option<&dyn awaken_session_execution_contract::CapabilityRuntime> {
        Some(self)
    }
}

awaken_session_execution_contract::neutral_role!(BudgetRuntime for BlockingSessionRunRuntime);

awaken_session_execution_contract::neutral_role!(StreamingRunRuntime for BlockingSessionRunRuntime);

awaken_session_execution_contract::neutral_role!(EnvironmentRuntime for BlockingSessionRunRuntime);

awaken_session_execution_contract::neutral_role!(ThreadReadRuntime for BlockingSessionRunRuntime);

awaken_session_execution_contract::neutral_role!(TerminalCleanupRuntime for BlockingSessionRunRuntime);

awaken_session_execution_contract::neutral_role!(OutcomeRuntime for BlockingSessionRunRuntime);

awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for BlockingSessionRunRuntime);

awaken_session_execution_contract::neutral_role!(CapabilityRuntime for BlockingSessionRunRuntime);

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for BlockingSessionRunRuntime {
    fn observe_realization_request(
        &self,
    ) -> Result<awaken_runtime_contract::authority_lease::AuthorityProofStarted, RunError> {
        crate::tests::test_realization_request_started()
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
        timing: awaken_runtime_contract::authority_lease::AuthorityLeaseTiming,
    ) -> Result<usize, awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_projections(control, owner, runtime_incarnation, timing)
            .await
    }

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
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_sandbox_layout(thread, layout)
    }

    async fn install_session_projection(
        &self,
        thread: &str,
        projection: awaken_session_realization_contract::FrozenSessionProjection,
        mode: awaken_session_realization_contract::SessionProjectionInstallMode,
    ) -> Result<(), RunError> {
        install_complete_test_projection(self, thread, projection, mode).await
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
    ) -> Result<(), awaken_session_error_contract::RunError> {
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
        lease: &awaken_session_resource_contract::SessionRealizationLease,
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .invalidate_session_realization_lease(thread, lease)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Install the durable callback used at the exact sandbox creation boundary.
    fn install_environment_binding_sink(
        &self,
        sink: std::sync::Arc<dyn awaken_session_execution_contract::SessionEnvironmentBindingSink>,
    ) {
        awaken_session_execution_contract::NeutralRuntime.install_environment_binding_sink(sink)
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::CoordinationRuntime for BlockingSessionRunRuntime {
    async fn reserve_session_run(
        &self,
        _command: awaken_session_execution_contract::AdmitSessionRun,
    ) -> Result<
        awaken_session_execution_contract::SessionRunReservation,
        awaken_session_error_contract::RunError,
    > {
        Ok(SessionRunReservation::Reserved)
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
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .activate_session_run(delivery)
            .await
    }

    async fn activate_and_observe_session_run(
        &self,
        admission: AdmittedSessionRun,
        _input_message_ids: Vec<String>,
        _sink: Option<Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<StepOutcome, RunError> {
        let delivery = admission
            .delivery()
            .expect("fresh test admission carries the durable activity receipt");
        self.entered.add_permits(1);
        self.release
            .acquire()
            .await
            .expect("test release semaphore")
            .forget();
        self.application
            .get()
            .and_then(std::sync::Weak::upgrade)
            .expect("test Session application")
            .settle_activity(
                delivery.session_id.as_str(),
                delivery.session_activity_epoch,
            )
            .await
            .expect("committed Run observation settles the exact activity");
        Ok(StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read the exact Run lifecycle from existing committed Thread truth. This
    /// is the recovery observation for batch advancement, not a completion
    /// registry or protocol event cache.
    async fn session_run_state(
        &self,
        session_id: &awaken_session_types::SessionId,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<
        Option<awaken_agent_contract::agent::run::RunState>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .session_run_state(session_id, run_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Read the runtime-owned, durable child-Run relationships for `thread`.
    /// Protocol adapters use this only to rebuild disposable projections after a
    /// restart; the runtime relationship registry remains the sole authority.
    async fn delegated_runs(
        &self,
        thread: &str,
    ) -> Result<
        Vec<awaken_session_application_contract::DelegatedRun>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .delegated_runs(thread)
            .await
    }

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
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .admit_coordinated_run(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
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
        session_id: &awaken_session_types::SessionId,
        observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<
        Vec<awaken_session_execution_contract::CoordinatedThreadLink>,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .coordinated_threads(session_id, observed_root)
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
        awaken_session_error_contract::RunError,
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
    ) -> Result<(), awaken_session_error_contract::RunError> {
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
    ) -> Result<(), awaken_session_error_contract::RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .interrupt_session_thread(session_id, child_thread_id)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
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
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .session_thread_tool_reply_fence(command)
            .await
    }

    // Former `awaken_session_execution_contract::SessionRuntime` default, kept verbatim: this implementation never
    // answered this role method with anything but the refusal below.
    /// Reply to the exact pending tool of a Primary or logical child Thread
    /// through the Session-affine claim-fenced resume/commit boundary.
    async fn reply_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), awaken_session_error_contract::RunError> {
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
    ) -> Result<
        awaken_session_application_contract::StepOutcome,
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .reply_and_observe_session_thread_tool(delivery)
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
        awaken_session_error_contract::RunError,
    > {
        self.streaming_runs()
            .ok_or_else(|| {
                awaken_session_error_contract::RunError::unavailable(
                    "StreamingRunRuntime is unsupported by this runtime",
                )
            })?
            .interrupt(thread)
            .await?;
        Ok(awaken_session_application_contract::DelegatedRunSnapshot {
            delegated_runs: self.delegated_runs(thread).await?,
            coordinated_thread_ids: Vec::new(),
            watermark: 0,
            runtime_commit_cursor: self
                .thread_reads()
                .ok_or_else(|| {
                    awaken_session_error_contract::RunError::unavailable(
                        "ThreadReadRuntime is unsupported by this runtime",
                    )
                })?
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
        awaken_session_error_contract::RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_agent_message_source(command)
            .await
    }
}

#[tokio::test]
async fn background_activation_uses_the_session_runtime_without_a_second_queue_port() {
    // Cause/effect graph: C1 admission returns a delivery, recovery-claimed, or
    // completed; C2 the caller wants asynchronous observation. Effects: E1 an
    // exact delivery crosses the SessionRuntime activation port once; E2 closed
    // recovery/terminal outcomes are projected without touching Runtime.
    // Constraint: SessionApplication exposes no dispatch repository or raw row.
    // Decision table: R1=C1.delivery+C2=>E1; R2=C1.recovery=>E2.RecoveryClaimed;
    // R3=C1.completed=>E2.Completed.
    let repository: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let runtime = Arc::new(ActivationOnlySessionRunRuntime::default());
    let application = application_with_runtime(
        runtime.clone(),
        repository,
        Arc::new(RecordingEnvironmentSource::default()),
    );
    let delivery = SessionRunDelivery {
        session_id: "background-session".into(),
        run_id: awaken_agent_contract::agent::run::Id("background-run".into()),
        session_activity_epoch: 7,
    };
    assert_eq!(
        application
            .activate_admitted_session_run(AdmittedSessionRun::Reserved(delivery.clone()))
            .await
            .expect("R1 activation"),
        SessionRunActivation::Activated,
        "R1/E1"
    );
    assert_eq!(&*runtime.deliveries.lock().unwrap(), &[delivery], "R1/E1");
    assert_eq!(
        application
            .activate_admitted_session_run(AdmittedSessionRun::RecoveryClaimed {
                session_id: "background-session".into(),
                run_id: awaken_agent_contract::agent::run::Id("background-run".into()),
            })
            .await
            .expect("R2 projection"),
        SessionRunActivation::RecoveryClaimed,
        "R2/E2"
    );
    assert_eq!(runtime.deliveries.lock().unwrap().len(), 1, "R2/E2");
    assert_eq!(
        application
            .activate_admitted_session_run(AdmittedSessionRun::Completed {
                session_id: "background-session".into(),
                run_id: awaken_agent_contract::agent::run::Id("background-run".into()),
            })
            .await
            .expect("R3 projection"),
        SessionRunActivation::Completed,
        "R3/E2"
    );
    assert_eq!(runtime.deliveries.lock().unwrap().len(), 1, "R3/E2");
}

#[tokio::test]
async fn admission_surfaces_dispatch_failure_and_preserves_retryable_intent() {
    // Session-realization dispatch FMECA and cause/effect graph. C1 the frozen
    // Session requires a registered Worker; C2 the durable dispatch projection
    // is installed; C3 the WorkQueue write succeeds or fails; C4 the same
    // durable intent is retried after the queue recovers. Effects: E1 a failed
    // first dispatch returns the stable `session_work_dispatch_failed` error
    // before Runtime execution; E2 the Session remains reconcilable (no false
    // Idle/terminal transition); E3 retry uses the same Session identity and
    // dispatches exactly once. Critical failure mode: swallowing C3 previously
    // left the caller waiting with no immediate cause while appearing admitted.
    //
    // | Rule | C1 | C2 | C3 | C4 | Effect |
    // | D1   | T  | T  | F  | F  | E1+E2  |
    // | D2   | T  | T  | T  | T  | E3     |
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let environments = Arc::new(RecordingEnvironmentSource::default());
    create(repo.as_ref(), persisted("dispatch-admission", true, "idle")).await;
    environments.fail_for("dispatch-admission");
    let refresh = Arc::new(ToggleProjectionRefresh::default());
    refresh.fail.store(true, Ordering::SeqCst);
    let mut application = application(repo.clone(), environments.clone());
    application
        .set_executable_projection_refresh(paired_projection_refresh(refresh.clone()))
        .unwrap();

    // Projection prerequisite decision extension: C5 refresh unavailable;
    // E4 recovery fails before Resource/Runtime/WorkQueue effects; C6 retry
    // after recovery; E5 the existing canonical dispatch path runs. Constraint
    // K1 durable Session intent remains unchanged. D3 C5=>E4; D4 C6=>E5.
    let revision = repo
        .get(&SessionId::from("dispatch-admission"))
        .await
        .unwrap()
        .revision;
    assert!(
        matches!(
            application
                .recover_session_projection("dispatch-admission", Some("workspace"))
                .await,
            Err(SessionProjectionRecoveryError::Unavailable(_))
        ),
        "D3/E4"
    );
    assert_eq!(
        repo.get(&SessionId::from("dispatch-admission"))
            .await
            .unwrap()
            .revision,
        revision,
        "D3/E4 no durable mutation"
    );
    assert!(environments.dispatched.lock().unwrap().is_empty(), "D3/E4");
    refresh.fail.store(false, Ordering::SeqCst);

    let error = match application
        .recover_session_projection("dispatch-admission", Some("workspace"))
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("D1/E1 queue failure must reject current admission"),
    };
    let SessionProjectionRecoveryError::Rejected(error) = error else {
        panic!("D1/E1 must preserve the classified Run error");
    };
    assert_eq!(error.code, "session_work_dispatch_failed", "D1/E1");
    assert!(error.message.contains("injected failure"), "D1/E1");
    let persisted = repo
        .get(&SessionId::from("dispatch-admission"))
        .await
        .expect("D1/E2 intent");
    assert!(persisted.needs_work_dispatch(), "D1/E2 remains retryable");
    assert!(!persisted.is_terminal(), "D1/E2 is not falsely terminal");

    environments.recover_for("dispatch-admission");
    let recovered = application
        .recover_session_projection("dispatch-admission", Some("workspace"))
        .await
        .expect("D2/E3 queue recovered")
        .expect("D2/E3 Session remains present");
    assert!(
        recovered.session.needs_work_dispatch(),
        "D2/E3 awaits Worker ack"
    );
    assert_eq!(
        environments
            .dispatched
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>(),
        ["dispatch-admission".to_string()],
        "D2/E3 one stable dispatch identity"
    );
    application
        .admit_run_session("workspace", "dispatch-admission", "ignored")
        .await
        .expect("D2/E3 a driving event wakes the stable Work item");
    assert!(
        environments
            .awakened
            .lock()
            .unwrap()
            .contains("dispatch-admission"),
        "D2/E3 the admitted event explicitly wakes completed Work"
    );
    assert!(refresh.calls.load(Ordering::SeqCst) >= 3, "D3-D4");
}

#[tokio::test]
async fn worker_placed_cloud_session_admits_the_event_that_triggers_realization() {
    // Cause/effect decision table: C1 frozen placement=Worker; C2 Environment
    // is Cloud rather than self-hosted; C3 Session is Preparing; C4 no
    // late Session input. Effect E1 admission installs the dispatch projection
    // and succeeds without manufacturing Environment WorkQueue work;
    // the accepted event is what creates the Runtime Run claimed by a Worker.
    // Local Preparing is independently rejected by activity rule A8.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let environments = Arc::new(RecordingEnvironmentSource::default());
    let mut session = persisted("cloud-worker-admission", false, "preparing");
    let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
        &mut session.baseline
    else {
        unreachable!("fixture is frozen")
    };
    baseline.runtime_placement = SessionRuntimePlacement::Worker;
    create(repo.as_ref(), session).await;
    let application = application(repo.clone(), environments.clone());

    application
        .admit_run_session("workspace", "cloud-worker-admission", "ignored")
        .await
        .expect("E1 Worker-owned realization is triggered by this admitted event");
    assert!(
        environments.dispatched.lock().unwrap().is_empty(),
        "E1 Cloud Environment does not enter the self-hosted WorkQueue"
    );
    assert_eq!(
        repo.get(&SessionId::from("cloud-worker-admission"))
            .await
            .expect("E1 durable Session")
            .execution,
        SessionExecutionState::Preparing,
        "E1 only the claimed Worker may acknowledge physical realization"
    );
}

#[tokio::test]
async fn recovered_running_activity_admits_a_fenced_successor() {
    // Crash-recovery cause/effect graph: C1 a prior process committed Running
    // plus activity epoch 1; C2 no external Worker realization is required; C3
    // a successor driving event enters after restart. C1+C2+C3 => admission
    // succeeds and begin_activity can advance the epoch; Preparing/Activating/
    // Rescheduling and terminal partitions remain rejected by the shared
    // `admits_activity` table. FMECA: rejecting Running permanently strands the
    // Outcome/continuation; treating every nonterminal state as ready bypasses
    // realization convergence.
    let repo = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    let mut running = persisted("crash-orphaned-activity", false, "running");
    running.activity_epoch = 1;
    create(repo.as_ref(), running).await;
    let application = application(
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );

    application
        .admit_run_session("workspace", "crash-orphaned-activity", "agent")
        .await
        .expect("Running is an execution-ready, epoch-fenced state");
    let successor = application
        .begin_activity(&SessionId::from("crash-orphaned-activity"))
        .await
        .expect("successor activity advances the durable fence");
    assert_eq!(successor.execution, SessionExecutionState::Running);
    assert_eq!(successor.activity_epoch, 2);
}

#[tokio::test]
async fn public_protocol_run_projects_durable_running_until_runtime_settles() {
    // Hosted MCP authorization cause/effect graph: C1 an AI SDK/AG-UI/A2A Run
    // has passed the canonical Session owner/Agent admission; C2 Runtime is
    // executing; C3 Runtime settles; C4 a foreign owner replays the exact stable
    // operation after its activity receipt exists. Effects: E1 the durable Session is Running
    // throughout C2 so an internal MCP `tools/call` can prove a live Run; E2 C3
    // closes the same activity and returns the Session to Idle; E3 C4 is rejected
    // before Runtime even though response-loss recovery truth exists. Constraint: the
    // protocol keeps its original Message/stream path; SessionApplication is the
    // sole activity owner. Decision table: P1=C1+C2=>E1; P2=C1+C3=>E2;
    // P3=C4=>E3. The
    // adjacent decorator test covers admission and Runtime failures.
    let repository: Arc<dyn awaken_session_repository_contract::SessionStore> = Arc::new(
        awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository"),
    );
    create(
        repository.as_ref(),
        persisted("protocol-running", false, "idle"),
    )
    .await;
    let entered = Arc::new(tokio::sync::Semaphore::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let session_runtime = Arc::new(BlockingSessionRunRuntime {
        entered: entered.clone(),
        release: release.clone(),
        application: std::sync::OnceLock::new(),
    });
    let application = Arc::new(application_with_runtime(
        session_runtime.clone(),
        repository.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    ));
    assert!(
        session_runtime
            .application
            .set(Arc::downgrade(&application))
            .is_ok(),
        "install test settlement observer"
    );
    let protocol = SessionRunApplication::new(
        Arc::new(ProtocolProjectionRuntime),
        application.clone(),
        |_| "workspace".into(),
        |_| Some("agent".into()),
    );

    let drive = tokio::spawn(async move {
        awaken_session_application_contract::RunApplication::run(
            &protocol,
            "protocol-operation",
            "protocol-running",
            None,
            vec![awaken_agent_contract::agent::message::Message::text(
                awaken_agent_contract::agent::message::Id("protocol-input".into()),
                awaken_agent_contract::agent::message::Role::User,
                "hello",
            )],
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(30), entered.acquire())
        .await
        .expect("timed out waiting for entered.acquire()")
        .expect("P1 Runtime entered")
        .forget();
    assert_eq!(
        repository
            .get(&SessionId::from("protocol-running"))
            .await
            .expect("P1 durable Session")
            .execution,
        SessionExecutionState::Running,
        "P1/E1"
    );

    release.add_permits(1);
    drive.await.expect("P2 join").expect("P2 Run settled");
    assert_eq!(
        repository
            .get(&SessionId::from("protocol-running"))
            .await
            .expect("P2 durable Session")
            .execution,
        SessionExecutionState::Idle,
        "P2/E2"
    );

    let foreign = SessionRunApplication::new(
        Arc::new(ProtocolProjectionRuntime),
        application,
        |_| "foreign-workspace".into(),
        |_| Some("agent".into()),
    );
    assert!(
        awaken_session_application_contract::RunApplication::run(
            &foreign,
            "protocol-operation",
            "protocol-running",
            None,
            vec![awaken_agent_contract::agent::message::Message::text(
                awaken_agent_contract::agent::message::Id("protocol-input".into()),
                awaken_agent_contract::agent::message::Role::User,
                "hello",
            )],
        )
        .await
        .is_err(),
        "P3/E3 a receipt never bypasses owner authorization"
    );
}
