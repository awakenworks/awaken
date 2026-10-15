// @formal entrypoint delegation.stable_child_crash_recovery
// @formal entrypoint tool.concurrency_resource_conflict
// @formal entrypoint tool.executor_entry_order
// @formal entrypoint tool.publication_barrier
// @formal entrypoint tool.recovery_policy
// @formal entrypoint tool_result.call_is_consumed_at_most_once
// @formal entrypoint tool_schedule.every_wave_is_pairwise_compatible
// @formal entrypoint tool_schedule.order_is_stable_and_conflicting_predecessors_are_never_bypassed
// @formal entrypoint tool_schedule.parallel_bound_is_positive_exact_and_zero_normalizes_to_one
// @formal entrypoint tool_schedule.partition_is_total_contiguous_and_exactly_once
// @formal entrypoint tool_schedule.serial_is_singleton_and_compatible_prefixes_are_maximal
//! The per-step tool-dispatch branch of the execution loop.
//!
//! Split out of `engine/mod.rs` (the step-loop driver) so the "run this step's
//! tool calls" responsibility is isolated from "drive the loop", and the module
//! stays within the file-length limit. Items are shared through `use super::*`
//! (same-crate privates included), exactly like `inference`.

use super::delegation::is_resolved_advisor_call;
use super::*;
use awaken_agent_contract::agent::state::Action as StateAction;

fn is_client_executed(resolved: &ResolvedRun, call: &ToolCall) -> bool {
    resolved.spec.tool_descriptors.iter().any(|descriptor| {
        descriptor.id == call.tool_id && descriptor.kind == ToolKind::ClientExecuted
    })
}

/// One client ownership branch for initial and recovered Requested calls. An external
/// tool never enters the Host permission/execution path merely because its batch resumed.
#[allow(clippy::too_many_arguments)]
async fn await_client_call(
    context: &RuntimeRunContext,
    resolved: &ResolvedRun,
    run_id: &RunId,
    delegation_origin: Option<&DelegationOrigin>,
    call: &ToolCall,
    batch: &mut ToolBatch,
    ledger: &mut StepLedger,
    store: &mut Store,
) -> Result<Option<RunDisposition>> {
    if !is_client_executed(resolved, call) {
        return Ok(None);
    }
    let ticket = resume_ticket(
        context,
        resolved,
        run_id,
        delegation_origin,
        &call.call_id,
        call,
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
    stage_batch(batch, ledger, store);
    Ok(Some(RunDisposition::awaiting(ticket)))
}

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
        || is_client_executed(resolved, call)
    {
        ToolConcurrency::Serial
    } else {
        return tool_execution::tool_concurrency(runtime, env, context, call);
    };
    tool_execution::constrain_tool_concurrency(intrinsic, env.tool_constraints(), call)
}

fn requires_sequential_dispatch(resolved: &ResolvedRun, call: &ToolCall) -> bool {
    RuntimeToolOperation::classify(&call.tool_id) == Some(RuntimeToolOperation::ToolSearch)
        || is_resolved_advisor_call(resolved, call)
        || is_client_executed(resolved, call)
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
    let waves = awaken_tool_contract::tool_execution_waves(&claims, context.max_parallel_tools());
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
        if let Some(disposition) = await_client_call(
            context,
            resolved,
            run_id,
            delegation_origin,
            &call,
            &mut batch,
            ledger,
            store,
        )
        .await?
        {
            return Ok(Some(disposition));
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
            let decision = gate_decision(runtime, &call, env, store, context).await;
            ledger.audit.extend(decision.audit);
            match decision.outcome {
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
                        match super::tool_execution::prepare_tool_domain(
                            runtime, env, context, &call,
                        )
                        .await?
                        {
                            Err(output) => output,
                            Ok(domain) => {
                                batch
                                    .mark_executing_in_domain(&call.call_id, Some(domain))
                                    .map_err(|error| Error::Execution(error.to_string()))?;
                                persist_batch(&batch, ledger, store, context, thread_id, run_id)
                                    .await?;
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
                                                pending_tool_use_id: ticket
                                                    .call_id()
                                                    .map(str::to_owned),
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
                                    Some(Err(error)) => {
                                        delegation_error_output(resolved, &call, error)?
                                    }
                                    None => {
                                        let admission = batch
                                            .execution_admission(&call.call_id)
                                            .map_err(|error| Error::Execution(error.to_string()))?;
                                        execute_regular_or_unavailable_advisor(
                                            runtime, resolved, env, &call, context, run_id,
                                            thread_id, admission, store,
                                        )
                                        .await?
                                    }
                                }
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

        record_tool_output(
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
            BatchPersistence::WhenIncomplete,
            &mut batch,
            ledger,
            store,
        )
        .await?;
    }

    batch
        .finalize()
        .map_err(|error| Error::Execution(error.to_string()))?;
    stage_batch_publication(&batch, ledger, store)?;
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
            let decision = gate_decision(runtime, call, env, store, context).await;
            ledger.audit.extend(decision.audit);
            gates.push(decision.outcome);
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
            let domain =
                match super::tool_execution::prepare_tool_domain(runtime, env, context, call)
                    .await?
                {
                    Ok(domain) => domain,
                    Err(output) => {
                        immediate_outputs[position] = Some(output);
                        continue;
                    }
                };
            batch
                .mark_executing_in_domain(&call.call_id, Some(domain))
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
                        let admission = executing_batch
                            .execution_admission(&call.call_id)
                            .map_err(|error| Error::Execution(error.to_string()));
                        ConcurrentInvocation::Regular(match admission {
                            Ok(admission) => {
                                execute_regular_or_unavailable_advisor(
                                    runtime,
                                    resolved,
                                    env,
                                    call,
                                    context,
                                    run_id,
                                    thread_id,
                                    admission,
                                    committed_view,
                                )
                                .await
                            }
                            Err(error) => Err(error),
                        })
                    }
                }),
        )
        .await;
        let mut invocations = invocations.into_iter();
        let mut deferred_exit: Option<Result<Option<RunDisposition>>> = None;

        // Consume results in model order. Hooks and state application therefore
        // remain deterministic even when executor completion order differs.
        // Every entered invocation already returned. A logical fatal result
        // must not discard later siblings' known outcomes; record them through
        // the same batch owner before returning the first disposition. Recorder
        // persistence/fence failures still abort rather than invent a receipt.
        for (position, call) in wave_calls.iter().enumerate() {
            let output = if let Some(output) = immediate_outputs[position].take() {
                output
            } else {
                match invocations
                    .next()
                    .expect("every entered call produced one invocation result")
                {
                    ConcurrentInvocation::Regular(Ok(output)) => output,
                    ConcurrentInvocation::Regular(Err(error)) => {
                        deferred_exit.get_or_insert(Err(error));
                        continue;
                    }
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
                        deferred_exit.get_or_insert(Ok(Some(RunDisposition::ended(
                            run_id.clone(),
                            EndCause::Indeterminate,
                        ))));
                        continue;
                    }
                    ConcurrentInvocation::Delegation(Some(Err(error))) => {
                        match delegation_error_output(resolved, call, error) {
                            Ok(output) => output,
                            Err(error) => {
                                deferred_exit.get_or_insert(Err(error));
                                continue;
                            }
                        }
                    }
                    ConcurrentInvocation::Delegation(None) => {
                        deferred_exit.get_or_insert(Err(Error::Execution(
                            "concurrent delegation call was not handled by its executor"
                                .to_string(),
                        )));
                        continue;
                    }
                }
            };
            record_tool_output(
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
                BatchPersistence::WhenIncomplete,
                &mut batch,
                ledger,
                store,
            )
            .await?;
        }
        if let Some(exit) = deferred_exit {
            return exit;
        }
    }

    batch
        .finalize()
        .map_err(|error| Error::Execution(error.to_string()))?;
    stage_batch_publication(&batch, ledger, store)?;
    persist_batch(&batch, ledger, store, context, thread_id, run_id).await?;
    Ok(None)
}

enum ConcurrentInvocation {
    Regular(Result<ToolOutput>),
    Delegation(Option<std::result::Result<DelegationInvocation, DelegationExecutionError>>),
}

/// The step index a recovered attempt cannot reconstruct. Recovery resumes one
/// durable batch whose originating step is not part of that batch, and tool
/// reactions are keyed by call, not by step, so the recorded coordinate is
/// stated here once instead of appearing as a bare `0` at three call sites.
const RECOVERED_STEP: usize = 0;

/// When the batch reaches durable storage after one call's terminal outcome.
///
/// This is one of the exactly two coordinates on which fresh dispatch and
/// recovery legitimately differ, so it is a named argument of the one recorder
/// rather than a reason to keep two copies of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BatchPersistence {
    /// Fresh dispatch: the enclosing loop finalizes and persists a complete
    /// batch itself, so persisting here would write the same rows twice.
    WhenIncomplete,
    /// Recovery: every reconstructed outcome is persisted as it is decided, so
    /// a second crash cannot lose the work this attempt already redid.
    Always,
}

/// Record one call's terminal outcome into the open batch: spill the output,
/// complete the call, apply its state, collect reactions and reminders, and
/// persist.
///
/// Fresh dispatch and recovery once had a copy of this body each. They drifted
/// by construction: any reaction, reminder or state rule added to one was
/// silently absent from the other, and the recovered branch is the one no
/// ordinary run exercises. There is now one body, and the two coordinates the
/// paths really differ on — the step index a recovered attempt cannot
/// reconstruct, and when the batch is persisted — are arguments.
#[allow(clippy::too_many_arguments)]
async fn record_tool_output(
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
    persistence: BatchPersistence,
    batch: &mut ToolBatch,
    ledger: &mut StepLedger,
    store: &mut Store,
) -> Result<()> {
    if let Some(spiller) = &context.tool_output_spiller
        && output
            .content
            .iter()
            .all(|block| matches!(block, ContentBlock::Text { .. }))
    {
        verify_attempt_ownership(context.ownership.as_deref()).await?;
        let domain = context
            .physical_quiescence
            .observe_return(spiller.execution_domain(run_id, &output.call_id, &output.text()))
            .await
            .map_err(|error| Error::Execution(error.to_string()))?
            .bind_resource_origin(context.resource_origin.as_ref())
            .map_err(|error| Error::Execution(error.to_string()))?;
        let effectful = !domain.is_worker_contained();
        batch
            .bind_materialization_domain(&output.call_id, domain)
            .map_err(|error| Error::Execution(error.to_string()))?;
        if effectful {
            persist_batch(batch, ledger, store, context, thread_id, run_id).await?;
        }
    }
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
        collect_tool_reactions(env, run_id, step, reaction_call, &output, store, context).await;
    for command in &reactions {
        store.apply(command);
    }
    let mut result_state = output.state.clone();
    result_state.extend(reactions);
    let mut result_messages = vec![tool_result_message(batch, call, &output)?];
    result_messages.extend(reminders);
    batch
        .set_result_messages(&call.call_id, result_messages)
        .map_err(|error| Error::Execution(error.to_string()))?;
    batch
        .set_result_state(&call.call_id, result_state)
        .map_err(|error| Error::Execution(error.to_string()))?;
    if persistence == BatchPersistence::Always || !batch.is_complete() {
        persist_batch(batch, ledger, store, context, thread_id, run_id).await?;
    }
    Ok(())
}

/// Advance the authoritative open batch before either a native inference or an
/// external-loop handoff. Both entries use the same recovery and conflict rules;
/// a finalized/other-Run batch never enters an executor again.
#[allow(clippy::too_many_arguments)]
pub(super) async fn recover_pending_tool_batch(
    runtime: &Runtime,
    context: &RuntimeRunContext,
    delegation_origin: Option<&DelegationOrigin>,
    resolved: &ResolvedRun,
    env: &ResolvedExecutionEnv,
    run_id: &RunId,
    thread_id: &ThreadId,
    ledger: &mut StepLedger,
    store: &mut Store,
    discovery: &mut ToolDiscoveryState,
) -> Result<Option<RunDisposition>> {
    let Some(batch) = ActiveToolBatch::load(store)
        .map_err(|error| Error::Execution(error.to_string()))?
        .filter(|batch| batch.run_id() == run_id && batch.phase() != ToolBatchPhase::Finalized)
    else {
        return Ok(None);
    };
    tool_execution_disposition(
        run_id,
        recover_tool_batch(
            runtime,
            context,
            delegation_origin,
            resolved,
            env,
            run_id,
            thread_id,
            batch,
            ledger,
            store,
            discovery,
        )
        .await,
    )
}

/// Normal and recovered batches consume the same fatal executor signals. These
/// are logical outcomes only; the attempt-local latch separately vetoes ACK.
pub(super) fn tool_execution_disposition(
    run_id: &RunId,
    result: Result<Option<RunDisposition>>,
) -> Result<Option<RunDisposition>> {
    match result {
        Err(Error::StateConflict) => Ok(Some(RunDisposition::ended(
            run_id.clone(),
            EndCause::Error(Failure::StateConflict),
        ))),
        Err(Error::Indeterminate) => Ok(Some(RunDisposition::ended(
            run_id.clone(),
            EndCause::Indeterminate,
        ))),
        outcome => outcome,
    }
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
    // The shared Run reconstruction already restored terminal effects in their
    // original commit order. Reapplying them here would rewind newer deliveries.
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
                if let Some(disposition) = abandon_delegation_on_indeterminate(
                    runtime,
                    resolved,
                    delegation_origin,
                    &call,
                    &mut batch,
                    ledger,
                    store,
                )? {
                    return Ok(Some(disposition));
                }
                persist_batch(&batch, ledger, store, context, thread_id, run_id).await?;
                continue;
            }
            ToolCallPhase::Requested | ToolCallPhase::Executing { .. } => {}
        }

        if matches!(&durable.phase, ToolCallPhase::Requested)
            && let Some(disposition) = await_client_call(
                context,
                resolved,
                run_id,
                delegation_origin,
                &call,
                &mut batch,
                ledger,
                store,
            )
            .await?
        {
            return Ok(Some(disposition));
        }

        let capability =
            recovery_capability(runtime, context, delegation_origin, resolved, env, &call);

        // A Requested call must still pass the gate. An Executing call already
        // committed its allow decision with the transition and goes directly to
        // capability-governed recovery.
        let was_requested = matches!(&durable.phase, ToolCallPhase::Requested);
        if was_requested
            && RuntimeToolOperation::classify(&call.tool_id)
                != Some(RuntimeToolOperation::ToolSearch)
        {
            let gate = gate_decision(runtime, &call, env, store, context).await;
            ledger.audit.extend(gate.audit);
            match gate.outcome {
                GateOutcome::Block { reason } => {
                    record_tool_output(
                        runtime,
                        env,
                        run_id,
                        thread_id,
                        context,
                        RECOVERED_STEP,
                        &call,
                        &call,
                        ToolOutput::error(&call.call_id, format!("blocked: {reason}")),
                        false,
                        false,
                        BatchPersistence::Always,
                        &mut batch,
                        ledger,
                        store,
                    )
                    .await?;
                    continue;
                }
                GateOutcome::SetResult(output) => {
                    record_tool_output(
                        runtime,
                        env,
                        run_id,
                        thread_id,
                        context,
                        RECOVERED_STEP,
                        &call,
                        &call,
                        output,
                        false,
                        false,
                        BatchPersistence::Always,
                        &mut batch,
                        ledger,
                        store,
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

        match batch
            .recover_execution(&call.call_id, capability)
            .map_err(|error| Error::Execution(error.to_string()))?
        {
            ToolRecoveryAction::Entered { entry } => {
                // Scalar admission precedes lazy realization. Consuming this
                // uncommitted borrow installs the one entry; failure/cancel
                // leaves the call unchanged, before executor authority exists.
                super::tool_execution::enter_pending_tool(runtime, env, context, &call, entry)
                    .await?;
            }
            ToolRecoveryAction::Indeterminate => {
                if let Some(disposition) = abandon_delegation_on_indeterminate(
                    runtime,
                    resolved,
                    delegation_origin,
                    &call,
                    &mut batch,
                    ledger,
                    store,
                )? {
                    return Ok(Some(disposition));
                }
                persist_batch(&batch, ledger, store, context, thread_id, run_id).await?;
                continue;
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
                    let admission = batch
                        .execution_admission(&call.call_id)
                        .map_err(|error| Error::Execution(error.to_string()))?;
                    execute_regular_or_unavailable_advisor(
                        runtime, resolved, env, &call, context, run_id, thread_id, admission, store,
                    )
                    .await?
                }
            }
        };
        record_tool_output(
            runtime,
            env,
            run_id,
            thread_id,
            context,
            RECOVERED_STEP,
            &call,
            &reaction_call,
            output,
            true,
            delegation_started,
            BatchPersistence::Always,
            &mut batch,
            ledger,
            store,
        )
        .await?;
    }

    batch
        .finalize()
        .map_err(|error| Error::Execution(error.to_string()))?;
    stage_batch_publication(&batch, ledger, store)?;
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
    admission: ToolExecutionAdmission<'_>,
    state: &Store,
) -> Result<ToolOutput> {
    if is_resolved_advisor_call(resolved, call) {
        return Ok(ToolOutput::error(
            &call.call_id,
            awaken_runtime_contract::resolved::ADVISOR_UNAVAILABLE_NOTICE,
        ));
    }
    debug_assert_eq!(admission.call_id(), call.call_id);
    let operation_id = admission.operation_id();
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
    target: Option<awaken_tool_contract::tool::ToolExecutionTarget>,
    tool: ToolRecoveryCapability,
    sandbox_executor: Option<ToolRecoveryCapability>,
) -> ToolRecoveryCapability {
    match target {
        Some(awaken_tool_contract::tool::ToolExecutionTarget::Brain) => tool,
        Some(awaken_tool_contract::tool::ToolExecutionTarget::Sandbox) => {
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

/// Stage the one ordered publication admitted by the finalized ToolBatch.
/// Fresh sequential, compatible concurrent, and recovery paths share this
/// adapter; the aggregate remains the sole lifecycle and ordering authority.
fn stage_batch_publication(
    batch: &ToolBatch,
    ledger: &mut StepLedger,
    store: &Store,
) -> Result<()> {
    let publication = batch
        .publication()
        .map_err(|error| Error::Execution(error.to_string()))?;
    ledger
        .staged_state
        .extend(publication.result_state().cloned().map(|mut command| {
            // These cells are Runtime bookkeeping, not tool-owned state. A resumed
            // call retains its original snapshot in result_state, but subsequent
            // requests, deliveries and consumption may already have advanced it.
            // Publish the current execution projection at this one adapter; copying
            // the old snapshot would resurrect a relationship or erase later usage.
            if [
                (RunDelegations::SCOPE, RunDelegations::KEY),
                (PendingChildRunResults::SCOPE, PendingChildRunResults::KEY),
                (ThreadUsageKey::SCOPE, ThreadUsageKey::KEY),
            ]
            .contains(&(command.scope, command.key.0.as_str()))
            {
                command.action = store
                    .get(command.scope, &command.key)
                    .cloned()
                    .map(StateAction::Set)
                    .unwrap_or(StateAction::Remove);
            }
            command
        }));
    for message in publication.messages() {
        ledger.push_message(message.clone());
    }
    Ok(())
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
    batch: &mut ToolBatch,
    ledger: &mut StepLedger,
    store: &mut Store,
) -> Result<Option<RunDisposition>> {
    let output = batch.output(&call.call_id).expect("now terminal");
    let result = tool_result_message(batch, call, &output)?;
    batch
        .set_result_messages(&call.call_id, vec![result])
        .map_err(|error| Error::Execution(error.to_string()))?;
    Ok(
        is_resolved_delegation_call(runtime, resolved, delegation_origin, call).then(|| {
            stage_batch(batch, ledger, store);
            RunDisposition::ended(batch.run_id().clone(), EndCause::Indeterminate)
        }),
    )
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
    use awaken_tool_contract::tool::ToolExecutionTarget::{Brain, Sandbox};

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

/// Fresh dispatch and durable recovery are twins that cannot be collapsed into
/// one function: one compiles a batch out of a model's tool calls, the other
/// resumes a batch that already exists durably. Everything they share has been
/// pulled into a kernel they both call, and this registry is what keeps that
/// true — a rule added to one member and not the other, or a third entry point
/// added beside them, is what the twin defects in this file have always been.
///
/// The registry is deliberately *small*: registering a pair is the fallback for
/// a twin that cannot be eliminated, not a place to record ones that can.
#[cfg(test)]
mod twin_registry {
    /// This module reads its own file at compile time, so the registry can
    /// never observe a stale copy of what it guards. Only the part before this
    /// module is the subject: the registry's own prose names the symbols it
    /// guards and must not count as a use of them.
    const WHOLE_FILE: &str = include_str!("dispatch.rs");
    const REGISTRY_MARKER: &str = "mod twin_registry {";

    fn dispatch() -> &'static str {
        let end = WHOLE_FILE
            .find(REGISTRY_MARKER)
            .expect("the registry declares itself in the file it guards");
        &WHOLE_FILE[..end]
    }

    /// The one registered twin pair: (fresh dispatch, durable recovery).
    const TWIN_PAIR: (&str, &str) = ("run_tool_calls", "recover_tool_batch");

    /// Entry points that only select a batch and hand it to a registered
    /// member, mapped to the member they must delegate to.
    const DELEGATORS: &[(&str, &str)] = &[("recover_pending_tool_batch", "recover_tool_batch")];

    /// Every shared kernel both members of the pair must route through. A rule
    /// that belongs to both paths lives in one of these, never inline in one.
    const SHARED_KERNELS: &[&str] = &[
        "executable_tool_descriptors",
        "gate_decision",
        "record_tool_output",
        "persist_batch",
    ];

    /// The signature coordinate that makes a function a tool-dispatch entry
    /// point rather than a helper: it advances the run's tool discovery state.
    const ENTRY_POINT_MARKER: &str = "discovery: &mut ToolDiscoveryState";

    /// Body of one top-level function, which rustfmt guarantees ends at the
    /// first closing brace in column zero.
    fn body(name: &str) -> &'static str {
        function_body(dispatch(), name)
    }

    fn function_body<'a>(source: &'a str, name: &str) -> &'a str {
        let needle = format!("fn {name}(");
        let start = source
            .find(&needle)
            .unwrap_or_else(|| panic!("registered twin `{name}` is not declared in dispatch.rs"));
        let tail = &source[start..];
        let mut offset = 0;
        for line in tail.split_inclusive('\n') {
            if line.trim_end_matches(['\r', '\n']) == "}" {
                return &tail[..offset];
            }
            offset += line.len();
        }
        panic!("a top-level function closes in column zero")
    }

    #[test]
    fn registered_function_boundaries_accept_lf_and_crlf() {
        // R5 both checkout line endings preserve the same function boundary;
        // an indented nested brace cannot hide the later kernel invocation,
        // and a neighboring function cannot supply the missing invocation.
        let source = "fn first() {\n    if true {\n        nested();\n    }\n    kernel();\n}\nfn next() {\n    unrelated();\n}\n";
        for source in [source.to_string(), source.replace('\n', "\r\n")] {
            let first = function_body(&source, "first");
            assert!(first.contains("kernel();"));
            assert!(!first.contains("unrelated();"));
        }
    }

    /// Names of every `pub(super) async fn` in this file that carries the
    /// entry-point marker.
    fn declared_entry_points() -> Vec<&'static str> {
        dispatch()
            .lines()
            .filter_map(|line| line.strip_prefix("pub(super) async fn "))
            .filter_map(|rest| rest.split('(').next())
            .filter(|name| body(name).contains(ENTRY_POINT_MARKER))
            .collect()
    }

    #[test]
    fn both_members_of_the_registered_pair_route_through_every_shared_kernel() {
        // C1 a shared rule exists as a named kernel; C2 a member calls it.
        // E1 admit. R1 = C1 & C2 for both members => E1; a member that grows
        // its own inline copy of a shared rule stops calling the kernel and is
        // refused here.
        let (fresh, recovered) = TWIN_PAIR;
        for kernel in SHARED_KERNELS {
            for member in [fresh, recovered] {
                assert!(
                    body(member).contains(&format!("{kernel}(")),
                    "R1 twin `{member}` no longer routes through the shared kernel `{kernel}`; \
                     either restore the call or move the rule out of the kernel deliberately"
                );
            }
        }
    }

    #[test]
    fn the_tool_output_recorder_has_exactly_one_body() {
        // The eliminated twin. `record_fresh_output`/`record_recovered_output`
        // were line-for-line copies that differed only in the step index and
        // when the batch was persisted; a reaction or reminder rule added to
        // one was silently missing from the recovered path. C1 a function
        // collects tool reactions; E1 it is the one recorder.
        // R2 exactly one caller of `collect_tool_reactions` => E1.
        let callers = dispatch().matches("collect_tool_reactions(").count();
        assert_eq!(
            callers, 1,
            "R2 tool reactions are collected in {callers} places; there is one recorder \
             (`record_tool_output`) and a second copy of it is the twin defect this file had"
        );
        assert!(
            body("record_tool_output").contains("collect_tool_reactions("),
            "R2 the single reaction collector is the registered recorder"
        );
    }

    #[test]
    fn every_tool_dispatch_entry_point_is_registered() {
        // Completeness: a new entry point beside the pair is a new twin, and it
        // cannot be added silently. C1 a `pub(super) async fn` advances tool
        // discovery; C2 it is a registered pair member or a registered
        // delegator. E1 admit, E2 refuse. R3 = C1 & !C2 => E2.
        let (fresh, recovered) = TWIN_PAIR;
        let mut registered = vec![fresh, recovered];
        registered.extend(DELEGATORS.iter().map(|(entry, _)| *entry));
        registered.sort_unstable();
        let mut declared = declared_entry_points();
        declared.sort_unstable();
        assert_eq!(
            declared, registered,
            "R3 the tool-dispatch entry points and the twin registry disagree; register the new \
             entry point in TWIN_PAIR or DELEGATORS (and say which twin it must stay level with), \
             or give it a signature that is not a dispatch entry point"
        );
    }

    #[test]
    fn every_registered_delegator_reaches_its_member() {
        // R4 a delegator that stops delegating has become a third twin.
        for (entry, member) in DELEGATORS {
            assert!(
                body(entry).contains(&format!("{member}(")),
                "R4 delegator `{entry}` no longer hands its batch to `{member}`"
            );
        }
    }
}
