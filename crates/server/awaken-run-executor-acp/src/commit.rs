//! ACP's one ThreadCommit projection: messages, batch, ticket and terminal fact.
//! Transport notifications and one-shot permission grants never commit directly.
use super::*;

/// Commit a terminal failure that occurred before/instead of a Step (open fault):
/// the activation input and one assistant message with the failure prompt, plus
/// the terminal state.
pub(super) async fn finish_failure(
    context: &RuntimeRunContext,
    activation: &RunActivation,
    failure: &AcpFailure,
    unconsumed_resume: Option<&PermissionResume>,
) -> Result<RunState> {
    let disposition = RunDisposition::ended(activation.run_id.clone(), failure_cause(failure));
    let state = disposition.state();
    let mut messages = activation.input.clone();
    let mut next_assistant_step = assistant_step_base(context, activation);
    push_assistant_text(
        &mut messages,
        &activation.run_id,
        &mut next_assistant_step,
        failure.prompt(),
    );
    commit(
        context,
        unconsumed_resume,
        &activation.thread_id,
        disposition,
        messages,
        Vec::new(),
    )
    .await?;
    Ok(state)
}

/// Commit the Step's messages and disposition through the one boundary (G13).
pub(super) async fn commit(
    context: &RuntimeRunContext,
    unconsumed_resume: Option<&PermissionResume>,
    thread_id: &ThreadId,
    disposition: RunDisposition,
    mut messages: Vec<Message>,
    state: Vec<StateCommand>,
) -> Result<()> {
    // A final/new-Await commit consumes the prior permission continuation. If
    // that exact decision never granted execution, settle its original ToolUse
    // here, atomically with ticket replacement/removal. Do not infer non-execution
    // from a missing result after a grant was consumed: that outcome is unknown.
    if let Some(pending) = unconsumed_resume
        && !messages.iter().flat_map(|message| &message.content).any(|block| {
            matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == &pending.call_id)
        })
    {
        let reason = if pending.allow {
            "Tool request was not executed: the resumed agent did not reissue the approved tool with its exact arguments before this permission continuation ended. A changed request requires its own approval."
        } else {
            "Tool request was not executed because permission was denied."
        };
        messages.push(Message {
            id: acp_tool_message_id(disposition.run_id(), "permission-unexecuted", &pending.call_id),
            role: Role::Tool,
            content: vec![ContentBlock::tool_result_with_error(
                pending.call_id.clone(), vec![ContentBlock::text(reason)], true,
            )],
        });
    }
    // Only Awaken-owned continuations execute through the Runtime ToolBatch.
    // Opaque ACP permissions retain the explicit RunAttemptExecutor authority.
    let mut state = state;
    if let Some(ticket) = disposition.resume_ticket()
        && let AwaitTarget::ToolCall {
            reason: ToolAwaitReason::Permission,
            call_id,
            tool,
            authority: awaken_agent_contract::agent::awaiting::ToolCallAuthority::RuntimeToolBatch,
        } = ticket.target()
    {
        let mut batch = awaken_runtime_contract::ToolBatch::for_step(
            ticket.run_id.clone(),
            messages.len(),
            [(
                ToolCall {
                    call_id: call_id.clone(),
                    tool_id: tool.tool_id.clone(),
                    arguments: tool.arguments.clone(),
                },
                awaken_runtime_contract::tool::ToolRecoveryPolicy::default(),
            )],
        )
        .map_err(|error| Error::Commit(error.to_string()))?;
        batch
            .mark_awaiting(
                call_id,
                awaken_runtime_contract::ToolWaitKind::ToolPermission,
                &ticket.correlation_id,
            )
            .map_err(|error| Error::Commit(error.to_string()))?;
        state.push(awaken_runtime_contract::ActiveToolBatch::write(&Some(
            batch,
        )));
    } else {
        state.push(awaken_runtime_contract::ActiveToolBatch::remove());
    }
    let terminal = match disposition.state() {
        RunState::Ended(cause) => Some(CommittedTerminalRun {
            run_id: disposition.run_id().clone(),
            thread_id: thread_id.clone(),
            cause,
        }),
        RunState::Running | RunState::Awaiting => None,
    };
    let coordinator = context
        .commit
        .as_ref()
        .ok_or_else(|| Error::Commit("ACP execution requires a CommitCoordinator".to_string()))?;
    awaken_agent_contract::thread::commit::commit_run(
        coordinator.as_ref(),
        thread_id,
        disposition,
        messages,
        state,
    )
    .await
    .map_err(|e| Error::Commit(e.to_string()))?;

    if let Some(terminal) = &terminal {
        // Observation is post-commit and failure-isolated by the shared
        // runtime-contract helper. Stable-id recovery may redeliver.
        let _ = deliver_committed_terminal(&context.terminal_observers, terminal).await;
    }
    Ok(())
}
