// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: Static owner: the Session application's coordination command and read roles served by
// @formal reason: ChildRunCoordination: Run reservation, activation, child admission, replies, interruption,
// @formal reason: archive and the dispatch reads they validate against; this is a product-requirement
// @formal reason: classification, not a direct proof claim.
//! The Session application's `CoordinationRuntime` and
//! `SessionCoordinationReads` roles, served by [`ChildRunCoordination`].

use std::sync::Arc;

use awaken_dispatch_contract::SessionRunReservations as _;
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
        let session_id = command.session_id.clone();
        if command.session_id.as_str().trim().is_empty()
            || command.agent_id.trim().is_empty()
            || command.operation_id.trim().is_empty()
            || command.run_id.0.trim().is_empty()
            || command.messages.is_empty()
        {
            return Err(RunError::bad_request(
                "Session Run reservation is incomplete",
            ));
        }
        // Freeze the Session application's complete immutable command before
        // Skill expansion or any current Agent/model/Resource/placement
        // projection can change the delivery payload reconstructed by a retry.
        let command_fingerprint = command.command_fingerprint();
        let input = command.messages.clone();
        // The Session application's Run reservation enters this host for the
        // Session: its install path, so the Session-dispatch mark has a slot.
        self.installed_sessions
            .install(command.session_id.as_str())
            .map_err(crate::installed_session::registry_run_error)?;
        self.realization_executor
            .validate_thread_resource_bindings(command.session_id.as_str())
            .await?;
        let ctx = self
            .run_assembly
            .ctx_for_session_coordination(command.session_id.as_str(), Some(&command.agent_id))
            .await
            .map_err(to_run_error)?;
        let reservation_context_is_ephemeral = ctx.is_coordination_envelope();
        let reservation = async {
            let input = match &ctx.skill_registry {
                Some(registry) => awaken_ext_skills::expand_slash_commands(
                    registry.as_ref(),
                    command.session_id.as_str(),
                    input,
                )
                .map_err(RunError::internal)?,
                None => input,
            };
            let (_, mut activation) =
                ctx.runtime
                    .prepare(&ctx.config, command.session_id.to_string(), input);
            activation.run_id = command.run_id;
            // Application requirements may only narrow the Session-owned tool
            // authority; they cannot restore a capability removed by the
            // frozen Session profile.
            activation = activation
                .narrow_tools(command.execution_requirements.tool_capability_narrowing)
                .map_err(RunError::bad_request)?;
            activation.model_ref_override = self
                .capabilities
                .agent
                .inference_routing
                .override_for(command.session_id.as_str())?;
            activation.data_subject_id = command
                .data_subject_id
                .map(awaken_runtime_contract::DataSubjectId);
            let mut request = self
                .run_assembly
                .prepare_dispatch(activation)
                .with_traceparent(command.traceparent)
                .map_err(|error| to_run_error(error.into()))?
                .with_session_command_fingerprint(command_fingerprint)
                .with_session_run_replacement(command.replacement);
            if !command
                .execution_requirements
                .required_worker_capabilities
                .is_empty()
            {
                // Protocol-specific capabilities are implemented only by a
                // registered Worker; never fall back to a generic local runner.
                let strict_remote =
                    awaken_dispatch_contract::PlacementRequirements::remote_required();
                request.placement.location = strict_remote.location;
                if request.placement.contract_version == 0 {
                    request.placement.contract_version = strict_remote.contract_version;
                    request.placement.dispatch_contract_version =
                        strict_remote.dispatch_contract_version;
                    request.placement.runtime_protocol_version =
                        strict_remote.runtime_protocol_version;
                }
                request
                    .placement
                    .required_capabilities
                    .extend(command.execution_requirements.required_worker_capabilities);
            }
            match self
                .session_runs
                .coordinator_dispatch()
                .map_err(to_run_error)?
                .reserve_session_run(request, 30_000)
                .await
                .map_err(|error| RunError::unavailable(error.to_string()))?
            {
                awaken_dispatch_contract::SessionRunReservationOutcome::Reserved => {
                    Ok(awaken_session_execution_contract::SessionRunReservation::Reserved)
                }
                awaken_dispatch_contract::SessionRunReservationOutcome::AlreadyReserved => {
                    Ok(awaken_session_execution_contract::SessionRunReservation::AlreadyReserved)
                }
                awaken_dispatch_contract::SessionRunReservationOutcome::RecoveryClaimed => {
                    Ok(awaken_session_execution_contract::SessionRunReservation::RecoveryClaimed)
                }
                awaken_dispatch_contract::SessionRunReservationOutcome::AlreadyActivated {
                    session_activity_epoch,
                } => Ok(
                    awaken_session_execution_contract::SessionRunReservation::AlreadyActivated {
                        session_activity_epoch,
                    },
                ),
                awaken_dispatch_contract::SessionRunReservationOutcome::Completed => {
                    Ok(awaken_session_execution_contract::SessionRunReservation::Completed)
                }
                awaken_dispatch_contract::SessionRunReservationOutcome::Conflict => Err(
                    RunError::bad_request("Session Run id was reused with different input"),
                ),
            }
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
        crate::step_projection::settled_step(receipt)
    }

    async fn admit_coordinated_run(
        &self,
        command: awaken_session_execution_contract::CoordinatedRunCommand,
    ) -> Result<awaken_session_execution_contract::SessionAgentMessageReceipt, RunError> {
        self.admit_coordinated_run(command)
            .await
            .map_err(to_run_error)
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
            .await
            .map_err(to_run_error)?;
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
        self.continue_session_agent_report(command)
            .await
            .map_err(to_run_error)
    }

    async fn interrupt_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        child_thread_id: &awaken_agent_contract::agent::thread::Id,
    ) -> Result<(), RunError> {
        self.session_runs
            .interrupt_session_thread(session_id.as_str(), child_thread_id)
            .await
            .map_err(to_run_error)
    }

    async fn reply_session_thread_tool(
        &self,
        delivery: awaken_session_execution_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), RunError> {
        self.reply_session_thread_tool(delivery)
            .await
            .map_err(to_run_error)
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
        crate::step_projection::settled_step(receipt)
    }

    async fn archive_session_thread(
        &self,
        session_id: &awaken_session_types::SessionId,
        thread_id: &str,
    ) -> Result<(), RunError> {
        self.archive_session_thread(
            session_id.as_str(),
            &awaken_agent_contract::agent::thread::Id(thread_id.to_string()),
        )
        .await
        .map_err(to_run_error)
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
        let Ok(source) = self.session_runs.coordinator_dispatch() else {
            return Default::default();
        };
        awaken_dispatch_execution::read_source_dispatch_observation(source.as_ref(), session).await
    }

    async fn execution_recovery_observation(
        &self,
        session: &awaken_session_state_contract::PersistedSession,
    ) -> awaken_session_execution_contract::SessionExecutionRecoveryObservation {
        let Ok(source) = self.session_runs.coordinator_dispatch() else {
            return awaken_session_execution_contract::SessionExecutionRecoveryObservation::NotProven {
                reason: awaken_session_execution_contract::SessionExecutionRecoveryGap::HistoryUnavailable,
            };
        };
        awaken_dispatch_execution::read_execution_recovery(
            source.as_ref(),
            self.thread_reads.as_ref(),
            session,
        )
        .await
    }

    async fn coordinated_admission_receipt(
        &self,
        command: awaken_session_execution_contract::CoordinatedAdmissionRef<'_>,
    ) -> Result<Option<awaken_session_execution_contract::SessionAgentMessageReceipt>, RunError> {
        use awaken_dispatch_contract::SessionRunAdmissionObservation;
        if command.activity_epoch() == Some(0)
            || matches!(command, awaken_session_execution_contract::CoordinatedAdmissionRef::Report(_, baseline)
                if baseline.agent_revision.is_none_or(|revision| revision == 0))
        {
            return Err(RunError::unavailable(
                "original coordination activity epoch is invalid",
            ));
        }
        let run = command.run_id();
        let source = self
            .session_runs
            .coordinator_dispatch()
            .map_err(|error| RunError::unavailable(error.message))?;
        let reads: &dyn awaken_dispatch_contract::SessionRunAdmissionReads = source.as_ref();
        let observed = reads
            .observe_session_run_admission(&run)
            .await
            .map_err(|error| RunError::unavailable(error.to_string()))?;
        match observed {
            SessionRunAdmissionObservation::Missing => Ok(None),
            SessionRunAdmissionObservation::Live(request)
                if request.identity_scope
                    == awaken_dispatch_contract::DispatchIdentityScope::FullDispatch
                    && request.session_command_fingerprint.is_none()
                    && request.session_thread_id.as_ref()
                        == Some(&awaken_agent_contract::agent::thread::Id(
                            command.session_id().to_string(),
                        ))
                    && command.activity_epoch().is_some_and(|epoch| epoch != 0)
                    && request.session_activity_epoch == command.activity_epoch()
                    && awaken_dispatch_execution::coordinated_activation_matches(
                        &command,
                        &request.activation,
                    ) =>
            {
                Ok(Some(
                    awaken_session_execution_contract::SessionAgentMessageReceipt {
                        thread_id: command.thread_id(),
                    },
                ))
            }
            _ => Err(RunError::unavailable(
                "original coordinated admission evidence is incomplete or mismatched",
            )),
        }
    }

    async fn session_run_state(
        &self,
        session_id: &awaken_session_types::SessionId,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<awaken_agent_contract::agent::run::RunState>, RunError> {
        let commit = self
            .thread_reads
            .commit_for_read(session_id.as_str())
            .await
            .map_err(to_run_error)?;
        commit
            .authoritative_run(run_id)
            .await
            .map(|record| record.map(|record| record.state))
            .map_err(|error| RunError::unavailable(error.to_string()))
    }

    async fn delegated_runs(&self, thread: &str) -> Result<Vec<DelegatedRun>, RunError> {
        self.thread_reads
            .delegated_runs(thread)
            .await
            .map_err(to_run_error)
    }

    async fn coordinated_threads(
        &self,
        session_id: &awaken_session_types::SessionId,
        observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<Vec<awaken_session_execution_contract::CoordinatedThreadLink>, RunError> {
        self.thread_reads
            .coordinated_threads(session_id.as_str(), observed_root)
            .await
            .map_err(to_run_error)
    }

    async fn session_thread_tool_reply_fence(
        &self,
        command: &awaken_session_execution_contract::SessionThreadToolReplyCommand,
    ) -> Result<awaken_session_execution_contract::SessionThreadToolReplyFence, RunError> {
        self.session_thread_tool_reply_fence(command)
            .await
            .map_err(to_run_error)
    }

    async fn validate_session_agent_message_source(
        &self,
        command: &awaken_session_execution_contract::SessionAgentMessageCommand,
    ) -> Result<awaken_session_execution_contract::SessionAgentMessageCommand, RunError> {
        self.validate_session_agent_message_source(command)
            .await
            .map_err(to_run_error)
    }
}
