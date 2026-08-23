//! Resume-path orchestration from committed thread truth.
//!
//! This module owns ticket-result folding, approval pre-commit, delegation
//! continuation, and re-entry into the ordinary execution loop.

use super::delegation::is_resolved_advisor_call;
use super::*;

/// Select the context that still needs to be committed for one durable resume.
/// Exact committed Messages are replay; a stable id with another payload is a
/// conflict. Running this before environment resolution also preserves accepted
/// System context when the resumed Run terminates at the capability boundary.
pub(super) fn fresh_resume_context(
    committed: &[Message],
    context_messages: Vec<Message>,
) -> Result<Vec<Message>> {
    let mut fresh_context = Vec::with_capacity(context_messages.len());
    for message in context_messages {
        match committed
            .iter()
            .find(|committed| committed.id == message.id)
        {
            Some(committed) if committed == &message => {}
            Some(_) => {
                return Err(Error::Execution(format!(
                    "resume context Message `{}` conflicts with committed truth",
                    message.id.0
                )));
            }
            None => fresh_context.push(message),
        }
    }
    Ok(fresh_context)
}

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
    fresh_context: Vec<Message>,
    reader: &dyn CommittedThreadView,
    context: &RuntimeRunContext,
) -> Result<RunState> {
    let committed = reader.committed_messages(thread_id);
    // Cause/effect decision table for resumed assistant ids:
    // R1 no prior assistant for this Run -> start at step 0;
    // R2 prior full steps 0..N -> resume at N+1;
    // R3 truncated partials/other Runs -> ignore them when deriving N.
    // This keeps retry/replay ids stable while preventing two distinct scheduled
    // resumes from both minting the former fixed `resume-step-1000` id.
    let resume_step_base =
        awaken_agent_contract::agent::message::next_assistant_step(&committed, run_id);
    let mut transcript = model_transcript(context, committed);
    transcript.extend(fresh_context.iter().cloned());
    let mut store = store_from_commands(reader.committed_state(thread_id), run_id);
    let approved = matches!(
        &result,
        ResumeResult::Permission(PermissionDecision::Allow { .. })
    );
    let permission_decision = match &result {
        ResumeResult::Permission(decision)
            if matches!(
                ticket.reason(),
                AwaitReason::ToolPermission | AwaitReason::ScheduledAction
            ) =>
        {
            Some(match decision {
                PermissionDecision::Allow { .. } => "approved",
                PermissionDecision::Deny { .. } => "denied",
            })
        }
        _ => None,
    };
    let mut decision_precommitted = false;
    let mut delegation_state = Vec::new();
    if ticket.reason() == AwaitReason::Delegation
        && matches!(&result, ResumeResult::ToolResult(_))
        && let Some((call_id, pending)) = ticket.tool_call()
    {
        let call = ToolCall {
            call_id: call_id.to_string(),
            tool_id: pending.tool_id.clone(),
            arguments: pending.arguments.clone(),
        };
        stage_delegation_completed(runtime, run_id, &call, &mut store, &mut delegation_state)?;
    }
    // Approval is a lifecycle decision, not a chat message. For a batch-aware
    // ticket, commit Awaiting(Approval) -> Executing before entering the tool.
    // A failed/fenced commit therefore cannot leak an unrecorded side effect.
    if matches!(
        &result,
        ResumeResult::Permission(PermissionDecision::Allow { .. })
    ) && matches!(
        ticket.reason(),
        AwaitReason::ToolPermission | AwaitReason::ScheduledAction
    ) && let Some((call_id, pending)) = ticket.tool_call()
        && let Some(mut batch) = ActiveToolBatch::load(&store)
            .map_err(|error| Error::Execution(error.to_string()))?
            .filter(|batch| batch.run_id() == run_id && batch.phase() == ToolBatchPhase::Open)
    {
        let wait_kind = match ticket.reason() {
            AwaitReason::ToolPermission => ToolWaitKind::ToolPermission,
            AwaitReason::ScheduledAction => ToolWaitKind::ScheduledAction,
            _ => unreachable!("guarded by the resume reason above"),
        };
        batch
            .resume_executing(call_id, wait_kind, &ticket.correlation_id)
            .map_err(|error| Error::Execution(error.to_string()))?;
        let command = ActiveToolBatch::write(&Some(batch));
        let mut approval_state = Vec::new();
        let call = ToolCall {
            call_id: call_id.to_string(),
            tool_id: pending.tool_id.clone(),
            arguments: pending.arguments.clone(),
        };
        stage_delegation_request(
            runtime,
            resolved,
            ticket.delegation_origin.as_ref(),
            run_id,
            &call,
            &mut store,
            &mut approval_state,
        )?;
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
                            tool_id: pending.tool_id.clone(),
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
    let result = if approved && let Some((call_id, pending)) = ticket.tool_call() {
        let call = ToolCall {
            call_id: call_id.to_string(),
            tool_id: pending.tool_id.clone(),
            arguments: pending.arguments.clone(),
        };
        let delegation_started = is_resolved_delegation_call(
            runtime,
            resolved,
            ticket.delegation_origin.as_ref(),
            &call,
        );
        let result = match run_delegation(
            runtime,
            resolved,
            DelegationParent {
                context,
                origin: ticket.delegation_origin.as_ref(),
                agent_id: &resolved.agent_id.0,
                run_id,
                thread_id,
            },
            &call,
            &mut store,
        )
        .await
        {
            Some(Ok(DelegationStep::Ended { text, usage })) => {
                merge_thread_usage(&mut store, &mut delegation_state, &usage);
                ResumeResult::ToolResult(ToolOutput::ok(&call.call_id, text))
            }
            Some(Ok(DelegationStep::Awaiting { continuation })) => {
                stage_delegation_awaiting(
                    runtime,
                    run_id,
                    &call,
                    &continuation,
                    &mut store,
                    &mut delegation_state,
                )?;
                let next_ticket = resume_ticket(
                    context,
                    resolved,
                    run_id,
                    ticket.delegation_origin.as_ref(),
                    &call.call_id,
                    &call,
                    ToolAwaitReason::Delegation,
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
                    runtime,
                    context,
                    thread_id,
                    run_id.clone(),
                    RunStepResult {
                        new_messages: fresh_context,
                        staged_state: delegation_state,
                        audit: Vec::new(),
                        disposition: RunDisposition::awaiting(next_ticket),
                    },
                )
                .await;
            }
            Some(Err(error)) => {
                ResumeResult::ToolResult(delegation_error_output(resolved, &call, error)?)
            }
            None if is_resolved_advisor_call(resolved, &call) => {
                ResumeResult::ToolResult(ToolOutput::error(
                    &call.call_id,
                    awaken_runtime_contract::resolved::ADVISOR_UNAVAILABLE_NOTICE,
                ))
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
        resume_into_messages(runtime, env, run_id, ticket, result, &store, context).await?;
    seed_state.splice(0..0, delegation_state);
    let seed_audit = if decision_precommitted {
        Vec::new()
    } else if let (Some(decision), Some((call_id, pending))) =
        (permission_decision, ticket.tool_call())
    {
        vec![
            RunEvent::PermissionDecided {
                tool_id: pending.tool_id.clone(),
                call_id: call_id.to_string(),
                decision: decision.to_string(),
            }
            .into(),
        ]
    } else {
        Vec::new()
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
        .filter(|batch| batch.run_id() == run_id && batch.phase() == ToolBatchPhase::Open)
        && let Some(call_id) = ticket.call_id()
        && batch
            .calls()
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
    let mut new_messages = fresh_context;
    new_messages.extend(resumed_new_messages);
    let step_result = drive(
        runtime,
        resolved,
        env,
        run_id,
        thread_id,
        context,
        ticket.delegation_origin.as_ref(),
        transcript,
        new_messages,
        Default::default(),
        resume_step_base,
        store,
        seed_state,
        seed_audit,
    )
    .await?;
    finalize(runtime, context, thread_id, run_id.clone(), step_result).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::message::Id as MessageId;

    #[test]
    fn resume_context_replay_is_exact_or_conflicting() {
        // Cause/effect graph: C1 a stable context id is absent, committed with
        // the exact payload, or committed with another payload. Effects: E1 an
        // absent Message remains fresh; E2 exact replay produces no second
        // Message; E3 a different payload fails before Runtime commits anything.
        //
        // | Rule | committed same id | payload | Effect |
        // |---|---|---|---|
        // | R1 | no | - | E1 fresh |
        // | R2 | yes | exact | E2 no-op |
        // | R3 | yes | changed | E3 conflict |
        // Constraints/invariants: stable Message id+payload is one idempotency
        // coordinate; replay cannot duplicate or overwrite committed context.
        let exact = Message::text(
            MessageId("session-system-reply".into()),
            Role::System,
            "context",
        );
        assert_eq!(
            fresh_resume_context(&[], vec![exact.clone()]).expect("R1"),
            vec![exact.clone()],
            "R1/E1"
        );
        assert!(
            fresh_resume_context(std::slice::from_ref(&exact), vec![exact.clone()])
                .expect("R2")
                .is_empty(),
            "R2/E2"
        );
        let changed = Message::text(exact.id.clone(), Role::System, "changed");
        assert!(
            fresh_resume_context(std::slice::from_ref(&exact), vec![changed]).is_err(),
            "R3/E3"
        );
    }
}
