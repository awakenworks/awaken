//! Delegated child-Run dispatch and resume.
//!
//! Kept separate from the main loop so parent/child correlation and continuation
//! handling form one bounded responsibility.

use super::*;

/// Commit the immutable parent/call/child relationship through the same Run
/// state delta that makes executor entry recoverable.
pub(super) fn stage_delegation_request(
    runtime: &Runtime,
    resolved: &ResolvedRun,
    initiator: Option<&DelegationOrigin>,
    run_id: &RunId,
    call: &ToolCall,
    store: &mut Store,
    staged_state: &mut Vec<StateCommand>,
) -> Result<bool> {
    let Some(executor) = runtime.delegation_executor() else {
        return Ok(false);
    };
    if executor.tool_id() != call.tool_id {
        return Ok(false);
    }
    let origin = delegation_origin(initiator, &resolved.agent_id.0, run_id, &call.call_id)
        .map_err(|error| Error::Execution(error.to_string()))?;
    let target_agent_id = executor
        .target_agent_id(&call.arguments)
        .map_err(|error| Error::Execution(error.to_string()))?;
    let mut registry = RunDelegations::load(store)
        .map_err(|error| Error::Execution(error.to_string()))?
        .unwrap_or_else(|| {
            DelegationRegistry::new(
                run_id.clone(),
                resolved.agent_id.0.clone(),
                initiator
                    .map(|origin| origin.agent_lineage.clone())
                    .unwrap_or_default(),
                initiator.map_or(0, |origin| origin.depth),
                resolved.spec.delegation_limits,
            )
        });
    let child_run_id = origin.child_run_id();
    registry
        .request(RequestDelegation {
            id: origin.delegation_id,
            parent_call_id: call.call_id.clone(),
            target_agent_id,
            child_run_id,
        })
        .map_err(|error| Error::Execution(error.to_string()))?;
    registry
        .validate()
        .map_err(|error| Error::Execution(error.to_string()))?;
    let command = RunDelegations::write(&Some(registry));
    store.apply(&command);
    staged_state.push(command);
    Ok(true)
}

/// Release the relationship's parallel slot in the same commit that records the
/// terminal ToolBatch result. The result payload itself remains ToolBatch-owned.
pub(super) fn stage_delegation_completed(
    runtime: &Runtime,
    run_id: &RunId,
    call: &ToolCall,
    store: &mut Store,
    staged_state: &mut Vec<StateCommand>,
) -> Result<()> {
    if runtime
        .delegation_executor()
        .is_none_or(|executor| executor.tool_id() != call.tool_id)
    {
        return Ok(());
    }
    let mut registry = RunDelegations::load(store)
        .map_err(|error| Error::Execution(error.to_string()))?
        .ok_or_else(|| Error::Execution("delegation relationship is not committed".into()))?;
    registry
        .complete(&DelegationId::for_parent_call(run_id, &call.call_id))
        .map_err(|error| Error::Execution(error.to_string()))?;
    let command = RunDelegations::write(&Some(registry));
    store.apply(&command);
    staged_state.push(command);
    Ok(())
}

fn delegation_resume_input(result: &ResumeResult) -> String {
    match result {
        ResumeResult::ToolResult(output) => output.content.clone(),
        ResumeResult::Input(text) => text.clone(),
        ResumeResult::Decision { note, .. } => note.clone().unwrap_or_default(),
    }
}

/// Resume an awaiting delegation and fold the resulting child boundary back into
/// the initiating Run.
#[allow(clippy::too_many_arguments)]
pub(super) async fn resume_delegation(
    runtime: &Runtime,
    ticket: &ResumeTicket,
    result: ResumeResult,
    resolved: &ResolvedRun,
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    thread_id: &ThreadId,
    reader: &dyn ThreadReader,
    context: &RuntimeRunContext,
) -> Result<RunState> {
    let call_id = ticket.call_id.clone().unwrap_or_default();
    let handle = ticket
        .pending_tool
        .as_ref()
        .and_then(|tool| tool.resume_handle.clone())
        .unwrap_or(serde_json::Value::Null);
    let input = delegation_resume_input(&result);

    let Some(executor) = runtime.delegation_executor() else {
        return finish(
            context,
            thread_id,
            run_id.clone(),
            RunStepResult::capability_bound(run_id.clone()),
        )
        .await;
    };
    let step = match delegation_origin(
        ticket.initiator.as_ref(),
        &resolved.agent_id.0,
        run_id,
        &call_id,
    ) {
        Ok(origin) => {
            executor
                .resume(DelegationResume {
                    child_run_id: origin.child_run_id(),
                    result_id: origin.result_id(),
                    context: context.for_child_run(),
                    origin,
                    continuation: handle,
                    input,
                })
                .await
        }
        Err(error) => Err(error),
    };

    let synthetic = match step {
        Ok(DelegationStep::Ended { text, .. }) => {
            ResumeResult::ToolResult(ToolOutput::ok(&call_id, text))
        }
        Ok(DelegationStep::Awaiting { continuation }) => {
            let mut next_ticket = ticket.clone();
            if let Some(pending) = next_ticket.pending_tool.as_mut() {
                pending.resume_handle = Some(continuation);
            }
            return finish(
                context,
                thread_id,
                run_id.clone(),
                RunStepResult::awaiting(next_ticket),
            )
            .await;
        }
        Err(err) => ResumeResult::ToolResult(ToolOutput::error(&call_id, err.to_string())),
    };

    drive_resumed(
        runtime, resolved, env, run_id, thread_id, ticket, synthetic, reader, context,
    )
    .await
}

/// Run `call` through the configured delegation executor. `None` means the call
/// belongs to the ordinary tool registry.
pub(super) async fn run_delegation(
    runtime: &Runtime,
    context: &RuntimeRunContext,
    initiator: Option<&DelegationOrigin>,
    parent_agent_id: &str,
    run_id: &RunId,
    call: &ToolCall,
) -> Option<std::result::Result<DelegationStep, DelegationExecutionError>> {
    let executor = runtime.delegation_executor()?;
    if executor.tool_id() != call.tool_id {
        return None;
    }
    let origin = match delegation_origin(initiator, parent_agent_id, run_id, &call.call_id) {
        Ok(origin) => origin,
        Err(error) => return Some(Err(error)),
    };
    let request = DelegationRequest {
        child_run_id: origin.child_run_id(),
        result_id: origin.result_id(),
        context: context.for_child_run(),
        origin,
        arguments: call.arguments.clone(),
    };
    Some(executor.start(request).await)
}

fn delegation_origin(
    initiator: Option<&DelegationOrigin>,
    parent_agent_id: &str,
    parent_run_id: &RunId,
    parent_call_id: &str,
) -> std::result::Result<DelegationOrigin, DelegationExecutionError> {
    match initiator {
        Some(parent) => DelegationOrigin::nested_for_agent(
            parent_run_id.clone(),
            parent_call_id,
            parent.depth,
            &parent.agent_lineage,
            parent_agent_id,
        )
        .map_err(|error| DelegationExecutionError::new(error.to_string())),
        None => Ok(DelegationOrigin::root_for_agent(
            parent_run_id.clone(),
            parent_call_id,
            parent_agent_id,
        )),
    }
}
