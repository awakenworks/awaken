// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: Static owner: the Session application's coordination commands served by
// @formal reason: ChildRunCoordination: Run reservation, activation, child admission, replies, interruption,
// @formal reason: archive and the dispatch reads they validate against; this is a product-requirement
// @formal reason: classification, not a direct proof claim.
//! The Session application's `ChildRunCoordination` role, served by
//! [`ChildRunCoordination`]. Coordination reads select the shared Thread component.

use std::sync::Arc;

use awaken_runtime_contract::runs::{RunError, StepOutcome};

use super::ChildRunCoordination;
use crate::session_execution_error::to_run_error;

#[async_trait::async_trait]
impl awaken_session_execution_contract::ChildRunCoordination for ChildRunCoordination {
    async fn run_prepared_session_run(
        &self,
        command: awaken_session_execution_contract::AdmitPreparedSessionRun,
        activity: &dyn awaken_session_execution_contract::SessionRunActivityAdmitter,
    ) -> Result<awaken_agent_contract::agent::run::RunState, RunError> {
        let commit = self
            .thread_reads
            .commit_for_read(command.session_id.as_str())
            .await?;
        awaken_session_run_delivery::run_prepared_session_run(
            command,
            |activation| self.run_assembly.prepare_dispatch(activation),
            awaken_session_run_delivery::PreparedSessionRunDelivery {
                owner: self.coordinator_dispatch().map_err(to_run_error)?,
                publication: self.session_runs.threads.publication(),
                completion: &self.session_runs.completion,
                commit,
                clock: self.session_runs.clock.as_ref(),
            },
            activity,
        )
        .await
    }

    async fn reserve_session_run(
        &self,
        command: awaken_session_execution_contract::AdmitSessionRun,
    ) -> Result<awaken_session_execution_contract::SessionRunReservation, RunError> {
        awaken_session_run_delivery::validate_session_run_command(&command)?;
        let session_id = command.session_id.clone();
        let (origin, original_dispatch) = if command.private_task_input.is_some() {
            let dispatch = self
                .session_runs
                .coordinator_dispatch()
                .map_err(to_run_error)?;
            let origin =
                awaken_session_run_delivery::select_session_run_origin(&command, dispatch.as_ref())
                    .await?;
            (origin, Some(dispatch))
        } else {
            (None, None)
        };
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
            .ctx_for_session_coordination(
                session_id.as_str(),
                Some(&command.agent_id),
                origin.as_deref(),
            )
            .await
            .map_err(to_run_error)?;
        let reservation_context_is_ephemeral = ctx.is_coordination_envelope();
        let reservation = async {
            let inputs = match origin {
                Some(source) => awaken_session_run_delivery::SessionRunInputs::Task(source),
                None => awaken_session_run_delivery::SessionRunInputs::Session {
                    snapshot: &ctx.config,
                    skills: ctx.skill_registry.as_deref(),
                    model_ref_override: self
                        .capabilities
                        .agent
                        .inference_routing
                        .override_for(session_id.as_str())?,
                },
            };
            let dispatch = match original_dispatch {
                Some(dispatch) => dispatch,
                None => self
                    .session_runs
                    .coordinator_dispatch()
                    .map_err(to_run_error)?,
            };
            awaken_session_run_delivery::reserve_session_run(
                command,
                inputs,
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
        self.thread_reads
            .subscribe_session_thread_live(&self.hub, session_id, thread_id)
            .await
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
        delivery: awaken_session_contract::SessionThreadToolReplyDelivery,
    ) -> Result<(), RunError> {
        self.threads().reply_session_thread_tool(delivery).await
    }

    async fn reply_and_observe_session_thread_tool(
        &self,
        delivery: awaken_session_contract::SessionThreadToolReplyDelivery,
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
