// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: An exported arrival commits Requested before reusing Runtime recovery.
// @formal reason: No MCP transport task mints an effect token or dispatches a tool.
//! Automatic MCP requests share the committed Runtime ToolBatch authority.
use super::{bind_runtime_execution, owned_tool_result_context};
use crate::{
    AcpRunExecutor, AgentSession, Error, Message, PermissionAsk, PermissionResume, RawAcpError,
    Result, RunActivation, RunState, RuntimeRunContext, Stage, StateCommand, ToolCall,
    assistant_step_base, classify_error, commit, ensure_pending_tool_use, finish_failure,
    open_agent_session, prompt_of, verify_attempt_ownership,
};

impl AcpRunExecutor {
    // These are the Step's existing staged values, not another execution context.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn handoff_runtime_tool(
        &self,
        activation: &RunActivation,
        context: &RuntimeRunContext,
        mut call: ToolCall,
        mut messages: Vec<Message>,
        first_staged_message: usize,
        next_assistant_step: &mut usize,
        audit: Vec<awaken_agent_contract::audit::draft::Draft>,
        unconsumed_resume: Option<&PermissionResume>,
        state: Vec<StateCommand>,
    ) -> Result<(RunState, Option<(String, AgentSession)>)> {
        let started = awaken_time::mono::Stopwatch::start();
        bind_runtime_execution(&mut call, &messages[first_staged_message..])?;
        ensure_pending_tool_use(
            &activation.run_id,
            next_assistant_step,
            &mut messages,
            super::PendingToolUseSource::Arrival(first_staged_message),
            &PermissionAsk {
                tool: call.tool_id.clone(),
                call_id: call.call_id.clone(),
                arguments: call.arguments.clone(),
            },
        )?;
        commit::commit_requested_tool(
            context,
            audit,
            unconsumed_resume,
            &activation.thread_id,
            activation.run_id.clone(),
            call,
            messages,
            state,
        )
        .await?;
        let owner = self.owned_tool_continuation.as_ref().ok_or_else(|| {
            Error::Execution("automatic handoff requires its Runtime owner".into())
        })?;
        tracing::info!(awaken.run.id = %activation.run_id.0, elapsed_ms = started.elapsed().as_millis() as u64,
            phase = "requested_commit", "ACP Runtime handoff phase completed");
        let recovery_started = awaken_time::mono::Stopwatch::start();
        let outcome = owner.recover(activation.clone(), context.clone()).await?;
        tracing::info!(awaken.run.id = %activation.run_id.0, elapsed_ms = recovery_started.elapsed().as_millis() as u64,
            phase = "runtime_recovery", "ACP Runtime handoff phase completed");
        if outcome != RunState::Running {
            return Ok((outcome, None));
        }
        let receipts = owned_tool_result_context(owner.as_ref(), activation, context)?
            .ok_or_else(|| Error::Execution("automatic handoff has no committed result".into()))?;
        *next_assistant_step = (*next_assistant_step).max(assistant_step_base(context, activation));
        verify_attempt_ownership(context.ownership.as_deref()).await?;
        let reopen_started = awaken_time::mono::Stopwatch::start();
        let mut session = match open_agent_session(self.source.as_ref(), activation, context).await
        {
            Ok(session) => session,
            Err(open) => {
                let failure = classify_error(Stage::Initialize, &RawAcpError::message(open.0));
                return Ok((
                    finish_failure(context, activation, &failure, None).await?,
                    None,
                ));
            }
        };
        tracing::info!(awaken.run.id = %activation.run_id.0, elapsed_ms = reopen_started.elapsed().as_millis() as u64,
            total_elapsed_ms = started.elapsed().as_millis() as u64, phase = "sdk_reopen",
            "ACP Runtime handoff phase completed");
        self.apply_session_mcp_projection(&mut session);
        Ok((RunState::Running, Some((prompt_of(&[receipts]), session))))
    }
}
