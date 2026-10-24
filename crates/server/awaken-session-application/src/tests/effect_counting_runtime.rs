//! Effect-counting Session Runtime fixture for acceptance oracles.
//!
//! The production Session application is the unit under test; this fake is
//! only its Runtime collaborator. Unlike activation-only fakes, every effect
//! that could execute a model or tool (inline `run`/`resume`, tool-reply
//! delivery, Run activation, budget resumption, Resource application) is
//! COUNTED instead of panicking, so a test can assert "zero effects" with a
//! concrete counter and a negative control can prove the counter detects a
//! leaked effect that a success/error return code would hide.
use super::*;
use awaken_session_application_contract::{
    SessionBudgetResumeDelivery, SessionBudgetResumeDisposition, SessionBudgetResumeTicket,
    StepOutcome,
};
use awaken_session_execution_contract::{
    AdmitSessionRun, AdmittedSessionRun, SessionRunActivation, SessionRunDelivery,
    SessionRunReservation,
};

/// Snapshot of every counted effect. Equality of two snapshots is the
/// "zero additional effect" oracle.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct EffectCounts {
    pub(crate) model_calls: usize,
    pub(crate) tool_calls: usize,
    pub(crate) reservations: usize,
    pub(crate) activations: usize,
    pub(crate) budget_resumes: usize,
    pub(crate) resource_applies: usize,
}

#[derive(Default)]
pub(crate) struct EffectCountingRuntime {
    pub(crate) model_calls: AtomicUsize,
    pub(crate) tool_calls: AtomicUsize,
    pub(crate) reservations: Mutex<Vec<AdmitSessionRun>>,
    pub(crate) deliveries: Mutex<Vec<SessionRunDelivery>>,
    pub(crate) events: Mutex<Vec<&'static str>>,
    pub(crate) already_reserved: AtomicBool,
    /// `Some` enables budget-pause discovery; `None` keeps the neutral
    /// unsupported answer.
    pub(crate) budget_tickets: Mutex<Option<Vec<SessionBudgetResumeTicket>>>,
    pub(crate) budget_deliveries: Mutex<Vec<SessionBudgetResumeDelivery>>,
    /// Every forward Resource transition: (previous revision, desired revision,
    /// desired binding ids). Rollbacks are recorded separately.
    pub(crate) resource_applies: Mutex<Vec<(u64, u64, Vec<String>)>>,
    pub(crate) resource_rollbacks: Mutex<Vec<(u64, u64)>>,
    /// Fail a forward application whose desired manifest contains this binding,
    /// modelling the second input of a replacement failing its preparation
    /// after the first one was prepared.
    pub(crate) fail_apply_binding: Mutex<Option<String>>,
}

impl EffectCountingRuntime {
    pub(crate) fn counts(&self) -> EffectCounts {
        EffectCounts {
            model_calls: self.model_calls.load(Ordering::SeqCst),
            tool_calls: self.tool_calls.load(Ordering::SeqCst),
            reservations: self.reservations.lock().unwrap().len(),
            activations: self.deliveries.lock().unwrap().len(),
            budget_resumes: self.budget_deliveries.lock().unwrap().len(),
            resource_applies: self.resource_applies.lock().unwrap().len(),
        }
    }

    pub(crate) fn set_budget_tickets(&self, tickets: Vec<SessionBudgetResumeTicket>) {
        *self.budget_tickets.lock().unwrap() = Some(tickets);
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionRuntime for EffectCountingRuntime {
    async fn run(
        &self,
        _agent: &str,
        _thread: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        self.model_calls.fetch_add(1, Ordering::SeqCst);
        self.events.lock().unwrap().push("model");
        Ok(StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }

    async fn resume(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _decision: awaken_session_execution_contract::ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        self.tool_calls.fetch_add(1, Ordering::SeqCst);
        self.events.lock().unwrap().push("tool");
        Ok(StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }

    async fn resume_custom(
        &self,
        _thread: &str,
        _tool_use_id: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        _is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        self.tool_calls.fetch_add(1, Ordering::SeqCst);
        self.events.lock().unwrap().push("tool");
        Ok(StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }

    fn model(&self) -> String {
        "effect-counting-session-runtime".into()
    }
}

awaken_session_execution_contract::neutral_role!(StreamingRunRuntime for EffectCountingRuntime);
awaken_session_execution_contract::neutral_role!(TerminalCleanupRuntime for EffectCountingRuntime);
awaken_session_execution_contract::neutral_role!(OutcomeRuntime for EffectCountingRuntime);
awaken_session_execution_contract::neutral_role!(LiveInboxRuntime for EffectCountingRuntime);
awaken_session_execution_contract::neutral_role!(CapabilityRuntime for EffectCountingRuntime);
awaken_session_execution_contract::neutral_role!(SessionCoordinationReads for EffectCountingRuntime);
awaken_session_execution_contract::neutral_role!(EnvironmentRuntime for EffectCountingRuntime);
no_committed_threads!(EffectCountingRuntime);

#[async_trait::async_trait]
impl awaken_session_execution_contract::BudgetRuntime for EffectCountingRuntime {
    async fn resume_budget_reached(
        &self,
        delivery: SessionBudgetResumeDelivery,
    ) -> Result<SessionBudgetResumeDisposition, RunError> {
        self.events.lock().unwrap().push("budget_resume");
        self.budget_deliveries.lock().unwrap().push(delivery);
        Ok(SessionBudgetResumeDisposition::Dispatched)
    }

    async fn session_budget_resume_tickets(
        &self,
        session_id: &awaken_session_types::SessionId,
    ) -> Result<Vec<SessionBudgetResumeTicket>, RunError> {
        let configured = self.budget_tickets.lock().unwrap().clone();
        match configured {
            Some(tickets) => Ok(tickets),
            None => {
                awaken_session_execution_contract::NeutralRuntime
                    .session_budget_resume_tickets(session_id)
                    .await
            }
        }
    }

    async fn session_usage(
        &self,
        _thread: &str,
    ) -> Result<awaken_session_budget_contract::SessionUsage, RunError> {
        Ok(Default::default())
    }

    async fn session_thread_usage(
        &self,
        _session_id: &awaken_session_types::SessionId,
        _thread_id: &str,
    ) -> Result<awaken_session_budget_contract::SessionUsage, RunError> {
        Ok(Default::default())
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationRuntime for EffectCountingRuntime {
    fn observe_realization_request(
        &self,
    ) -> Result<awaken_lease_contract::authority::AuthorityProofStarted, RunError> {
        crate::tests::test_realization_request_started()
    }

    async fn renew_session_realization_projections(
        &self,
        control: &dyn awaken_session_realization_contract::SessionRealizationControl,
        owner: &str,
        runtime_incarnation: &str,
        timing: awaken_lease_contract::authority::AuthorityLeaseTiming,
    ) -> Result<usize, RunError> {
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
        self.events.lock().unwrap().push("projection");
        install_complete_test_projection(self, thread, projection, mode)
            .await
            .map_err(Into::into)
    }

    async fn renew_session_realization_lease(
        &self,
        thread: &str,
        lease: awaken_session_realization_contract::ObservedSessionRealizationLease,
    ) -> Result<(), RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .renew_session_realization_lease(thread, lease)
            .await
    }

    async fn invalidate_session_realization_lease(
        &self,
        thread: &str,
        lease: &crate::session_resource::SessionRealizationLease,
    ) -> Result<(), RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .invalidate_session_realization_lease(thread, lease)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::CoordinationRuntime for EffectCountingRuntime {
    async fn reserve_session_run(
        &self,
        command: AdmitSessionRun,
    ) -> Result<SessionRunReservation, RunError> {
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
        if let Some(delivery) = admission.delivery() {
            self.deliveries.lock().unwrap().push(delivery.clone());
        }
        Ok(StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }

    async fn admit_coordinated_run(
        &self,
        command: awaken_session_execution_contract::CoordinatedRunCommand,
    ) -> Result<awaken_session_execution_contract::SessionAgentMessageReceipt, RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .admit_coordinated_run(command)
            .await
    }

    async fn subscribe_session_thread_live(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<
        Option<Box<dyn awaken_session_execution_contract::SessionThreadLiveSubscription>>,
        RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .subscribe_session_thread_live(session_id, thread_id)
            .await
    }

    async fn continue_session_agent_report(
        &self,
        command: awaken_session_execution_contract::SessionAgentReportContinuation,
    ) -> Result<(), RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .continue_session_agent_report(command)
            .await
    }

    async fn interrupt_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        child_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<(), RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .interrupt_session_thread(session_id, child_thread_id)
            .await
    }

    async fn reply_session_thread_tool(
        &self,
        _delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), RunError> {
        self.tool_calls.fetch_add(1, Ordering::SeqCst);
        self.events.lock().unwrap().push("tool");
        Ok(())
    }

    async fn reply_and_observe_session_thread_tool(
        &self,
        _delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
        _stream_sink: Option<Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<StepOutcome, RunError> {
        self.tool_calls.fetch_add(1, Ordering::SeqCst);
        self.events.lock().unwrap().push("tool");
        Ok(StepOutcome::ended(
            Vec::new(),
            awaken_agent_contract::agent::run::EndCause::NaturalEnd,
        ))
    }

    async fn archive_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<(), RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .archive_session_thread(session_id, thread_id)
            .await
    }

    async fn quiesce_terminal_delegations(
        &self,
        thread: &str,
    ) -> Result<awaken_session_application_contract::DelegatedRunSnapshot, RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .quiesce_terminal_delegations(thread)
            .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::RealizationEffects for EffectCountingRuntime {
    fn validate_session_sandbox_layout(
        &self,
        thread: &str,
        layout: &awaken_session_configuration_contract::SessionSandboxLayout,
    ) -> Result<(), RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .validate_session_sandbox_layout(thread, layout)
    }

    async fn checkpoint_session_environment(
        &self,
        thread: &str,
        request: awaken_session_state_contract::SandboxCheckpointRequest,
    ) -> Result<awaken_session_state_contract::CheckpointReceipt, RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .checkpoint_session_environment(thread, request)
            .await
    }

    async fn prepare_checkpoint_source_disposal(
        &self,
        thread: &str,
        preparation: &awaken_session_state_contract::SourceReleasePreparationEffect,
        generation: &awaken_session_state_contract::SandboxGeneration,
        source_binding: &str,
    ) -> Result<awaken_session_state_contract::SourceReleasePreparedReceipt, RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .prepare_checkpoint_source_disposal(thread, preparation, generation, source_binding)
            .await
    }

    async fn dispose_prepared_checkpoint_source(
        &self,
        thread: &str,
        disposal: &awaken_session_state_contract::SourceReleaseDisposal,
    ) -> Result<awaken_session_state_contract::SourceDisposedReceipt, RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .dispose_prepared_checkpoint_source(thread, disposal)
            .await
    }

    async fn restore_checkpointed_session_environment(
        &self,
        request: awaken_session_state_contract::SandboxRestoreRequest,
    ) -> Result<awaken_session_state_contract::RestoreReceipt, RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .restore_checkpointed_session_environment(request)
            .await
    }

    async fn delete_session_checkpoint(
        &self,
        thread: &str,
        checkpoint: &awaken_session_state_contract::SandboxCheckpointRef,
    ) -> Result<(), RunError> {
        awaken_session_execution_contract::NeutralRuntime
            .delete_session_checkpoint(thread, checkpoint)
            .await
    }

    async fn apply_session_inputs(
        &self,
        _thread: &str,
        transition: &crate::session_resource::SessionResourceTransition,
    ) -> Result<(), RunError> {
        let previous = transition.previous().revision;
        let desired = transition.desired().revision;
        if desired < previous {
            self.resource_rollbacks
                .lock()
                .unwrap()
                .push((previous, desired));
            return Ok(());
        }
        let bindings = transition
            .desired()
            .resources
            .inputs()
            .iter()
            .map(|input| input.binding_id.as_str().to_string())
            .collect::<Vec<_>>();
        self.resource_applies
            .lock()
            .unwrap()
            .push((previous, desired, bindings.clone()));
        if let Some(failing) = self.fail_apply_binding.lock().unwrap().clone()
            && bindings.contains(&failing)
        {
            return Err(RunError::bad_request(format!(
                "injected preparation failure for Resource binding {failing}"
            )));
        }
        Ok(())
    }

    async fn execute_terminal_repository_publication_for_lease(
        &self,
        command: awaken_session_execution_contract::__macro_support::SessionRepositoryPublicationCommand,
        lease: &awaken_session_execution_contract::__macro_support::SessionRealizationLease,
    ) -> Result<
        awaken_session_execution_contract::__macro_support::SessionRepositoryPublicationEffect,
        RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .execute_terminal_repository_publication_for_lease(command, lease)
            .await
    }

    async fn dispose_terminal_cleanup_for_effect(
        &self,
        effect: awaken_session_execution_contract::__macro_support::SessionTerminalCleanupDisposalEffect,
    ) -> Result<
        awaken_session_execution_contract::__macro_support::SessionCleanupDisposalReceipt,
        RunError,
    > {
        awaken_session_execution_contract::NeutralRuntime
            .dispose_terminal_cleanup_for_effect(effect)
            .await
    }
}
