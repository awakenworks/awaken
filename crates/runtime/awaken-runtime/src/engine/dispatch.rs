//! The per-step tool-dispatch branch of the execution loop.
//!
//! Split out of `engine/mod.rs` (the step-loop driver) so the "run this step's
//! tool calls" responsibility is isolated from "drive the loop", and the module
//! stays within the file-length limit. Items are shared through `use super::*`
//! (same-crate privates included), exactly like `inference`.

use super::delegation::is_resolved_advisor_call;
use super::*;

fn call_concurrency(
    runtime: &Runtime,
    context: &RuntimeRunContext,
    delegation_origin: Option<&DelegationOrigin>,
    resolved: &ResolvedRun,
    env: &ResolvedExecutionEnv,
    call: &ToolCall,
) -> ToolConcurrency {
    let intrinsic = if is_resolved_delegation_call(runtime, resolved, delegation_origin, call) {
        runtime
            .run_delegation()
            .map_or(ToolConcurrency::Serial, |executor| {
                if executor.supports_parallel_completion_for(&call.tool_id, &call.arguments) {
                    ToolConcurrency::Parallel
                } else {
                    ToolConcurrency::Serial
                }
            })
    } else if RuntimeToolOperation::classify(&call.tool_id).is_some()
        || is_resolved_advisor_call(resolved, call)
        || resolved.spec.tool_descriptors.iter().any(|descriptor| {
            descriptor.id == call.tool_id && descriptor.kind == ToolKind::ClientExecuted
        })
    {
        ToolConcurrency::Serial
    } else {
        tool_execution::tool_concurrency(runtime, env, context, call)
    };
    env.tool_constraints()
        .iter()
        .fold(intrinsic, |claim, constraint| {
            claim.narrowed_with(constraint.constrain(call))
        })
}

fn requires_sequential_dispatch(resolved: &ResolvedRun, call: &ToolCall) -> bool {
    RuntimeToolOperation::classify(&call.tool_id) == Some(RuntimeToolOperation::ToolSearch)
        || is_resolved_advisor_call(resolved, call)
        || resolved.spec.tool_descriptors.iter().any(|descriptor| {
            descriptor.id == call.tool_id && descriptor.kind == ToolKind::ClientExecuted
        })
}

fn is_detached_only_call(descriptors: &[ToolDescriptor], call: &ToolCall) -> bool {
    descriptors.iter().any(|descriptor| {
        (descriptor.id == call.tool_id && descriptor.kind == ToolKind::DetachedOnly)
            || descriptor.detached_targets.contains(&call.tool_id)
    })
}

fn reject_direct_detached_call(
    descriptors: &[ToolDescriptor],
    call: &ToolCall,
) -> Option<ToolOutput> {
    is_detached_only_call(descriptors, call).then(|| {
        ToolOutput::error(
            &call.call_id,
            format!("tool `{}` is not directly callable", call.tool_id),
        )
    })
}

/// Run each requested tool call for one step, feeding each result back into the
/// transcript and folding tool-outcome reactions into state and reminders. Returns
/// `Some(RunDisposition)` when a call awaits input (delegation/permission/scheduled) or fails
/// it closed (an unpermitted scheduled-action kind) — the caller ends the step loop
/// with it; `None` when every call produced a result and the loop continues. The
/// reserved `tool_search` meta-tool bypasses the gate and only mutates durable discovery state.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_tool_calls(
    runtime: &Runtime,
    context: &RuntimeRunContext,
    delegation_origin: Option<&DelegationOrigin>,
    resolved: &ResolvedRun,
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    thread_id: &ThreadId,
    step: usize,
    calls: Vec<ToolCall>,
    ledger: &mut StepLedger,
    store: &mut Store,
    discovery: &mut ToolDiscoveryState,
) -> Result<Option<RunDisposition>> {
    let dynamic_descriptors = env.dynamic_descriptors();
    let executable_descriptors = executable_tool_descriptors(&resolved.spec, &dynamic_descriptors);
    let policies = calls.iter().cloned().map(|call| {
        let policy = if is_resolved_delegation_call(runtime, resolved, delegation_origin, &call) {
            // A delegated child is addressed by its stable RunId. Recovery
            // reconnects/re-dispatches that durable request; treating it as a
            // generic non-replayable side effect would strand an Open child
            // relationship after an owner crash.
            ToolRecoveryPolicy::durable_request()
        } else if RuntimeToolOperation::classify(&call.tool_id)
            == Some(RuntimeToolOperation::ToolSearch)
        {
            ToolRecoveryPolicy::replay_safe()
        } else {
            executable_descriptors
                .iter()
                .find(|descriptor| descriptor.id == call.tool_id)
                .map(|descriptor| descriptor.recovery_policy.clone())
                .unwrap_or_default()
        };
        (call, policy)
    });
    let mut batch = ToolBatch::for_step(run_id.clone(), step, policies)
        .map_err(|error| Error::Execution(error.to_string()))?;

    // Commit the assistant tool-use blocks and the complete Requested batch before
    // entering any executor. Recovery can now distinguish "never entered" from an
    // unknown in-flight external effect.
    persist_batch(&batch, ledger, store, context, thread_id, run_id).await?;

    let claims = calls
        .iter()
        .map(|call| call_concurrency(runtime, context, delegation_origin, resolved, env, call))
        .collect::<Vec<_>>();
    let waves =
        awaken_runtime_contract::tool_execution_waves(&claims, context.max_parallel_tools());
    let has_parallel_wave = waves.iter().any(|wave| wave.len() > 1);
    let supports_concurrent_path = calls.iter().all(|call| {
        !requires_sequential_dispatch(resolved, call)
            && !is_detached_only_call(&executable_descriptors, call)
    });
    if has_parallel_wave && supports_concurrent_path {
        return run_concurrent_tool_calls(
            runtime,
            context,
            delegation_origin,
            resolved,
            env,
            run_id,
            thread_id,
            step,
            calls,
            waves,
            batch,
            ledger,
            store,
        )
        .await;
    }

    for call in calls {
        // The reserved `tool_search` meta-tool (ADR-0053) discovers deferred tools for
        // later steps: its ToolOutput carries the Run-scoped state command committed
        // atomically with the result, so it
        // bypasses the gate and never reaches an executor.
        let mut entered_executor = false;
        let mut delegation_started = false;
        if resolved.spec.tool_descriptors.iter().any(|descriptor| {
            descriptor.id == call.tool_id && descriptor.kind == ToolKind::ClientExecuted
        }) {
            let ticket = resume_ticket(
                context,
                resolved,
                run_id,
                delegation_origin,
                &call.call_id,
                &call,
                ToolAwaitReason::ClientExecution,
            );
            emit(
                context,
                run_id,
                AgentEvent::Fact(Fact::Awaiting {
                    pending_tool_use_id: ticket.call_id().map(str::to_owned),
                }),
            )
            .await;
            batch
                .mark_awaiting(
                    &call.call_id,
                    ToolWaitKind::ExternalResult,
                    ticket.correlation_id.clone(),
                )
                .map_err(|error| Error::Execution(error.to_string()))?;
            stage_batch(&batch, ledger, store);
            return Ok(Some(RunDisposition::awaiting(ticket)));
        }
        let reaction_call = call.clone();
        let output = if let Some(output) =
            reject_direct_detached_call(&executable_descriptors, &call)
        {
            output
        } else if RuntimeToolOperation::classify(&call.tool_id)
            == Some(RuntimeToolOperation::ToolSearch)
        {
            execute_tool_search(
                &resolved.spec.tool_presentation,
                &executable_descriptors,
                &call,
                discovery,
            )
        } else {
            let outcome = gate_decision(runtime, &call, env, store, context).await;
            // Audit the decision of a real (policy-backed) gate (ADR-0030).
            if runtime.gate().is_some() || context.tool_permission_policy.is_some() {
                ledger.audit.push(permission_audit(&call, &outcome));
            }
            match outcome {
                GateOutcome::Allow => {
                    delegation_started = stage_delegation_request(
                        runtime,
                        resolved,
                        delegation_origin,
                        run_id,
                        &call,
                        store,
                        &mut ledger.staged_state,
                    )?;
                    let capability = recovery_capability(
                        runtime,
                        context,
                        delegation_origin,
                        resolved,
                        env,
                        &call,
                    );
                    let policy = batch
                        .calls()
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
                            resolved,
                            DelegationParent {
                                context,
                                origin: delegation_origin,
                                agent_id: &resolved.agent_id.0,
                                run_id,
                                thread_id,
                            },
                            &call,
                            store,
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
                            // The child needs input: await the parent on a Delegation ticket;
                            // its opaque execution reference stays in the relationship and
                            // is resumed through the delegation executor.
                            Some(Ok(DelegationStep::Awaiting { continuation })) => {
                                stage_delegation_awaiting(
                                    runtime,
                                    run_id,
                                    &call,
                                    &continuation,
                                    store,
                                    &mut ledger.staged_state,
                                )?;
                                let ticket = resume_ticket(
                                    context,
                                    resolved,
                                    run_id,
                                    delegation_origin,
                                    &call.call_id,
                                    &call,
                                    ToolAwaitReason::Delegation,
                                );
                                emit(
                                    context,
                                    run_id,
                                    AgentEvent::Fact(Fact::Awaiting {
                                        pending_tool_use_id: ticket.call_id().map(str::to_owned),
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
                            Some(Err(error)) => delegation_error_output(resolved, &call, error)?,
                            None => {
                                execute_regular_or_unavailable_advisor(
                                    runtime, resolved, env, &call, context, run_id, thread_id,
                                    &batch, store,
                                )
                                .await?
                            }
                        }
                    }
                }
                GateOutcome::Block { reason } => {
                    ToolOutput::error(&call.call_id, format!("blocked: {reason}"))
                }
                GateOutcome::SetResult(output) => output,
                GateOutcome::RequireConfirmation { correlation_id } => {
                    // Await the run on a structured ticket carrying the pending
                    // call so an allow decision can run it later.
                    let ticket = resume_ticket(
                        context,
                        resolved,
                        run_id,
                        delegation_origin,
                        &correlation_id,
                        &call,
                        ToolAwaitReason::Permission,
                    );
                    emit(
                        context,
                        run_id,
                        AgentEvent::Fact(Fact::Awaiting {
                            pending_tool_use_id: ticket.call_id().map(str::to_owned),
                        }),
                    )
                    .await;
                    batch
                        .mark_awaiting(
                            &call.call_id,
                            ToolWaitKind::ToolPermission,
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
                        context,
                        resolved,
                        run_id,
                        delegation_origin,
                        &correlation_id,
                        &call,
                        ToolAwaitReason::ScheduledAction,
                    );
                    emit(
                        context,
                        run_id,
                        AgentEvent::Fact(Fact::Awaiting {
                            pending_tool_use_id: ticket.call_id().map(str::to_owned),
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

        record_fresh_output(
            runtime,
            env,
            run_id,
            thread_id,
            context,
            step,
            &call,
            &reaction_call,
            output,
            entered_executor,
            delegation_started,
            &mut batch,
            ledger,
            store,
        )
        .await?;
    }

    batch
        .finalize()
        .map_err(|error| Error::Execution(error.to_string()))?;
    for entry in batch.calls() {
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

#[allow(clippy::too_many_arguments)]
async fn run_concurrent_tool_calls(
    runtime: &Runtime,
    context: &RuntimeRunContext,
    delegation_origin: Option<&DelegationOrigin>,
    resolved: &ResolvedRun,
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    thread_id: &ThreadId,
    step: usize,
    calls: Vec<ToolCall>,
    waves: Vec<ToolExecutionWave>,
    mut batch: ToolBatch,
    ledger: &mut StepLedger,
    store: &mut Store,
) -> Result<Option<RunDisposition>> {
    for wave in waves {
        let wave_calls = &calls[wave.range()];
        let mut immediate_outputs = (0..wave_calls.len())
            .map(|_| None)
            .collect::<Vec<Option<ToolOutput>>>();
        let mut delegation_started = vec![false; wave_calls.len()];
        let mut entered_executor = vec![false; wave_calls.len()];

        let mut gates = Vec::with_capacity(wave_calls.len());
        for call in wave_calls {
            let outcome = gate_decision(runtime, call, env, store, context).await;
            if runtime.gate().is_some() || context.tool_permission_policy.is_some() {
                ledger.audit.push(permission_audit(call, &outcome));
            }
            gates.push(outcome);
        }

        // A wait applies before any executor in this wave starts. Earlier waves
        // are already durably complete; the remaining Requested calls stay in
        // the open batch and recovery resumes them after this exact ticket.
        for (call, gate) in wave_calls.iter().zip(&gates) {
            match gate {
                GateOutcome::RequireConfirmation { correlation_id } => {
                    let ticket = resume_ticket(
                        context,
                        resolved,
                        run_id,
                        delegation_origin,
                        correlation_id,
                        call,
                        ToolAwaitReason::Permission,
                    );
                    emit(
                        context,
                        run_id,
                        AgentEvent::Fact(Fact::Awaiting {
                            pending_tool_use_id: ticket.call_id().map(str::to_owned),
                        }),
                    )
                    .await;
                    batch
                        .mark_awaiting(
                            &call.call_id,
                            ToolWaitKind::ToolPermission,
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
                    if let Some(kind) = action_kind
                        && !env.permits_action_kind(kind)
                    {
                        return Ok(Some(RunDisposition::ended(
                            run_id.clone(),
                            EndCause::Error(Failure::CapabilityBound),
                        )));
                    }
                    let ticket = resume_ticket(
                        context,
                        resolved,
                        run_id,
                        delegation_origin,
                        correlation_id,
                        call,
                        ToolAwaitReason::ScheduledAction,
                    );
                    emit(
                        context,
                        run_id,
                        AgentEvent::Fact(Fact::Awaiting {
                            pending_tool_use_id: ticket.call_id().map(str::to_owned),
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
                GateOutcome::Allow | GateOutcome::Block { .. } | GateOutcome::SetResult(_) => {}
            }
        }

        for (position, (call, gate)) in wave_calls.iter().zip(gates).enumerate() {
            match gate {
                GateOutcome::Block { reason } => {
                    immediate_outputs[position] = Some(ToolOutput::error(
                        &call.call_id,
                        format!("blocked: {reason}"),
                    ));
                    continue;
                }
                GateOutcome::SetResult(output) => {
                    immediate_outputs[position] = Some(output);
                    continue;
                }
                GateOutcome::Allow => {}
                GateOutcome::RequireConfirmation { .. } | GateOutcome::Schedule { .. } => {
                    unreachable!("wait gates return before executor entry")
                }
            }
            let capability =
                recovery_capability(runtime, context, delegation_origin, resolved, env, call);
            let policy = &batch
                .calls()
                .iter()
                .find(|entry| entry.call.call_id == call.call_id)
                .expect("batch was built from calls")
                .recovery_policy;
            if let Err(error) = policy.validate(capability) {
                immediate_outputs[position] =
                    Some(ToolOutput::error(&call.call_id, error.to_string()));
                continue;
            }
            delegation_started[position] = stage_delegation_request(
                runtime,
                resolved,
                delegation_origin,
                run_id,
                call,
                store,
                &mut ledger.staged_state,
            )?;
            batch
                .mark_executing(&call.call_id)
                .map_err(|error| Error::Execution(error.to_string()))?;
            entered_executor[position] = true;
        }

        // Persist only this wave's executor-entry facts. Later conflicting waves
        // remain Requested, so recovery never mistakes an unstarted call for an
        // unknown in-flight side effect.
        if entered_executor.iter().any(|entered| *entered) {
            persist_batch(&batch, ledger, store, context, thread_id, run_id).await?;
        }
        let committed_view: &Store = store;
        let parent = DelegationParent {
            context,
            origin: delegation_origin,
            agent_id: &resolved.agent_id.0,
            run_id,
            thread_id,
        };
        let executing_batch = &batch;
        let invocations = futures_util::future::join_all(
            wave_calls
                .iter()
                .zip(&entered_executor)
                .filter(|(_, entered)| **entered)
                .map(|(call, _)| async move {
                    if is_resolved_delegation_call(runtime, resolved, delegation_origin, call) {
                        ConcurrentInvocation::Delegation(
                            invoke_delegation(runtime, resolved, parent, call, committed_view)
                                .await,
                        )
                    } else {
                        ConcurrentInvocation::Regular(
                            execute_regular_or_unavailable_advisor(
                                runtime,
                                resolved,
                                env,
                                call,
                                context,
                                run_id,
                                thread_id,
                                executing_batch,
                                committed_view,
                            )
                            .await,
                        )
                    }
                }),
        )
        .await;
        let mut invocations = invocations.into_iter();

        // Consume results in model order. Hooks and state application therefore
        // remain deterministic even when executor completion order differs.
        for (position, call) in wave_calls.iter().enumerate() {
            let output = if let Some(output) = immediate_outputs[position].take() {
                output
            } else {
                match invocations
                    .next()
                    .expect("every entered call produced one invocation result")
                {
                    ConcurrentInvocation::Regular(result) => result?,
                    ConcurrentInvocation::Delegation(Some(Ok(DelegationInvocation {
                        id,
                        step: DelegationStep::Ended { text, usage },
                    }))) => {
                        let result = persist_child_run_result(
                            context,
                            thread_id,
                            RunDisposition::running(run_id.clone()),
                            &id,
                            store,
                            ChildRunResult {
                                child_run_id: id.child_run_id(),
                                text,
                                usage,
                            },
                        )
                        .await
                        .map_err(|error| Error::Execution(error.to_string()))?;
                        merge_thread_usage(store, &mut ledger.staged_state, &result.usage);
                        ToolOutput::ok(&call.call_id, result.text)
                    }
                    ConcurrentInvocation::Delegation(Some(Ok(DelegationInvocation {
                        step: DelegationStep::Awaiting { continuation },
                        ..
                    }))) => {
                        // Terminal-only capability was overstated. The child is
                        // addressable, but several waits cannot be represented by
                        // one parent ticket; end fail closed.
                        stage_delegation_awaiting(
                            runtime,
                            run_id,
                            call,
                            &continuation,
                            store,
                            &mut ledger.staged_state,
                        )?;
                        stage_batch(&batch, ledger, store);
                        return Ok(Some(RunDisposition::ended(
                            run_id.clone(),
                            EndCause::Indeterminate,
                        )));
                    }
                    ConcurrentInvocation::Delegation(Some(Err(error))) => {
                        delegation_error_output(resolved, call, error)?
                    }
                    ConcurrentInvocation::Delegation(None) => {
                        return Err(Error::Execution(
                            "concurrent delegation call was not handled by its executor"
                                .to_string(),
                        ));
                    }
                }
            };
            record_fresh_output(
                runtime,
                env,
                run_id,
                thread_id,
                context,
                step,
                call,
                call,
                output,
                entered_executor[position],
                delegation_started[position],
                &mut batch,
                ledger,
                store,
            )
            .await?;
        }
    }

    batch
        .finalize()
        .map_err(|error| Error::Execution(error.to_string()))?;
    for entry in batch.calls() {
        ledger.staged_state.extend(entry.result_state.clone());
        for message in &entry.result_messages {
            ledger.push_message(message.clone());
        }
    }
    persist_batch(&batch, ledger, store, context, thread_id, run_id).await?;
    Ok(None)
}

enum ConcurrentInvocation {
    Regular(Result<ToolOutput>),
    Delegation(Option<std::result::Result<DelegationInvocation, DelegationExecutionError>>),
}

#[allow(clippy::too_many_arguments)]
async fn record_fresh_output(
    runtime: &Runtime,
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    thread_id: &ThreadId,
    context: &RuntimeRunContext,
    step: usize,
    call: &ToolCall,
    reaction_call: &ToolCall,
    output: ToolOutput,
    entered_executor: bool,
    delegation_started: bool,
    batch: &mut ToolBatch,
    ledger: &mut StepLedger,
    store: &mut Store,
) -> Result<()> {
    let output = spill_tool_output(context, run_id, output).await?;
    if delegation_started {
        stage_delegation_completed(runtime, run_id, call, store, &mut ledger.staged_state)?;
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
    for command in &output.state {
        store.apply(command);
    }
    let (reactions, reminders) =
        collect_tool_reactions(env, run_id, step, reaction_call, &output, store).await;
    for command in &reactions {
        store.apply(command);
    }
    let mut result_state = output.state.clone();
    result_state.extend(reactions);
    let mut result_messages = vec![tool_result_message(call, &output)];
    result_messages.extend(reminders);
    batch
        .set_result_messages(&call.call_id, result_messages)
        .map_err(|error| Error::Execution(error.to_string()))?;
    batch
        .set_result_state(&call.call_id, result_state)
        .map_err(|error| Error::Execution(error.to_string()))?;
    if !batch.is_complete() {
        persist_batch(batch, ledger, store, context, thread_id, run_id).await?;
    }
    Ok(())
}

/// Recover an open batch committed by a prior owner. Requested calls have not
/// entered an executor and may start normally. Executing calls follow their
/// pinned policy; terminal calls are only published, never invoked again.
#[allow(clippy::too_many_arguments)]
pub(super) async fn recover_tool_batch(
    runtime: &Runtime,
    context: &RuntimeRunContext,
    delegation_origin: Option<&DelegationOrigin>,
    resolved: &ResolvedRun,
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    thread_id: &ThreadId,
    mut batch: ToolBatch,
    ledger: &mut StepLedger,
    store: &mut Store,
    discovery: &mut ToolDiscoveryState,
) -> Result<Option<RunDisposition>> {
    let dynamic_descriptors = env.dynamic_descriptors();
    let executable_descriptors = executable_tool_descriptors(&resolved.spec, &dynamic_descriptors);
    // Rehydrate terminal per-call effects into the attempt's live state. They
    // remain unpublished StateCommands until Finalized, but later gates/hooks in
    // this recovery must observe the same state as the original attempt.
    for entry in batch.calls() {
        if entry.phase.is_terminal() {
            for command in &entry.result_state {
                store.apply(command);
            }
        }
    }
    *discovery =
        ToolDiscoveryStateKey::load(store).map_err(|error| Error::Execution(error.to_string()))?;
    for index in 0..batch.calls().len() {
        let durable = batch.calls()[index].clone();
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
                    runtime,
                    resolved,
                    delegation_origin,
                    &call,
                    &batch,
                    ledger,
                    store,
                ) {
                    return Ok(Some(disposition));
                }
                persist_batch(&batch, ledger, store, context, thread_id, run_id).await?;
                continue;
            }
            ToolCallPhase::Executing { .. }
                if durable.recovery_policy.mode() == ToolRecoveryMode::NeverReplay =>
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
                    runtime,
                    resolved,
                    delegation_origin,
                    &call,
                    &batch,
                    ledger,
                    store,
                ) {
                    return Ok(Some(disposition));
                }
                persist_batch(&batch, ledger, store, context, thread_id, run_id).await?;
                continue;
            }
            ToolCallPhase::Requested | ToolCallPhase::Executing { .. } => {}
        }

        let capability =
            recovery_capability(runtime, context, delegation_origin, resolved, env, &call);
        if let Err(error) = durable.recovery_policy.validate(capability) {
            batch
                .mark_indeterminate(&call.call_id, error.to_string())
                .map_err(|error| Error::Execution(error.to_string()))?;
            let output = batch.output(&call.call_id).expect("now terminal");
            batch
                .set_result_messages(&call.call_id, vec![tool_result_message(&call, &output)])
                .map_err(|error| Error::Execution(error.to_string()))?;
            if let Some(disposition) = abandon_delegation_on_indeterminate(
                runtime,
                resolved,
                delegation_origin,
                &call,
                &batch,
                ledger,
                store,
            ) {
                return Ok(Some(disposition));
            }
            persist_batch(&batch, ledger, store, context, thread_id, run_id).await?;
            continue;
        }

        // A Requested call must still pass the gate. An Executing call already
        // committed its allow decision with the transition and goes directly to
        // capability-governed recovery.
        let was_requested = matches!(&durable.phase, ToolCallPhase::Requested);
        if was_requested
            && RuntimeToolOperation::classify(&call.tool_id)
                != Some(RuntimeToolOperation::ToolSearch)
        {
            let gate = gate_decision(runtime, &call, env, store, context).await;
            if runtime.gate().is_some() || context.tool_permission_policy.is_some() {
                ledger.audit.push(permission_audit(&call, &gate));
            }
            match gate {
                GateOutcome::Block { reason } => {
                    record_recovered_output(
                        env,
                        run_id,
                        &call,
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
                        env, run_id, &call, &call, output, false, &mut batch, ledger, store,
                        context, thread_id,
                    )
                    .await?;
                    continue;
                }
                GateOutcome::RequireConfirmation { correlation_id } => {
                    let ticket = resume_ticket(
                        context,
                        resolved,
                        run_id,
                        delegation_origin,
                        &correlation_id,
                        &call,
                        ToolAwaitReason::Permission,
                    );
                    batch
                        .mark_awaiting(
                            &call.call_id,
                            ToolWaitKind::ToolPermission,
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
                        context,
                        resolved,
                        run_id,
                        delegation_origin,
                        &correlation_id,
                        &call,
                        ToolAwaitReason::ScheduledAction,
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
            delegation_origin,
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
                    runtime,
                    resolved,
                    delegation_origin,
                    &call,
                    &batch,
                    ledger,
                    store,
                ) {
                    return Ok(Some(disposition));
                }
                persist_batch(&batch, ledger, store, context, thread_id, run_id).await?;
                continue;
            }
            Err(error) => return Err(Error::Execution(error.to_string())),
        }
        persist_batch(&batch, ledger, store, context, thread_id, run_id).await?;

        let reaction_call = call.clone();
        let output = if let Some(output) =
            reject_direct_detached_call(&executable_descriptors, &call)
        {
            output
        } else if RuntimeToolOperation::classify(&call.tool_id)
            == Some(RuntimeToolOperation::ToolSearch)
        {
            execute_tool_search(
                &resolved.spec.tool_presentation,
                &executable_descriptors,
                &call,
                discovery,
            )
        } else {
            match run_delegation(
                runtime,
                resolved,
                DelegationParent {
                    context,
                    origin: delegation_origin,
                    agent_id: &resolved.agent_id.0,
                    run_id,
                    thread_id,
                },
                &call,
                store,
            )
            .await
            {
                Some(Ok(DelegationStep::Ended { text, usage })) => {
                    merge_thread_usage(store, &mut ledger.staged_state, &usage);
                    ToolOutput::ok(&call.call_id, text)
                }
                Some(Ok(DelegationStep::Awaiting { continuation })) => {
                    stage_delegation_awaiting(
                        runtime,
                        run_id,
                        &call,
                        &continuation,
                        store,
                        &mut ledger.staged_state,
                    )?;
                    let ticket = resume_ticket(
                        context,
                        resolved,
                        run_id,
                        delegation_origin,
                        &call.call_id,
                        &call,
                        ToolAwaitReason::Delegation,
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
                Some(Err(error)) => delegation_error_output(resolved, &call, error)?,
                None => {
                    execute_regular_or_unavailable_advisor(
                        runtime, resolved, env, &call, context, run_id, thread_id, &batch, store,
                    )
                    .await?
                }
            }
        };
        if delegation_started {
            stage_delegation_completed(runtime, run_id, &call, store, &mut ledger.staged_state)?;
        }
        record_recovered_output(
            env,
            run_id,
            &call,
            &reaction_call,
            output,
            true,
            &mut batch,
            ledger,
            store,
            context,
            thread_id,
        )
        .await?;
    }

    batch
        .finalize()
        .map_err(|error| Error::Execution(error.to_string()))?;
    for entry in batch.calls() {
        ledger.staged_state.extend(entry.result_state.clone());
        for message in &entry.result_messages {
            ledger.push_message(message.clone());
        }
    }
    persist_batch(&batch, ledger, store, context, thread_id, run_id).await?;
    Ok(None)
}

/// Execute a regular tool or return the one fail-closed Advisor result when no
/// durable Host delegation service accepted the call. Advisor has no Runtime
/// provider fallback: fresh dispatch and recovery share this exact boundary.
#[allow(clippy::too_many_arguments)]
async fn execute_regular_or_unavailable_advisor(
    runtime: &Runtime,
    resolved: &ResolvedRun,
    env: &ResolvedExecutionEnv,
    call: &ToolCall,
    context: &RuntimeRunContext,
    run_id: &RunId,
    thread_id: &ThreadId,
    batch: &ToolBatch,
    state: &Store,
) -> Result<ToolOutput> {
    if is_resolved_advisor_call(resolved, call) {
        return Ok(ToolOutput::error(
            &call.call_id,
            awaken_runtime_contract::resolved::ADVISOR_UNAVAILABLE_NOTICE,
        ));
    }
    let operation_id = batch.operation_id(&call.call_id);
    execute_tool(
        runtime,
        Some(env),
        Some(resolved),
        call,
        context,
        ToolExecutionOrigin {
            run_id,
            thread_id,
            operation_id,
        },
        state,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn record_recovered_output(
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    call: &ToolCall,
    reaction_call: &ToolCall,
    output: ToolOutput,
    entered_executor: bool,
    batch: &mut ToolBatch,
    ledger: &mut StepLedger,
    store: &mut Store,
    context: &RuntimeRunContext,
    thread_id: &ThreadId,
) -> Result<()> {
    let output = spill_tool_output(context, run_id, output).await?;
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
    let (reactions, reminders) =
        collect_tool_reactions(env, run_id, 0, reaction_call, &output, store).await;
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
    delegation_origin: Option<&DelegationOrigin>,
    resolved: &ResolvedRun,
    env: &ResolvedExecutionEnv,
    call: &ToolCall,
) -> ToolRecoveryCapability {
    if is_resolved_delegation_call(runtime, resolved, delegation_origin, call) {
        return ToolRecoveryCapability::DurableRequest;
    }
    if RuntimeToolOperation::classify(&call.tool_id) == Some(RuntimeToolOperation::ToolSearch) {
        return ToolRecoveryCapability::ReplaySafe;
    }
    if is_resolved_advisor_call(resolved, call) {
        // The missing/primary-only path never enters an external executor and
        // deterministically publishes the same redacted result. Keep the
        // descriptor's durable-request contract valid so a temporarily absent
        // Host capability cannot transform a safe model-visible failure into an
        // indeterminate parent Run.
        return ToolRecoveryCapability::DurableRequest;
    }
    tool_execution::tool_recovery_capability(runtime, env, context, &call.tool_id)
}

#[cfg(test)]
fn routed_recovery_capability(
    target: Option<awaken_runtime_contract::tool::ToolExecutionTarget>,
    tool: ToolRecoveryCapability,
    sandbox_executor: Option<ToolRecoveryCapability>,
) -> ToolRecoveryCapability {
    match target {
        Some(awaken_runtime_contract::tool::ToolExecutionTarget::Brain) => tool,
        Some(awaken_runtime_contract::tool::ToolExecutionTarget::Sandbox) => {
            sandbox_executor.unwrap_or(ToolRecoveryCapability::NonRecoverable)
        }
        None => ToolRecoveryCapability::NonRecoverable,
    }
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
    resolved: &ResolvedRun,
    delegation_origin: Option<&DelegationOrigin>,
    call: &ToolCall,
    batch: &ToolBatch,
    ledger: &mut StepLedger,
    store: &mut Store,
) -> Option<RunDisposition> {
    is_resolved_delegation_call(runtime, resolved, delegation_origin, call).then(|| {
        stage_batch(batch, ledger, store);
        RunDisposition::ended(batch.run_id().clone(), EndCause::Indeterminate)
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

#[cfg(test)]
mod execution_target_recovery_tests {
    use super::*;
    use awaken_runtime_contract::tool::ToolExecutionTarget::{Brain, Sandbox};

    #[test]
    fn recovery_capability_follows_the_execution_target_decision_table() {
        for (case, target, tool, executor, expected) in [
            (
                "unknown",
                None,
                ToolRecoveryCapability::ReplaySafe,
                Some(ToolRecoveryCapability::DurableRequest),
                ToolRecoveryCapability::NonRecoverable,
            ),
            (
                "brain ignores sandbox executor",
                Some(Brain),
                ToolRecoveryCapability::ReplaySafe,
                Some(ToolRecoveryCapability::DurableRequest),
                ToolRecoveryCapability::ReplaySafe,
            ),
            (
                "sandbox uses executor",
                Some(Sandbox),
                ToolRecoveryCapability::ReplaySafe,
                Some(ToolRecoveryCapability::DurableRequest),
                ToolRecoveryCapability::DurableRequest,
            ),
            (
                "sandbox without executor fails closed",
                Some(Sandbox),
                ToolRecoveryCapability::ReplaySafe,
                None,
                ToolRecoveryCapability::NonRecoverable,
            ),
        ] {
            assert_eq!(
                routed_recovery_capability(target, tool, executor),
                expected,
                "{case}"
            );
        }
    }
}
