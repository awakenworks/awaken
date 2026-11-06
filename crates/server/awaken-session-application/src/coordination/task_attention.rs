// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Existing Session coordination commands recover the original immutable Task input,
// @formal reason: then reuse Session activity/dispatch and publication owners. No new state or authority.
//! Task callbacks admitted on the same Session's existing logical Threads.
use super::{MAX_UNARCHIVED_AGENT_CHILD_THREADS, SessionCoordination};
use crate::SessionRootReads as _;
use awaken_agent_contract::{RunId, ThreadId};
use awaken_runtime_contract::runs::RunError;
use awaken_session_execution_contract::{CoordinatedRunIntent, session_run_activity_operation_id};

impl SessionCoordination {
    /// Build the original command only from committed membership and the
    /// incoming immutable subject. Receipt recovery must not resolve a live
    /// publication, realization or a new activity before reading that fact.
    pub(crate) async fn task_attention_child_command(
        &self,
        attention: &awaken_session_application_contract::TaskAttention,
    ) -> Result<awaken_session_execution_contract::AdmitSessionRun, RunError> {
        let session_id = awaken_session_types::SessionId::from(attention.tenure.session_thread());
        let thread_id = ThreadId(attention.tenure.thread.clone());
        let session = self
            .queries
            .session(&session_id)
            .await
            .map_err(crate::session_repository_error)?;
        let root_agent = &session
            .frozen_baseline()
            .ok_or_else(|| RunError::unavailable("Task Session has no frozen root Agent"))?
            .agent_id;
        let (agent_id, _) = self
            .coordination_reads
            .task_callback_source(&session_id, &thread_id, root_agent)
            .await?
            .ok_or_else(|| {
                RunError::unavailable("Task child Thread has no retained Session creation evidence")
            })?;
        Ok(awaken_session_execution_contract::AdmitSessionRun {
            session_id,
            agent_id,
            operation_id: attention.operation_id.clone(),
            run_id: awaken_session_execution_contract::session_run_id(
                &awaken_session_types::SessionId::from(attention.tenure.thread.as_str()),
                &attention.operation_id,
            ),
            messages: vec![attention.message.clone()],
            data_subject_id: None,
            traceparent: None,
            execution_requirements: Default::default(),
            replacement: awaken_session_types::SessionRunReplacement::PreservePrior,
            private_task_input: attention.input.clone(),
        })
    }

    pub(crate) async fn submit_child_task_attention(
        &self,
        task: awaken_session_execution_contract::AdmitSessionRun,
    ) -> Result<awaken_session_application_contract::TaskAttentionAck, RunError> {
        let tenure = task
            .private_task_input
            .as_ref()
            .and_then(|input| input.tenure())
            .ok_or_else(|| RunError::bad_request("Task child requires its complete subject"))?;
        let thread_id = ThreadId(tenure.thread.clone());
        let parent_run_id = RunId(tenure.run.clone());
        let parent_call_id = tenure.task.clone();
        let (owner, session) = self.coordination_session(&task.session_id).await?;
        let root_agent = &session
            .frozen_baseline()
            .ok_or_else(|| RunError::unavailable("Task Session has no frozen root Agent"))?
            .agent_id;
        let (agent_id, origin) = self
            .coordination_reads
            .task_callback_source(&task.session_id, &thread_id, root_agent)
            .await?
            .ok_or_else(|| {
                RunError::unavailable("Task callback creation evidence is unavailable")
            })?;
        if agent_id != task.agent_id {
            return Err(RunError::conflict(
                "Task callback Agent differs from its immutable command",
            ));
        }
        let source = self
            .coordination_reads
            .task_origin_execution(&task.session_id, &thread_id, &parent_run_id)
            .await?;
        if source.0.snapshot.root_agent_id.0 != agent_id {
            return Err(RunError::unavailable(
                "Task origin publication differs from committed Thread Agent",
            ));
        }
        if source.0.delegation_origin.as_ref() != Some(&origin) {
            return Err(RunError::unavailable(
                "Task origin execution differs from committed Thread creation",
            ));
        }
        let snapshot = source.0.snapshot.clone();
        self.realizations
            .install_dispatch_projection(&owner, &session)
            .await
            .map_err(|error| RunError::unavailable(error.to_string()))?;
        let (_, epoch) = self
            .activities
            .begin_activity_for_operation_with_policy(
                &task.session_id,
                &task.operation_id,
                false,
                None,
                None,
                Some((&thread_id, &task.run_id)),
            )
            .await
            .map_err(crate::SessionActivityError::run_error)?;
        let run_id = task.run_id.clone();
        let session_id = task.session_id.clone();
        let command = awaken_session_execution_contract::CoordinatedRunCommand {
            intent: CoordinatedRunIntent::FollowUp,
            session_id: session_id.clone(),
            thread_id: thread_id.clone(),
            run_id: run_id.clone(),
            parent_run_id,
            parent_call_id,
            operation_id: task.operation_id.clone(),
            snapshot,
            message: String::new(),
            session_activity_epoch: Some(epoch),
            max_unarchived_threads: MAX_UNARCHIVED_AGENT_CHILD_THREADS,
            task_attention: Some(task),
            task_attention_execution: Some(source),
        };
        let result = self.coordination.admit_coordinated_run(command).await;
        self.settle_definitive_coordination_rejection(session_id.as_str(), epoch, &result)
            .await?;
        let receipt = result?;
        if receipt.thread_id != thread_id {
            return Err(RunError::unavailable(
                "Task child admission answered for another Thread",
            ));
        }
        Ok(awaken_session_application_contract::TaskAttentionAck { run_id })
    }

    pub(crate) async fn task_attention_receipt(
        &self,
        command: &awaken_session_execution_contract::AdmitSessionRun,
    ) -> Result<bool, RunError> {
        let Some(observed) = self
            .coordination_reads
            .session_run_command_receipt(command)
            .await?
        else {
            return Ok(false);
        };
        let tenure = command
            .private_task_input
            .as_ref()
            .and_then(|input| input.tenure())
            .ok_or_else(|| {
                RunError::unavailable("original Task command has no complete subject")
            })?;
        let thread = ThreadId(tenure.thread.clone());
        let operation = if matches!(
            tenure.partition,
            awaken_session_application_contract::TaskReadPartition::Session { .. }
        ) {
            command.operation_id.clone()
        } else {
            session_run_activity_operation_id(&command.session_id, &command.run_id)
        };
        let (_, epoch) = self
            .activities
            .recover_activity_for_operation(
                &command.session_id,
                &operation,
                Some((&thread, &command.run_id)),
            )
            .await
            .map_err(crate::SessionActivityError::run_error)?
            .ok_or_else(|| {
                RunError::unavailable("original Task activity receipt is unavailable")
            })?;
        match observed {
            awaken_session_execution_contract::SessionRunReservation::Completed => Ok(true),
            awaken_session_execution_contract::SessionRunReservation::AlreadyActivated {
                session_activity_epoch,
            } if session_activity_epoch == epoch && epoch > 0 => Ok(true),
            _ => Err(RunError::unavailable(
                "original Task activity and dispatch receipts differ",
            )),
        }
    }
}
