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
    initiator: Option<&DelegationOrigin>,
    resolved: &ResolvedRun,
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    thread_id: &ThreadId,
    step: usize,
    calls: Vec<ToolCall>,
    ledger: &mut StepLedger,
    store: &mut Store,
    opened: &mut std::collections::BTreeSet<String>,
) -> Result<Option<RunDisposition>> {
    let policies = calls.iter().cloned().map(|call| {
        let policy = if runtime
            .delegation_executor()
            .is_some_and(|executor| executor.tool_id() == call.tool_id)
        {
            // A delegated child is addressed by its stable RunId. Recovery
            // reconnects/re-dispatches that durable request; treating it as a
            // generic non-replayable side effect would strand an Open child
            // relationship after an owner crash.
            ToolRecoveryPolicy::durable_request()
        } else if call.tool_id == awaken_runtime_contract::resolved::TOOL_OPEN_ID {
            ToolRecoveryPolicy {
                mode: ToolRecoveryMode::ReplaySafe,
                ..ToolRecoveryPolicy::default()
            }
        } else {
            resolved
                .spec
                .tool_descriptors
                .iter()
                .find(|descriptor| descriptor.id == call.tool_id)
                .map(|descriptor| descriptor.recovery_policy.clone())
                .unwrap_or_default()
        };
        (call, policy)
    });
    let mut batch = ToolBatch::new(
        ToolBatchId::for_step(run_id, step),
        run_id.clone(),
        policies,
    )
    .map_err(|error| Error::Execution(error.to_string()))?;

    // Commit the assistant tool-use blocks and the complete Requested batch before
    // entering any executor. Recovery can now distinguish "never entered" from an
    // unknown in-flight external effect.
    persist_batch(&batch, ledger, store, context, thread_id, run_id).await?;

    for call in calls {
        // The reserved `tool_open` meta-tool (ADR-0053) loads a deferred tool for
        // later steps: it mutates this run's `opened` set and returns a result, so it
        // bypasses the gate and never reaches an executor.
        let mut entered_executor = false;
        let mut delegation_started = false;
        let output = if call.tool_id == awaken_runtime_contract::resolved::TOOL_OPEN_ID {
            open_deferred_tool(&resolved.spec.tool_presentation, &call, opened)
        } else {
            let outcome = gate_decision(runtime, &call, env, store).await;
            // Audit the decision of a real (policy-backed) gate (ADR-0030).
            if runtime.gate().is_some() {
                ledger.audit.push(permission_audit(&call, &outcome));
            }
            match outcome {
                GateOutcome::Allow => {
                    delegation_started = stage_delegation_request(
                        runtime,
                        resolved,
                        initiator,
                        run_id,
                        &call,
                        store,
                        &mut ledger.staged_state,
                    )?;
                    let capability = recovery_capability(runtime, context, env, &call);
                    let policy = batch
                        .calls
                        .iter()
                        .find(|entry| entry.call.call_id == call.call_id)
                        .expect("batch was built from calls")
                        .recovery_policy
                        .clone();
                    if let Err(error) = policy.validate(capability) {
                        ToolOutput::error(&call.call_id, error.to_string())
                    } else {
                        batch
                            .mark_executing(&call.call_id)
                            .map_err(|error| Error::Execution(error.to_string()))?;
                        persist_batch(&batch, ledger, store, context, thread_id, run_id).await?;
                        entered_executor = true;
                        match run_delegation(
                            runtime,
                            context,
                            initiator,
                            &resolved.agent_id.0,
                            run_id,
                            &call,
                        )
                        .await
                        {
                            Some(Ok(DelegationStep::Ended { text, usage })) => {
                                // Fold the delegate's token spend into this thread's running
                                // tally, so a session's usage counts delegated work (the
                                // child Run has already committed its own usage).
                                merge_thread_usage(store, &mut ledger.staged_state, &usage);
                                ToolOutput::ok(&call.call_id, text)
                            }
                            // The delegate awaiting needing input: await the parent on a
                            // Delegation ticket carrying the opaque handle (durable), resumed
                            // through the delegation executor.
                            Some(Ok(DelegationStep::Awaiting { continuation })) => {
                                let ticket = resume_ticket(
                                    resolved,
                                    run_id,
                                    initiator,
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
                                batch
                                    .mark_awaiting(
                                        &call.call_id,
                                        ToolWaitKind::Delegation,
                                        ticket.correlation_id.clone(),
                                    )
                                    .map_err(|error| Error::Execution(error.to_string()))?;
                                stage_batch(&batch, ledger, store);
                                return Ok(Some(RunDisposition::awaiting(ticket)));
                            }
                            Some(Err(err)) => ToolOutput::error(&call.call_id, err.to_string()),
                            None => execute_tool(runtime, Some(env), &call, context).await,
                        }
                    }
                }
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
                        initiator,
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
                    batch
                        .mark_awaiting(
                            &call.call_id,
                            ToolWaitKind::Approval,
                            ticket.correlation_id.clone(),
                        )
                        .map_err(|error| Error::Execution(error.to_string()))?;
                    stage_batch(&batch, ledger, store);
                    return Ok(Some(RunDisposition::awaiting(ticket)));
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
                        return Ok(Some(RunDisposition::ended(
                            run_id.clone(),
                            EndCause::Error(Failure::CapabilityBound),
                        )));
                    }
                    // Commit a ScheduledAction (ADR-0020): the call is deferred and
                    // performed later from the committed request, not decided.
                    let ticket = resume_ticket(
                        resolved,
                        run_id,
                        initiator,
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
                    batch
                        .mark_awaiting(
                            &call.call_id,
                            ToolWaitKind::ScheduledAction,
                            ticket.correlation_id.clone(),
                        )
                        .map_err(|error| Error::Execution(error.to_string()))?;
                    stage_batch(&batch, ledger, store);
                    return Ok(Some(RunDisposition::awaiting(ticket)));
                }
            }
        };

        if delegation_started {
            stage_delegation_completed(runtime, run_id, &call, store, &mut ledger.staged_state)?;
        }

        if entered_executor {
            batch
                .complete(output.clone())
                .map_err(|error| Error::Execution(error.to_string()))?;
        } else {
            batch
                .complete_immediate(output.clone())
                .map_err(|error| Error::Execution(error.to_string()))?;
        }
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
        let mut result_state = output.state.clone();
        result_state.extend(reactions);
        let mut result_messages = vec![tool_result_message(&call, &output)];
        result_messages.extend(reminders);
        batch
            .set_result_messages(&call.call_id, result_messages)
            .map_err(|error| Error::Execution(error.to_string()))?;
        batch
            .set_result_state(&call.call_id, result_state)
            .map_err(|error| Error::Execution(error.to_string()))?;

        // The output, tool-owned state, reactions, and terminal call phase become
        // durable together. No later recovery re-enters this terminal call.
        if !batch.is_complete() {
            persist_batch(&batch, ledger, store, context, thread_id, run_id).await?;
        }
    }

    batch
        .finalize()
        .map_err(|error| Error::Execution(error.to_string()))?;
    for entry in &batch.calls {
        ledger.staged_state.extend(entry.result_state.clone());
        for message in &entry.result_messages {
            ledger.push_message(message.clone());
        }
    }
    // Publication barrier: Finalized and every ordered tool-result message are one
    // commit, so the model never observes a partial parallel/sequential batch.
    persist_batch(&batch, ledger, store, context, thread_id, run_id).await?;
    Ok(None)
}

/// Recover an open batch committed by a prior owner. Requested calls have not
/// entered an executor and may start normally. Executing calls follow their
/// pinned policy; terminal calls are only published, never invoked again.
#[allow(clippy::too_many_arguments)]
pub(super) async fn recover_tool_batch(
    runtime: &Runtime,
    context: &RuntimeRunContext,
    initiator: Option<&DelegationOrigin>,
    resolved: &ResolvedRun,
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    thread_id: &ThreadId,
    mut batch: ToolBatch,
    ledger: &mut StepLedger,
    store: &mut Store,
    opened: &mut std::collections::BTreeSet<String>,
) -> Result<Option<RunDisposition>> {
    // Rehydrate terminal per-call effects into the attempt's live state. They
    // remain unpublished StateCommands until Finalized, but later gates/hooks in
    // this recovery must observe the same state as the original attempt.
    for entry in &batch.calls {
        if entry.phase.is_terminal() {
            for command in &entry.result_state {
                store.apply(command);
            }
        }
    }
    for index in 0..batch.calls.len() {
        let durable = batch.calls[index].clone();
        let call = durable.call;
        match &durable.phase {
            ToolCallPhase::Completed(_) | ToolCallPhase::Indeterminate { .. } => continue,
            ToolCallPhase::Awaiting { wait } => {
                // Awaiting and its ResumeTicket commit atomically. Seeing an open
                // batch wait while the Run was reclaimed as Running means persisted
                // truth drifted; fail closed instead of inventing a resume message.
                batch
                    .mark_indeterminate(
                        &call.call_id,
                        format!("orphan {:?} correlation {}", wait.kind, wait.correlation_id),
                    )
                    .map_err(|error| Error::Execution(error.to_string()))?;
                let output = batch.output(&call.call_id).expect("now terminal");
                batch
                    .set_result_messages(&call.call_id, vec![tool_result_message(&call, &output)])
                    .map_err(|error| Error::Execution(error.to_string()))?;
                if let Some(disposition) = abandon_delegation_on_indeterminate(
                    runtime, run_id, &call, &batch, ledger, store,
                ) {
                    return Ok(Some(disposition));
                }
                persist_batch(&batch, ledger, store, context, thread_id, run_id).await?;
                continue;
            }
            ToolCallPhase::Executing { .. }
                if durable.recovery_policy.mode == ToolRecoveryMode::NeverReplay =>
            {
                batch
                    .mark_indeterminate(
                        &call.call_id,
                        "owner lease expired after the executor was entered",
                    )
                    .map_err(|error| Error::Execution(error.to_string()))?;
                let output = batch.output(&call.call_id).expect("now terminal");
                batch
                    .set_result_messages(&call.call_id, vec![tool_result_message(&call, &output)])
                    .map_err(|error| Error::Execution(error.to_string()))?;
                if let Some(disposition) = abandon_delegation_on_indeterminate(
                    runtime, run_id, &call, &batch, ledger, store,
                ) {
                    return Ok(Some(disposition));
                }
                persist_batch(&batch, ledger, store, context, thread_id, run_id).await?;
                continue;
            }
            ToolCallPhase::Requested | ToolCallPhase::Executing { .. } => {}
        }

        let capability = recovery_capability(runtime, context, env, &call);
        if let Err(error) = durable.recovery_policy.validate(capability) {
            batch
                .mark_indeterminate(&call.call_id, error.to_string())
                .map_err(|error| Error::Execution(error.to_string()))?;
            let output = batch.output(&call.call_id).expect("now terminal");
            batch
                .set_result_messages(&call.call_id, vec![tool_result_message(&call, &output)])
                .map_err(|error| Error::Execution(error.to_string()))?;
            if let Some(disposition) =
                abandon_delegation_on_indeterminate(runtime, run_id, &call, &batch, ledger, store)
            {
                return Ok(Some(disposition));
            }
            persist_batch(&batch, ledger, store, context, thread_id, run_id).await?;
            continue;
        }

        // A Requested call must still pass the gate. An Executing call already
        // committed its allow decision with the transition and goes directly to
        // capability-governed recovery.
        let was_requested = matches!(&durable.phase, ToolCallPhase::Requested);
        if was_requested && call.tool_id != awaken_runtime_contract::resolved::TOOL_OPEN_ID {
            let gate = gate_decision(runtime, &call, env, store).await;
            if runtime.gate().is_some() {
                ledger.audit.push(permission_audit(&call, &gate));
            }
            match gate {
                GateOutcome::Block { reason } => {
                    record_recovered_output(
                        env,
                        run_id,
                        &call,
                        ToolOutput::error(&call.call_id, format!("blocked: {reason}")),
                        false,
                        &mut batch,
                        ledger,
                        store,
                        context,
                        thread_id,
                    )
                    .await?;
                    continue;
                }
                GateOutcome::SetResult(output) => {
                    record_recovered_output(
                        env, run_id, &call, output, false, &mut batch, ledger, store, context,
                        thread_id,
                    )
                    .await?;
                    continue;
                }
                GateOutcome::Suspend { ticket_id } => {
                    let ticket = resume_ticket(
                        resolved,
                        run_id,
                        initiator,
                        &ticket_id,
                        &call,
                        AwaitReason::ToolPermission,
                        None,
                    );
                    batch
                        .mark_awaiting(
                            &call.call_id,
                            ToolWaitKind::Approval,
                            ticket.correlation_id.clone(),
                        )
                        .map_err(|error| Error::Execution(error.to_string()))?;
                    stage_batch(&batch, ledger, store);
                    return Ok(Some(RunDisposition::awaiting(ticket)));
                }
                GateOutcome::Schedule {
                    correlation_id,
                    action_kind,
                } => {
                    if let Some(kind) = &action_kind
                        && !env.permits_action_kind(kind)
                    {
                        return Ok(Some(RunDisposition::ended(
                            run_id.clone(),
                            EndCause::Error(Failure::CapabilityBound),
                        )));
                    }
                    let ticket = resume_ticket(
                        resolved,
                        run_id,
                        initiator,
                        &correlation_id,
                        &call,
                        AwaitReason::ScheduledAction,
                        None,
                    );
                    batch
                        .mark_awaiting(
                            &call.call_id,
                            ToolWaitKind::ScheduledAction,
                            ticket.correlation_id.clone(),
                        )
                        .map_err(|error| Error::Execution(error.to_string()))?;
                    stage_batch(&batch, ledger, store);
                    return Ok(Some(RunDisposition::awaiting(ticket)));
                }
                GateOutcome::Allow => {}
            }
        }

        let delegation_started = stage_delegation_request(
            runtime,
            resolved,
            initiator,
            run_id,
            &call,
            store,
            &mut ledger.staged_state,
        )?;
        match batch.mark_executing(&call.call_id) {
            Ok(_) => {}
            Err(awaken_runtime_contract::tool_batch::ToolBatchError::AttemptsExhausted) => {
                batch
                    .mark_indeterminate(&call.call_id, "recovery attempt budget exhausted")
                    .map_err(|error| Error::Execution(error.to_string()))?;
                let output = batch.output(&call.call_id).expect("now terminal");
                batch
                    .set_result_messages(&call.call_id, vec![tool_result_message(&call, &output)])
                    .map_err(|error| Error::Execution(error.to_string()))?;
                if let Some(disposition) = abandon_delegation_on_indeterminate(
                    runtime, run_id, &call, &batch, ledger, store,
                ) {
                    return Ok(Some(disposition));
                }
                persist_batch(&batch, ledger, store, context, thread_id, run_id).await?;
                continue;
            }
            Err(error) => return Err(Error::Execution(error.to_string())),
        }
        persist_batch(&batch, ledger, store, context, thread_id, run_id).await?;

        let output = if call.tool_id == awaken_runtime_contract::resolved::TOOL_OPEN_ID {
            open_deferred_tool(&resolved.spec.tool_presentation, &call, opened)
        } else {
            match run_delegation(
                runtime,
                context,
                initiator,
                &resolved.agent_id.0,
                run_id,
                &call,
            )
            .await
            {
                Some(Ok(DelegationStep::Ended { text, usage })) => {
                    merge_thread_usage(store, &mut ledger.staged_state, &usage);
                    ToolOutput::ok(&call.call_id, text)
                }
                Some(Ok(DelegationStep::Awaiting { continuation })) => {
                    let ticket = resume_ticket(
                        resolved,
                        run_id,
                        initiator,
                        &call.call_id,
                        &call,
                        AwaitReason::Delegation,
                        Some(continuation),
                    );
                    batch
                        .mark_awaiting(
                            &call.call_id,
                            ToolWaitKind::Delegation,
                            ticket.correlation_id.clone(),
                        )
                        .map_err(|error| Error::Execution(error.to_string()))?;
                    stage_batch(&batch, ledger, store);
                    return Ok(Some(RunDisposition::awaiting(ticket)));
                }
                Some(Err(error)) => ToolOutput::error(&call.call_id, error.to_string()),
                None => execute_tool(runtime, Some(env), &call, context).await,
            }
        };
        if delegation_started {
            stage_delegation_completed(runtime, run_id, &call, store, &mut ledger.staged_state)?;
        }
        record_recovered_output(
            env, run_id, &call, output, true, &mut batch, ledger, store, context, thread_id,
        )
        .await?;
    }

    batch
        .finalize()
        .map_err(|error| Error::Execution(error.to_string()))?;
    for entry in &batch.calls {
        ledger.staged_state.extend(entry.result_state.clone());
        for message in &entry.result_messages {
            ledger.push_message(message.clone());
        }
    }
    persist_batch(&batch, ledger, store, context, thread_id, run_id).await?;
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
async fn record_recovered_output(
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    call: &ToolCall,
    output: ToolOutput,
    entered_executor: bool,
    batch: &mut ToolBatch,
    ledger: &mut StepLedger,
    store: &mut Store,
    context: &RuntimeRunContext,
    thread_id: &ThreadId,
) -> Result<()> {
    if entered_executor {
        batch
            .complete(output.clone())
            .map_err(|error| Error::Execution(error.to_string()))?;
    } else {
        batch
            .complete_immediate(output.clone())
            .map_err(|error| Error::Execution(error.to_string()))?;
    }
    for command in &output.state {
        store.apply(command);
    }
    let (reactions, reminders) = collect_tool_reactions(env, run_id, 0, call, &output, store).await;
    for command in &reactions {
        store.apply(command);
    }
    let mut result_state = output.state.clone();
    result_state.extend(reactions);
    let mut messages = vec![tool_result_message(call, &output)];
    messages.extend(reminders);
    batch
        .set_result_messages(&call.call_id, messages)
        .map_err(|error| Error::Execution(error.to_string()))?;
    batch
        .set_result_state(&call.call_id, result_state)
        .map_err(|error| Error::Execution(error.to_string()))?;
    persist_batch(batch, ledger, store, context, thread_id, run_id).await
}

fn recovery_capability(
    runtime: &Runtime,
    context: &RuntimeRunContext,
    env: &ResolvedExecutionEnv,
    call: &ToolCall,
) -> ToolRecoveryCapability {
    if runtime
        .delegation_executor()
        .is_some_and(|executor| executor.tool_id() == call.tool_id)
    {
        return ToolRecoveryCapability::DurableRequest;
    }
    if call.tool_id == awaken_runtime_contract::resolved::TOOL_OPEN_ID {
        return ToolRecoveryCapability::ReplaySafe;
    }
    if let Some(executor) = context.tool_executor.as_deref() {
        return executor.recovery_capability(&call.tool_id);
    }
    env.dynamic_tool(&call.tool_id)
        .or_else(|| runtime.tool(&call.tool_id).cloned())
        .map_or(ToolRecoveryCapability::NonRecoverable, |tool| {
            tool.recovery_capability()
        })
}

fn stage_batch(batch: &ToolBatch, ledger: &mut StepLedger, store: &mut Store) {
    let command = ActiveToolBatch::write(&Some(batch.clone()));
    store.apply(&command);
    ledger.staged_state.push(command);
}

/// A delegation whose durable request can no longer be recovered must end its
/// parent attempt in the same commit that seals the ToolBatch and records child
/// cancellation intent. Continuing the parent with an indeterminate tool result
/// would leave an `Open` relationship that can neither deliver nor release its
/// parallel slot.
fn abandon_delegation_on_indeterminate(
    runtime: &Runtime,
    run_id: &RunId,
    call: &ToolCall,
    batch: &ToolBatch,
    ledger: &mut StepLedger,
    store: &mut Store,
) -> Option<RunDisposition> {
    runtime
        .delegation_executor()
        .is_some_and(|executor| executor.tool_id() == call.tool_id)
        .then(|| {
            stage_batch(batch, ledger, store);
            RunDisposition::ended(run_id.clone(), EndCause::Indeterminate)
        })
}

async fn persist_batch(
    batch: &ToolBatch,
    ledger: &mut StepLedger,
    store: &mut Store,
    context: &RuntimeRunContext,
    thread_id: &ThreadId,
    run_id: &RunId,
) -> Result<()> {
    stage_batch(batch, ledger, store);
    if context.commit.is_some() {
        if validate_batch(&ledger.staged_state).is_err() {
            ledger.rollback_state();
            return Err(Error::StateConflict);
        }
        ledger.commit_delta(context, thread_id, run_id).await?;
    }
    Ok(())
}
