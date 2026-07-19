//! Resume-path orchestration from committed thread truth.
//!
//! This module owns ticket-result folding, approval pre-commit, delegation
//! continuation, and re-entry into the ordinary execution loop.

use super::*;

/// Rebuild the transcript and state from committed truth, inject a resumed
/// `result` for the awaiting ticket, and drive the loop to its next terminal/awaiting
/// checkpoint. Shared by the two resume entries — a validated `ResumeCommand`
/// (`resume_run`) and a delegate step folded back into the parent
/// (`resume_delegation`) — so the rebuild/inject/drive/finalize glue lives once.
#[allow(clippy::too_many_arguments)]
pub(super) async fn drive_resumed(
    runtime: &Runtime,
    resolved: &ResolvedRun,
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    thread_id: &ThreadId,
    ticket: &ResumeTicket,
    result: ResumeResult,
    reader: &dyn ThreadReader,
    context: &RuntimeRunContext,
) -> Result<RunState> {
    let mut transcript = reader.committed_messages(thread_id);
    let mut store = store_from_commands(reader.committed_state(thread_id), run_id);
    let approved = matches!(&result, ResumeResult::Decision { allow: true, .. });
    let permission_decision = match &result {
        ResumeResult::Decision { allow, .. }
            if matches!(
                ticket.reason,
                AwaitReason::ToolPermission | AwaitReason::ScheduledAction
            ) =>
        {
            Some(if *allow { "approved" } else { "denied" })
        }
        _ => None,
    };
    let mut decision_precommitted = false;
    let mut delegation_state = Vec::new();
    if ticket.reason == AwaitReason::Delegation
        && matches!(&result, ResumeResult::ToolResult(_))
        && let Some(pending) = &ticket.pending_tool
    {
        let call = ToolCall {
            call_id: ticket.call_id.clone().unwrap_or_default(),
            tool_id: pending.tool_id.clone(),
            arguments: pending.arguments.clone(),
        };
        stage_delegation_completed(runtime, run_id, &call, &mut store, &mut delegation_state)?;
    }
    // Approval is a lifecycle decision, not a chat message. For a batch-aware
    // ticket, commit Awaiting(Approval) -> Executing before entering the tool.
    // A failed/fenced commit therefore cannot leak an unrecorded side effect.
    if matches!(&result, ResumeResult::Decision { allow: true, .. })
        && matches!(
            ticket.reason,
            AwaitReason::ToolPermission | AwaitReason::ScheduledAction
        )
        && let Some(call_id) = ticket.call_id.as_deref()
        && let Some(mut batch) = ActiveToolBatch::load(&store)
            .map_err(|error| Error::Execution(error.to_string()))?
            .filter(|batch| batch.run_id == *run_id && batch.phase == ToolBatchPhase::Open)
    {
        let wait_kind = match ticket.reason {
            AwaitReason::ToolPermission => ToolWaitKind::Approval,
            AwaitReason::ScheduledAction => ToolWaitKind::ScheduledAction,
            _ => unreachable!("guarded by the resume reason above"),
        };
        batch
            .resume_executing(call_id, wait_kind, &ticket.correlation_id)
            .map_err(|error| Error::Execution(error.to_string()))?;
        let command = ActiveToolBatch::write(&Some(batch));
        let mut approval_state = Vec::new();
        if let Some(pending) = &ticket.pending_tool {
            let call = ToolCall {
                call_id: call_id.to_string(),
                tool_id: pending.tool_id.clone(),
                arguments: pending.arguments.clone(),
            };
            stage_delegation_request(
                runtime,
                resolved,
                ticket.initiator.as_ref(),
                run_id,
                &call,
                &mut store,
                &mut approval_state,
            )?;
        }
        approval_state.push(command.clone());
        if let Some(coordinator) = &context.commit {
            coordinator
                .commit(ThreadCommit::assemble(
                    thread_id.clone(),
                    RunDisposition::running(run_id.clone()),
                    true,
                    Vec::new(),
                    approval_state,
                    vec![
                        RunEvent::PermissionDecided {
                            tool_id: ticket
                                .pending_tool
                                .as_ref()
                                .map(|tool| tool.tool_id.clone())
                                .unwrap_or_default(),
                            call_id: call_id.to_string(),
                            decision: "approved".to_string(),
                        }
                        .into(),
                    ],
                ))
                .await
                .map_err(|error| Error::Commit(error.to_string()))?;
        }
        store.apply(&command);
        decision_precommitted = true;
    }
    let result = if approved && let Some(pending) = &ticket.pending_tool {
        let call = ToolCall {
            call_id: ticket.call_id.clone().unwrap_or_default(),
            tool_id: pending.tool_id.clone(),
            arguments: pending.arguments.clone(),
        };
        let delegation_started = runtime
            .delegation_executor()
            .is_some_and(|executor| executor.tool_id() == call.tool_id);
        let result = match run_delegation(
            runtime,
            context,
            ticket.initiator.as_ref(),
            &resolved.agent_id.0,
            run_id,
            &call,
        )
        .await
        {
            Some(Ok(DelegationStep::Ended { text, usage })) => {
                merge_thread_usage(&mut store, &mut delegation_state, &usage);
                ResumeResult::ToolResult(ToolOutput::ok(&call.call_id, text))
            }
            Some(Ok(DelegationStep::Awaiting { continuation })) => {
                let next_ticket = resume_ticket(
                    resolved,
                    run_id,
                    ticket.initiator.as_ref(),
                    &call.call_id,
                    &call,
                    AwaitReason::Delegation,
                    Some(continuation),
                );
                let mut batch = ActiveToolBatch::load(&store)
                    .map_err(|error| Error::Execution(error.to_string()))?
                    .ok_or_else(|| {
                        Error::Execution(
                            "approved delegation is missing its committed tool batch".to_string(),
                        )
                    })?;
                batch
                    .mark_awaiting(
                        &call.call_id,
                        ToolWaitKind::Delegation,
                        next_ticket.correlation_id.clone(),
                    )
                    .map_err(|error| Error::Execution(error.to_string()))?;
                delegation_state.push(ActiveToolBatch::write(&Some(batch)));
                return finish(
                    context,
                    thread_id,
                    run_id.clone(),
                    RunStepResult {
                        new_messages: Vec::new(),
                        staged_state: delegation_state,
                        audit: Vec::new(),
                        disposition: RunDisposition::awaiting(next_ticket),
                    },
                )
                .await;
            }
            Some(Err(error)) => {
                ResumeResult::ToolResult(ToolOutput::error(&call.call_id, error.to_string()))
            }
            None => result,
        };
        if delegation_started {
            stage_delegation_completed(runtime, run_id, &call, &mut store, &mut delegation_state)?;
        }
        result
    } else {
        result
    };
    let (resumed, mut seed_state, resumed_output) =
        resume_into_messages(runtime, env, run_id, ticket, result, &store, context).await;
    seed_state.splice(0..0, delegation_state);
    let seed_audit = if decision_precommitted {
        Vec::new()
    } else {
        permission_decision
            .map(|decision| {
                vec![
                    RunEvent::PermissionDecided {
                        tool_id: ticket
                            .pending_tool
                            .as_ref()
                            .map(|tool| tool.tool_id.clone())
                            .unwrap_or_default(),
                        call_id: ticket.call_id.clone().unwrap_or_default(),
                        decision: decision.to_string(),
                    }
                    .into(),
                ]
            })
            .unwrap_or_default()
    };
    // The resumed tool's own state is folded into the store so a later step in
    // this resume observes the advanced state; it also seeds the attempt's batch
    // (kept first) so step commits and conflict validation cover it too.
    for command in &seed_state {
        store.apply(command);
    }
    // If this ticket belongs to an open tool batch, the resumed result satisfies
    // that call but stays behind the batch publication barrier. Recovery of the
    // batch below will finish any remaining calls and publish all results in the
    // original model order. A no-tool pause/input keeps the historic direct path.
    let resumed_new_messages = if let Some(mut batch) = ActiveToolBatch::load(&store)
        .map_err(|error| Error::Execution(error.to_string()))?
        .filter(|batch| batch.run_id == *run_id && batch.phase == ToolBatchPhase::Open)
        && let Some(call_id) = ticket.call_id.as_deref()
        && batch
            .calls
            .iter()
            .any(|entry| entry.call.call_id == call_id)
    {
        let output = resumed_output.ok_or_else(|| {
            Error::Execution("tool wait resumed without a tool output".to_string())
        })?;
        batch
            .complete(output)
            .map_err(|error| Error::Execution(error.to_string()))?;
        batch
            .set_result_messages(call_id, resumed)
            .map_err(|error| Error::Execution(error.to_string()))?;
        batch
            .set_result_state(call_id, seed_state.clone())
            .map_err(|error| Error::Execution(error.to_string()))?;
        let command = ActiveToolBatch::write(&Some(batch));
        store.apply(&command);
        seed_state = vec![command];
        Vec::new()
    } else {
        transcript.extend(resumed.iter().cloned());
        resumed
    };
    let step_result = drive(
        runtime,
        resolved,
        env,
        run_id,
        thread_id,
        context,
        ticket.initiator.as_ref(),
        transcript,
        resumed_new_messages,
        RESUME_STEP_BASE,
        store,
        seed_state,
        seed_audit,
    )
    .await?;
    finalize(context, thread_id, run_id.clone(), step_result).await
}
