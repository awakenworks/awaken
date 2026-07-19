//! The per-step tool-dispatch branch of the execution loop.
//!
//! Split out of `engine/mod.rs` (the step-loop driver) so the "run this step's
//! tool calls" responsibility is isolated from "drive the loop", and the module
//! stays within the file-length limit. Items are shared through `use super::*`
//! (same-crate privates included), exactly like `inference`.

use super::*;

/// Run each requested tool call for one step, feeding each result back into the
/// transcript and folding tool-outcome reactions into state and reminders. Returns
/// `Some(RunDisposition)` when a call awaits input (delegation/permission/scheduled) or fails
/// it closed (an unpermitted scheduled-action kind) — the caller ends the step loop
/// with it; `None` when every call produced a result and the loop continues. The
/// reserved `tool_open` meta-tool bypasses the gate and only mutates `opened`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_tool_calls(
    runtime: &Runtime,
    context: &RuntimeRunContext,
    resolved: &ResolvedRun,
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    step: usize,
    calls: Vec<ToolCall>,
    transcript: &mut Vec<Message>,
    new_messages: &mut Vec<Message>,
    staged_state: &mut Vec<StateCommand>,
    audit: &mut Vec<EventDraft>,
    store: &mut Store,
    opened: &mut std::collections::BTreeSet<String>,
) -> Option<RunDisposition> {
    for call in calls {
        // The reserved `tool_open` meta-tool (ADR-0053) loads a deferred tool for
        // later steps: it mutates this run's `opened` set and returns a result, so it
        // bypasses the gate and never reaches an executor.
        let output = if call.tool_id == awaken_runtime_contract::resolved::TOOL_OPEN_ID {
            open_deferred_tool(&resolved.spec.tool_presentation, &call, opened)
        } else {
            let outcome = gate_decision(runtime, &call, env, store).await;
            // Audit the decision of a real (policy-backed) gate (ADR-0030).
            if runtime.gate().is_some() {
                audit.push(permission_audit(&call, &outcome));
            }
            match outcome {
                GateOutcome::Allow => match run_delegation(runtime, context, run_id, &call).await {
                    Some(Ok(DelegationStep::Ended { text, usage })) => {
                        // Fold the delegate's token spend into this thread's running
                        // tally, so a session's usage counts delegated work (the
                        // child Run has already committed its own usage).
                        merge_thread_usage(store, staged_state, &usage);
                        ToolOutput::ok(&call.call_id, text)
                    }
                    // The delegate awaiting needing input: await the parent on a
                    // Delegation ticket carrying the opaque handle (durable), resumed
                    // through the delegation executor.
                    Some(Ok(DelegationStep::Awaiting { continuation })) => {
                        let ticket = resume_ticket(
                            resolved,
                            run_id,
                            &call.call_id,
                            &call,
                            AwaitReason::Delegation,
                            Some(continuation),
                        );
                        emit(
                            context,
                            run_id,
                            AgentEvent::Fact(Fact::Awaiting {
                                pending_tool_use_id: ticket.call_id.clone(),
                            }),
                        )
                        .await;
                        return Some(RunDisposition::awaiting(ticket));
                    }
                    Some(Err(err)) => ToolOutput::error(&call.call_id, err.to_string()),
                    None => execute_tool(runtime, Some(env), &call, context).await,
                },
                GateOutcome::Block { reason } => {
                    ToolOutput::error(&call.call_id, format!("blocked: {reason}"))
                }
                GateOutcome::SetResult(output) => output,
                GateOutcome::Suspend { ticket_id } => {
                    // Await the run on a structured ticket carrying the pending
                    // call so an allow decision can run it later.
                    let ticket = resume_ticket(
                        resolved,
                        run_id,
                        &ticket_id,
                        &call,
                        AwaitReason::ToolPermission,
                        None,
                    );
                    emit(
                        context,
                        run_id,
                        AgentEvent::Fact(Fact::Awaiting {
                            pending_tool_use_id: ticket.call_id.clone(),
                        }),
                    )
                    .await;
                    return Some(RunDisposition::awaiting(ticket));
                }
                GateOutcome::Schedule {
                    correlation_id,
                    action_kind,
                } => {
                    // A plugin-owned scheduled-action kind must be in the resolved
                    // environment; an absent kind (its plugin not selected) fails
                    // the run closed (RS-SCH-005, ADR-0027).
                    if let Some(kind) = &action_kind
                        && !env.permits_action_kind(kind)
                    {
                        return Some(RunDisposition::ended(
                            run_id.clone(),
                            EndCause::Error(Failure::CapabilityBound),
                        ));
                    }
                    // Commit a ScheduledAction (ADR-0020): the call is deferred and
                    // performed later from the committed request, not decided.
                    let ticket = resume_ticket(
                        resolved,
                        run_id,
                        &correlation_id,
                        &call,
                        AwaitReason::ScheduledAction,
                        None,
                    );
                    emit(
                        context,
                        run_id,
                        AgentEvent::Fact(Fact::Awaiting {
                            pending_tool_use_id: ticket.call_id.clone(),
                        }),
                    )
                    .await;
                    return Some(RunDisposition::awaiting(ticket));
                }
            }
        };
        staged_state.extend(output.state.clone());
        let message = tool_result_message(&call, &output);
        transcript.push(message.clone());
        new_messages.push(message);

        // Post-execution reaction: fold the tool's own state into the live
        // store, then let tool-outcome hooks stage transitions and reminders.
        for command in &output.state {
            store.apply(command);
        }
        let (reactions, reminders) =
            collect_tool_reactions(env, run_id, step, &call, &output, store).await;
        for command in &reactions {
            store.apply(command);
        }
        staged_state.extend(reactions);
        for reminder in reminders {
            transcript.push(reminder.clone());
            new_messages.push(reminder);
        }
    }
    None
}
