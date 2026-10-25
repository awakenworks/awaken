// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: The Coordinator's coordination command and read roles over the dispatch owner and committed
// @formal reason: Session truth; an observer registers before its reply is staged and only committed truth
// @formal reason: settles it. Classification, not a formal-proof claim.
//! The Session application's coordination command and read roles, served by
//! [`WorkerPlacedSessions`] for the Sessions registered Workers execute.

use std::sync::Arc;

use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_session_application_contract::{
    DelegatedRun, DelegatedRunSnapshot, RunResume, StepOutcome,
};
use awaken_session_execution_contract::LiveInboxError;
use awaken_session_execution_contract::{
    AdmitSessionRun, AdmittedSessionRun, CoordinatedAdmissionRef, CoordinatedRunCommand,
    CoordinatedThreadLink, CoordinationRuntime, SessionAgentMessageCommand,
    SessionAgentMessageReceipt, SessionAgentReportContinuation, SessionCoordinationReads,
    SessionExecutionRecoveryObservation, SessionRunActivation, SessionRunDelivery,
    SessionRunReservation, SessionSourceDispatchObservation, SessionThreadLiveSubscription,
    SessionThreadToolReplyCommand, SessionThreadToolReplyDelivery, SessionThreadToolReplyFence,
};
use awaken_session_state_contract::PersistedSession;
use awaken_session_types::{RunError, SessionId};

use super::thread_runs::ThreadRun;
use super::{SETTLED_RECONCILIATION_INTERVAL, WorkerPlacedSessions};
use crate::session_run_activation::read_settled_phase;
use crate::{
    StepCommitExpectation, authoritative_step_snapshot, await_completion_state,
    project_committed_step, settled_step,
};

impl WorkerPlacedSessions {
    /// Publish one Session-owned reply and observe the resumed Run settle:
    /// the observer registers before the reply is staged, and only a newly
    /// committed ticket or end of the Run releases it.
    pub async fn reply_and_observe_session_thread_tool(
        &self,
        delivery: SessionThreadToolReplyDelivery,
        sink: Option<Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<StepOutcome, RunError> {
        let session = delivery.command.session_id.to_string();
        let run_id = delivery.command.expected_run_id.clone();
        let correlation = delivery.command.expected_correlation_id.clone();
        let thread = ThreadId(session.clone());
        let commit = self.ports.reads.commit_for_read(&session).await?;
        let messages_before = authoritative_step_snapshot(&commit, &thread, &run_id)
            .await?
            .messages
            .len();
        let (settled, _waiter_guard) = self.ports.completion.register(&run_id, sink)?;
        self.threads.reply_session_thread_tool(delivery).await?;
        let state = await_completion_state(settled, SETTLED_RECONCILIATION_INTERVAL, || {
            read_settled_phase(&commit, &thread, &run_id, Some(&correlation))
        })
        .await?;
        let step = self.step_projection(&session)?;
        let _projection = step.lock().await;
        settled_step(
            project_committed_step(
                &commit,
                &self.ports.hub,
                &thread,
                run_id,
                state,
                StepCommitExpectation {
                    messages_before,
                    input_ids: &[],
                },
                &session,
            )
            .await?,
        )
    }
}

#[async_trait::async_trait]
impl CoordinationRuntime for WorkerPlacedSessions {
    async fn reserve_session_run(
        &self,
        command: AdmitSessionRun,
    ) -> Result<SessionRunReservation, RunError> {
        WorkerPlacedSessions::reserve_session_run(self, command).await
    }

    async fn activate_session_run(
        &self,
        delivery: SessionRunDelivery,
    ) -> Result<SessionRunActivation, RunError> {
        WorkerPlacedSessions::activate_session_run(self, delivery).await
    }

    async fn activate_and_observe_session_run(
        &self,
        admission: AdmittedSessionRun,
        input_message_ids: Vec<String>,
        sink: Option<Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<StepOutcome, RunError> {
        WorkerPlacedSessions::activate_and_observe_session_run(
            self,
            admission,
            input_message_ids,
            sink,
        )
        .await
    }

    async fn admit_coordinated_run(
        &self,
        command: CoordinatedRunCommand,
    ) -> Result<SessionAgentMessageReceipt, RunError> {
        self.threads.admit_coordinated_run(command).await
    }

    async fn subscribe_session_thread_live(
        &self,
        session_id: &SessionId,
        thread_id: &str,
    ) -> Result<Option<Box<dyn SessionThreadLiveSubscription>>, RunError> {
        if thread_id == session_id {
            return Ok(Some(self.ports.hub.live_subscription(thread_id)?));
        }
        let links = self
            .ports
            .reads
            .coordinated_threads(session_id.as_str(), None)
            .await?;
        if !links
            .iter()
            .any(|link| link.session_id == *session_id && link.thread_id.0.as_str() == thread_id)
        {
            return Err(RunError::bad_request(
                "live observer target is not a coordinated Session Thread",
            ));
        }
        Ok(Some(self.ports.hub.live_subscription(thread_id)?))
    }

    async fn continue_session_agent_report(
        &self,
        command: SessionAgentReportContinuation,
    ) -> Result<(), RunError> {
        self.threads.continue_session_agent_report(command).await
    }

    async fn interrupt_session_thread(
        &self,
        session_id: &SessionId,
        child_thread_id: &ThreadId,
    ) -> Result<(), RunError> {
        self.threads
            .interrupt_session_thread(session_id.as_str(), child_thread_id)
            .await
    }

    async fn reply_session_thread_tool(
        &self,
        delivery: SessionThreadToolReplyDelivery,
    ) -> Result<(), RunError> {
        self.threads.reply_session_thread_tool(delivery).await
    }

    async fn reply_and_observe_session_thread_tool(
        &self,
        delivery: SessionThreadToolReplyDelivery,
        stream_sink: Option<Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<StepOutcome, RunError> {
        WorkerPlacedSessions::reply_and_observe_session_thread_tool(self, delivery, stream_sink)
            .await
    }

    async fn archive_session_thread(
        &self,
        session_id: &SessionId,
        thread_id: &str,
    ) -> Result<(), RunError> {
        self.threads
            .archive_session_thread(session_id.as_str(), &ThreadId(thread_id.to_owned()))
            .await
    }

    async fn quiesce_terminal_delegations(
        &self,
        thread: &str,
    ) -> Result<DelegatedRunSnapshot, RunError> {
        // The Coordinator executes no attempt: durable cancellation is the
        // whole quiescence, with no resident accelerator.
        self.threads
            .quiesce_terminal_delegations(thread, None)
            .await
    }
}

#[async_trait::async_trait]
impl SessionCoordinationReads for WorkerPlacedSessions {
    async fn source_dispatch_observation(
        &self,
        session: &PersistedSession,
    ) -> SessionSourceDispatchObservation {
        self.threads.source_dispatch_observation(session).await
    }

    async fn execution_recovery_observation(
        &self,
        session: &PersistedSession,
    ) -> SessionExecutionRecoveryObservation {
        self.threads.execution_recovery_observation(session).await
    }

    async fn coordinated_admission_receipt(
        &self,
        command: CoordinatedAdmissionRef<'_>,
    ) -> Result<Option<SessionAgentMessageReceipt>, RunError> {
        self.threads.coordinated_admission_receipt(command).await
    }

    async fn session_run_state(
        &self,
        session_id: &SessionId,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<awaken_agent_contract::agent::run::RunState>, RunError> {
        self.threads.session_run_state(session_id, run_id).await
    }

    async fn delegated_runs(&self, thread: &str) -> Result<Vec<DelegatedRun>, RunError> {
        SessionCoordinationReads::delegated_runs(self.threads.as_ref(), thread).await
    }

    async fn coordinated_threads(
        &self,
        session_id: &SessionId,
        observed_root: Option<&awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>,
    ) -> Result<Vec<CoordinatedThreadLink>, RunError> {
        SessionCoordinationReads::coordinated_threads(
            self.threads.as_ref(),
            session_id,
            observed_root,
        )
        .await
    }

    async fn session_thread_tool_reply_fence(
        &self,
        command: &SessionThreadToolReplyCommand,
    ) -> Result<SessionThreadToolReplyFence, RunError> {
        SessionCoordinationReads::session_thread_tool_reply_fence(self.threads.as_ref(), command)
            .await
    }

    async fn validate_session_agent_message_source(
        &self,
        command: &SessionAgentMessageCommand,
    ) -> Result<SessionAgentMessageCommand, RunError> {
        SessionCoordinationReads::validate_session_agent_message_source(
            self.threads.as_ref(),
            command,
        )
        .await
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::BudgetRuntime for WorkerPlacedSessions {
    async fn resume_budget_reached(
        &self,
        delivery: awaken_session_application_contract::SessionBudgetResumeDelivery,
    ) -> Result<awaken_session_application_contract::SessionBudgetResumeDisposition, RunError> {
        self.budgets.resume_budget_reached(delivery).await
    }

    async fn session_budget_resume_tickets(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<awaken_session_application_contract::SessionBudgetResumeTicket>, RunError> {
        self.budgets
            .session_budget_resume_tickets(session_id.as_str())
            .await
    }

    async fn session_usage(
        &self,
        thread: &str,
    ) -> Result<awaken_session_budget_contract::SessionUsage, RunError> {
        awaken_session_execution_contract::BudgetRuntime::session_usage(&self.budgets, thread).await
    }

    async fn session_thread_usage(
        &self,
        session_id: &SessionId,
        thread_id: &str,
    ) -> Result<awaken_session_budget_contract::SessionUsage, RunError> {
        awaken_session_execution_contract::BudgetRuntime::session_thread_usage(
            &self.budgets,
            session_id,
            thread_id,
        )
        .await
    }
}

/// The Coordinator owns no Run attempt, so no live inbox is ever active here:
/// a message for a running Run is delivered as an ordinary Session event.
#[async_trait::async_trait]
impl awaken_session_execution_contract::LiveInboxRuntime for WorkerPlacedSessions {
    async fn live_inbox_snapshot(
        &self,
        _thread: &str,
    ) -> Result<awaken_session_execution_contract::LiveInboxSnapshot, LiveInboxError> {
        Ok(awaken_session_execution_contract::LiveInboxSnapshot::inactive())
    }

    async fn live_inbox_queue(
        &self,
        _thread: &str,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<u64, LiveInboxError> {
        Err(LiveInboxError::Inactive)
    }

    async fn live_inbox_remove(&self, _thread: &str, _id: u64) -> Result<(), LiveInboxError> {
        Err(LiveInboxError::Inactive)
    }

    async fn live_inbox_replace(
        &self,
        _thread: &str,
        _id: u64,
        _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<(), LiveInboxError> {
        Err(LiveInboxError::Inactive)
    }

    async fn live_inbox_reorder(
        &self,
        _thread: &str,
        _order: Vec<u64>,
    ) -> Result<(), LiveInboxError> {
        Err(LiveInboxError::Inactive)
    }
}

impl WorkerPlacedSessions {
    fn capabilities_in(
        &self,
        workspace: &str,
    ) -> awaken_session_configuration_contract::AgentCapabilities {
        awaken_session_configuration_contract::AgentCapabilities {
            builtin_tools: crate::builtin_hand_tools()
                .into_iter()
                .map(|(name, ask)| awaken_session_configuration_contract::BuiltinTool { name, ask })
                .collect(),
            // No client-executed tool is configured on a Coordinator, and its
            // Skills are listed by the Skill catalog, not advertised here.
            custom_tools: Vec::new(),
            skills: Vec::new(),
            delegates: self.ports.projections.default_delegates(workspace),
        }
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::CapabilityRuntime for WorkerPlacedSessions {
    fn capabilities(
        &self,
    ) -> Result<awaken_session_configuration_contract::AgentCapabilities, RunError> {
        Ok(self.capabilities_in(self.ports.projections.local_workspace()))
    }

    /// | Rule | projection | advertised delegates |
    /// |---|---|---|
    /// | C1 | installed | the Session's own roster |
    /// | C2 | none | the Workspace default Agent's roster |
    fn capabilities_for(
        &self,
        thread: &str,
    ) -> Result<awaken_session_configuration_contract::AgentCapabilities, RunError> {
        let projection = self.ports.projections.projection(thread)?;
        let workspace = projection.as_ref().map_or_else(
            || self.ports.projections.local_workspace().to_owned(),
            |projection| projection.workspace().to_owned(),
        );
        let mut capabilities = self.capabilities_in(&workspace);
        if let Some(projection) = projection {
            capabilities.delegates = projection.baseline().delegate_ids.clone();
        }
        Ok(capabilities)
    }
}

/// A fresh user message carrying `content`.
fn user_message(
    content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
) -> awaken_agent_contract::agent::message::Message {
    awaken_agent_contract::agent::message::Message::new(
        awaken_agent_contract::agent::message::Id(awaken_agent_contract::fresh_process_id("usr")),
        awaken_agent_contract::agent::message::Role::User,
        content,
    )
}

fn thread_run<'a>(
    agent: Option<&'a str>,
    thread: &'a str,
    input: Vec<awaken_agent_contract::agent::message::Message>,
) -> ThreadRun<'a> {
    ThreadRun {
        agent,
        thread,
        input,
        sink: None,
        data_subject_id: None,
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::SessionRuntime for WorkerPlacedSessions {
    async fn run(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
    ) -> Result<StepOutcome, RunError> {
        self.run_thread(thread_run(Some(agent), thread, vec![user_message(content)]))
            .await
    }

    async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        decision: awaken_session_execution_contract::ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        self.resume_thread(thread, tool_use_id, RunResume::Permission(decision), None)
            .await
    }

    async fn resume_custom(
        &self,
        thread: &str,
        tool_use_id: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        self.resume_thread(
            thread,
            tool_use_id,
            RunResume::ClientResult { content, is_error },
            None,
        )
        .await
    }

    fn model(&self) -> String {
        self.ports.default_model.clone()
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::StreamingRunRuntime for WorkerPlacedSessions {
    async fn run_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        data_subject_id: Option<String>,
    ) -> Result<StepOutcome, RunError> {
        let mut run = thread_run(Some(agent), thread, vec![user_message(content)]);
        run.data_subject_id = data_subject_id.map(awaken_runtime_contract::DataSubjectId);
        self.run_thread(run).await
    }

    async fn run_streaming(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunError> {
        let mut run = thread_run(Some(agent), thread, vec![user_message(content)]);
        run.sink = Some(sink);
        self.run_thread(run).await
    }

    async fn run_streaming_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        data_subject_id: Option<String>,
        sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunError> {
        let mut run = thread_run(Some(agent), thread, vec![user_message(content)]);
        run.sink = Some(sink);
        run.data_subject_id = data_subject_id.map(awaken_runtime_contract::DataSubjectId);
        self.run_thread(run).await
    }

    async fn interrupt(&self, thread: &str) -> Result<(), RunError> {
        self.interrupt_thread(thread).await
    }

    /// The Coordinator selects no model per Run: a Session's model is the one
    /// its baseline froze.
    async fn rebind_model(&self, _thread: &str, _model: &str) -> Result<(), RunError> {
        Err(RunError::bad_request(
            "a Worker-placed Session runs the model its baseline froze",
        ))
    }

    async fn supports_mid_conversation_system(&self, thread: &str) -> Result<bool, RunError> {
        Ok(crate::supports_mid_conversation_system(
            &self.model_for_thread(thread)?,
        ))
    }

    async fn pending_tool(
        &self,
        thread: &str,
    ) -> Result<Option<awaken_session_application_contract::Pending>, RunError> {
        self.ports.reads.pending_tool(thread).await
    }
}

#[async_trait::async_trait]
impl awaken_session_application_contract::RunApplication for WorkerPlacedSessions {
    async fn run(
        &self,
        _operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<awaken_agent_contract::agent::message::Message>,
    ) -> Result<StepOutcome, RunError> {
        self.run_thread(thread_run(agent.as_deref(), thread, messages))
            .await
    }

    async fn run_streaming(
        &self,
        _operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<awaken_agent_contract::agent::message::Message>,
        sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunError> {
        let mut run = thread_run(agent.as_deref(), thread, messages);
        run.sink = Some(sink);
        self.run_thread(run).await
    }

    async fn resume(
        &self,
        _operation_id: &str,
        thread: &str,
        tool_use_id: &str,
        resume: RunResume,
    ) -> Result<StepOutcome, RunError> {
        self.resume_thread(thread, tool_use_id, resume, None).await
    }

    async fn resume_streaming(
        &self,
        _operation_id: &str,
        thread: &str,
        tool_use_id: &str,
        resume: RunResume,
        sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunError> {
        self.resume_thread(thread, tool_use_id, resume, Some(sink))
            .await
    }

    async fn interrupt(&self, thread: &str) -> Result<(), RunError> {
        self.interrupt_thread(thread).await
    }

    async fn pending(
        &self,
        thread: &str,
    ) -> Result<Option<awaken_session_application_contract::Pending>, RunError> {
        self.ports.reads.pending_tool(thread).await
    }

    async fn history(
        &self,
        thread: &str,
    ) -> Result<Vec<awaken_agent_contract::agent::message::Message>, RunError> {
        self.ports.reads.committed_messages(thread).await
    }

    fn model(&self) -> String {
        self.ports.default_model.clone()
    }

    async fn usage(&self, thread: &str) -> Result<(u64, u64), RunError> {
        let usage = self
            .ports
            .reads
            .session_thread_usage(thread, &ThreadId(thread.to_owned()))
            .await?;
        Ok((usage.input_tokens, usage.output_tokens))
    }
}
