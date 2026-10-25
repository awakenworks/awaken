// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: Static owner: the Session application's coordination command and read roles served by
// @formal reason: ChildRunCoordination: Run reservation, activation, child admission, replies, interruption,
// @formal reason: archive and the dispatch reads they validate against; this is a product-requirement
// @formal reason: classification, not a direct proof claim.
//! The Session application's `CoordinationRuntime` and
//! `SessionCoordinationReads` roles, served by [`ChildRunCoordination`].

use std::sync::Arc;

use awaken_session_application_contract::{DelegatedRun, StepOutcome};
use awaken_session_types::RunError;

use super::ChildRunCoordination;
use crate::session_execution_error::to_run_error;

#[async_trait::async_trait]
impl awaken_session_execution_contract::CoordinationRuntime for ChildRunCoordination {
    async fn reserve_session_run(
        &self,
        command: awaken_session_execution_contract::AdmitSessionRun,
    ) -> Result<awaken_session_execution_contract::SessionRunReservation, RunError> {
        awaken_session_run_delivery::validate_session_run_command(&command)?;
        let session_id = command.session_id.clone();
        // The Session application's Run reservation enters this host for the
        // Session: its install path, so the Session-dispatch mark has an installed Session.
        self.installed_sessions
            .install(session_id.as_str())
            .map_err(crate::installed_session::registry_run_error)?;
        self.realization_executor
            .validate_thread_resource_bindings(session_id.as_str())
            .await?;
        let ctx = self
            .run_assembly
            .ctx_for_session_coordination(session_id.as_str(), Some(&command.agent_id))
            .await
            .map_err(to_run_error)?;
        let reservation_context_is_ephemeral = ctx.is_coordination_envelope();
        let reservation = async {
            let model_ref_override = self
                .capabilities
                .agent
                .inference_routing
                .override_for(session_id.as_str())?;
            let dispatch = self
                .session_runs
                .coordinator_dispatch()
                .map_err(to_run_error)?;
            awaken_session_run_delivery::reserve_session_run(
                command,
                awaken_session_run_delivery::SessionRunInputs {
                    snapshot: &ctx.config,
                    skills: ctx.skill_registry.as_deref(),
                    model_ref_override,
                },
                |activation| self.run_assembly.prepare_dispatch(activation),
                dispatch.as_ref(),
            )
            .await
        }
        .await;
        if reservation_context_is_ephemeral {
            self.realization_executor
                .environments()
                .evict_session_for_rebuild(session_id.as_str())
                .await?;
        }
        reservation
    }

    async fn activate_session_run(
        &self,
        delivery: awaken_session_execution_contract::SessionRunDelivery,
    ) -> Result<awaken_session_execution_contract::SessionRunActivation, RunError> {
        self.session_runs
            .activate_session_run_reservation(delivery)
            .await
            .map_err(to_run_error)
    }

    async fn activate_and_observe_session_run(
        &self,
        admission: awaken_session_execution_contract::AdmittedSessionRun,
        input_message_ids: Vec<String>,
        sink: Option<Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<StepOutcome, RunError> {
        let receipt = self
            .session_runs
            .activate_and_observe_session_run(admission, input_message_ids, sink)
            .await
            .map_err(to_run_error)?;
        awaken_session_run_delivery::settled_step(receipt)
    }

    async fn admit_coordinated_run(
        &self,
        command: awaken_session_execution_contract::CoordinatedRunCommand,
    ) -> Result<awaken_session_execution_contract::SessionAgentMessageReceipt, RunError> {
        self.threads().admit_coordinated_run(command).await
    }

    async fn subscribe_session_thread_live(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<
        Option<Box<dyn awaken_session_execution_contract::SessionThreadLiveSubscription>>,
        RunError,
    > {
        if thread_id == session_id {
            return Ok(Some(self.hub.live_subscription(thread_id)?));
        }
        let links = self
            .thread_reads
            .coordinated_threads(session_id.as_str(), None)
            .await?;
        if !links
            .iter()
            .any(|link| link.session_id == session_id && link.thread_id.0.as_str() == thread_id)
        {
            return Err(RunError::bad_request(
                "live observer target is not a coordinated Session Thread",
            ));
        }
        Ok(Some(self.hub.live_subscription(thread_id)?))
    }

    async fn continue_session_agent_report(
        &self,
        command: awaken_session_execution_contract::SessionAgentReportContinuation,
    ) -> Result<(), RunError> {
        self.threads().continue_session_agent_report(command).await
    }

    async fn interrupt_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        child_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<(), RunError> {
        self.threads()
            .interrupt_session_thread(session_id.as_str(), child_thread_id)
            .await
    }

    async fn reply_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), RunError> {
        self.threads().reply_session_thread_tool(delivery).await
    }

    async fn reply_and_observe_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
        stream_sink: Option<std::sync::Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<StepOutcome, RunError> {
        let receipt = self
            .reply_and_observe_session_thread_tool(delivery, stream_sink)
            .await
            .map_err(to_run_error)?;
        awaken_session_run_delivery::settled_step(receipt)
    }

    async fn archive_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<(), RunError> {
        self.threads()
            .archive_session_thread(
                session_id.as_str(),
                &awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
            )
            .await
    }

    async fn quiesce_terminal_delegations(
        &self,
        thread: &str,
    ) -> Result<awaken_session_application_contract::DelegatedRunSnapshot, RunError> {
        self.session_runs
            .quiesce_terminal_delegations(thread)
            .await
            .map_err(to_run_error)
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionCoordinationReads for ChildRunCoordination {
    async fn source_dispatch_observation(
        &self,
        session: &awaken_session_state_contract::PersistedSession,
    ) -> awaken_session_execution_contract::SessionSourceDispatchObservation {
        self.threads().source_dispatch_observation(session).await
    }

    async fn execution_recovery_observation(
        &self,
        session: &awaken_session_state_contract::PersistedSession,
    ) -> awaken_session_execution_contract::SessionExecutionRecoveryObservation {
        self.threads().execution_recovery_observation(session).await
    }

    async fn coordinated_admission_receipt(
        &self,
        command: awaken_session_execution_contract::CoordinatedAdmissionRef<'_>,
    ) -> Result<Option<awaken_session_execution_contract::SessionAgentMessageReceipt>, RunError>
    {
        self.threads().coordinated_admission_receipt(command).await
    }

    async fn session_run_state(
        &self,
        session_id: &awaken_session_types::SessionId,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<awaken_agent_contract::agent::run::RunState>, RunError> {
        self.threads().session_run_state(session_id, run_id).await
    }

    async fn delegated_runs(&self, thread: &str) -> Result<Vec<DelegatedRun>, RunError> {
        self.threads().delegated_runs(thread).await
    }

    async fn coordinated_threads(
        &self,
        session_id: &awaken_session_types::SessionId,
        observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<Vec<awaken_session_execution_contract::CoordinatedThreadLink>, RunError> {
        self.threads()
            .coordinated_threads(session_id, observed_root)
            .await
    }

    async fn session_thread_tool_reply_fence(
        &self,
        command: &awaken_session_execution_contract::SessionThreadToolReplyCommand,
    ) -> Result<awaken_session_execution_contract::SessionThreadToolReplyFence, RunError> {
        awaken_session_execution_contract::SessionCoordinationReads::session_thread_tool_reply_fence(
            self.threads(),
            command,
        )
        .await
    }

    async fn validate_session_agent_message_source(
        &self,
        command: &awaken_session_execution_contract::SessionAgentMessageCommand,
    ) -> Result<awaken_session_execution_contract::SessionAgentMessageCommand, RunError> {
        awaken_session_execution_contract::SessionCoordinationReads::validate_session_agent_message_source(
            self.threads(),
            command,
        )
        .await
    }
}
