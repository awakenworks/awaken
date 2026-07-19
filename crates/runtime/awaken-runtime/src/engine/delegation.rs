//! Delegated child-Run dispatch and resume.
//!
//! Kept separate from the main loop so parent/child correlation and continuation
//! handling form one bounded responsibility.

use super::*;

/// Stable parent context needed to address and durably deliver one delegated
/// child Run. Grouping these cohesive identities keeps the dispatch function's
/// signature in domain terms instead of passing five unrelated scalars.
#[derive(Clone, Copy)]
pub(super) struct DelegationParent<'a> {
    pub context: &'a RuntimeRunContext,
    pub origin: Option<&'a DelegationOrigin>,
    pub agent_id: &'a str,
    pub run_id: &'a RunId,
    pub thread_id: &'a ThreadId,
}

/// Commit the immutable parent/call/child relationship through the same Run
/// state delta that makes executor entry recoverable.
pub(super) fn stage_delegation_request(
    runtime: &Runtime,
    resolved: &ResolvedRun,
    delegation_origin: Option<&DelegationOrigin>,
    run_id: &RunId,
    call: &ToolCall,
    store: &mut Store,
    staged_state: &mut Vec<StateCommand>,
) -> Result<bool> {
    stage_delegation_requests(
        runtime,
        resolved,
        delegation_origin,
        run_id,
        std::slice::from_ref(call),
        store,
        staged_state,
    )
}

/// Admit a parallel child batch against one recovered registry snapshot and
/// persist the resulting relationship set once. This prevents N whole-registry
/// writes when one model step delegates to N Agents.
pub(super) fn stage_delegation_requests(
    runtime: &Runtime,
    resolved: &ResolvedRun,
    delegation_origin: Option<&DelegationOrigin>,
    run_id: &RunId,
    calls: &[ToolCall],
    store: &mut Store,
    staged_state: &mut Vec<StateCommand>,
) -> Result<bool> {
    let Some(executor) = runtime.run_delegation() else {
        return Ok(false);
    };
    if calls.is_empty() || calls.iter().any(|call| executor.tool_id() != call.tool_id) {
        return Ok(false);
    }
    let mut registry = RunDelegations::load(store)
        .map_err(|error| Error::Execution(error.to_string()))?
        .unwrap_or_else(|| {
            DelegationRegistry::new(
                run_id.clone(),
                resolved.agent_id.0.clone(),
                delegation_origin
                    .map(|origin| origin.agent_lineage.clone())
                    .unwrap_or_default(),
                delegation_origin.map_or(0, |origin| origin.depth),
                resolved.spec.delegation_limits,
            )
        });
    for call in calls {
        let origin = derive_delegation_origin(
            delegation_origin,
            &resolved.agent_id.0,
            run_id,
            &call.call_id,
        )
        .map_err(|error| Error::Execution(error.to_string()))?;
        let target_agent_id = executor
            .target_agent_id(&call.arguments)
            .map_err(|error| Error::Execution(error.to_string()))?;
        let child_run_id = origin.child_run_id();
        registry
            .request(RequestDelegation {
                id: origin.delegation_id,
                parent_call_id: call.call_id.clone(),
                target_agent_id,
                child_run_id,
            })
            .map_err(|error| Error::Execution(error.to_string()))?;
    }
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
        .run_delegation()
        .is_none_or(|executor| executor.tool_id() != call.tool_id)
    {
        return Ok(());
    }
    let delegation_id = DelegationId::for_parent_call(run_id, &call.call_id);
    let mut registry = RunDelegations::load(store)
        .map_err(|error| Error::Execution(error.to_string()))?
        .ok_or_else(|| Error::Execution("delegation relationship is not committed".into()))?;
    registry
        .complete(&delegation_id)
        .map_err(|error| Error::Execution(error.to_string()))?;
    let command = RunDelegations::write(&Some(registry));
    store.apply(&command);
    staged_state.push(command);

    // The durable delivery envelope and the ToolBatch terminal result are
    // consumed by the same parent commit. Executor errors have no envelope, so
    // absence is legal; a delivered child result is removed exactly once.
    let mut results =
        PendingChildRunResults::load(store).map_err(|error| Error::Execution(error.to_string()))?;
    if results.get(&delegation_id).is_some() {
        results
            .consume(&delegation_id)
            .map_err(|error| Error::Execution(error.to_string()))?;
        let command = if results.is_empty() {
            PendingChildRunResults::remove()
        } else {
            PendingChildRunResults::write(&results)
        };
        store.apply(&command);
        staged_state.push(command);
    }
    Ok(())
}

/// Persist the adapter reference needed to cancel an awaiting child after the
/// process that received the continuation has disappeared. It is committed in
/// the same boundary as the parent's Delegation wait.
pub(super) fn stage_delegation_awaiting(
    runtime: &Runtime,
    run_id: &RunId,
    call: &ToolCall,
    continuation: &serde_json::Value,
    store: &mut Store,
    staged_state: &mut Vec<StateCommand>,
) -> Result<()> {
    if runtime
        .run_delegation()
        .is_none_or(|executor| executor.tool_id() != call.tool_id)
    {
        return Ok(());
    }
    let delegation_id = DelegationId::for_parent_call(run_id, &call.call_id);
    let mut registry = RunDelegations::load(store)
        .map_err(|error| Error::Execution(error.to_string()))?
        .ok_or_else(|| Error::Execution("delegation relationship is not committed".into()))?;
    registry
        .record_cancellation_reference(&delegation_id, continuation.clone())
        .map_err(|error| Error::Execution(error.to_string()))?;
    let command = RunDelegations::write(&Some(registry));
    store.apply(&command);
    staged_state.push(command);
    Ok(())
}

/// Make a child Run's ended value durable before the parent publishes it as a
/// tool result. The stable delegation identity makes retries idempotent; a
/// conflicting payload or relationship fails closed.
pub(super) async fn persist_child_run_result(
    context: &RuntimeRunContext,
    thread_id: &ThreadId,
    disposition: RunDisposition,
    id: &DelegationId,
    store: &mut Store,
    result: ChildRunResult,
) -> std::result::Result<ChildRunResult, DelegationExecutionError> {
    let registry = RunDelegations::load(store)
        .map_err(|error| DelegationExecutionError::new(error.to_string()))?
        .ok_or_else(|| DelegationExecutionError::new("delegation relationship is not committed"))?;
    let relationship = registry
        .get(id)
        .ok_or_else(|| DelegationExecutionError::new("delegation relationship is not committed"))?;
    if relationship.child_run_id != result.child_run_id {
        return Err(DelegationExecutionError::new(
            "child result does not match the committed delegation relationship",
        ));
    }
    if relationship.status != awaken_agent_contract::agent::delegation::DelegationStatus::Open {
        return Err(DelegationExecutionError::new(
            "child result arrived after its delegation relationship closed",
        ));
    }

    let mut results = PendingChildRunResults::load(store)
        .map_err(|error| DelegationExecutionError::new(error.to_string()))?;
    let record = results
        .record(id.clone(), result)
        .map_err(|error| DelegationExecutionError::new(error.to_string()))?;
    let durable = results
        .get(id)
        .cloned()
        .expect("recorded delegation result is immediately readable");
    if record == ResultRecord::Duplicate {
        return Ok(durable);
    }

    let command = PendingChildRunResults::write(&results);
    if let Some(coordinator) = &context.commit {
        coordinator
            .commit(ThreadCommit::assemble(
                thread_id.clone(),
                disposition,
                false,
                Vec::new(),
                vec![command.clone()],
                Vec::new(),
            ))
            .await
            .map_err(|error| DelegationExecutionError::new(error.to_string()))?;
    }
    store.apply(&command);
    Ok(durable)
}

/// One placement-neutral child invocation before its ended value is copied into
/// the parent's durable delivery inbox. Keeping invocation and delivery as two
/// boundaries lets several terminal-only children run concurrently while their
/// parent-thread commits remain serialized and lossless.
pub(super) struct DelegationInvocation {
    pub id: DelegationId,
    pub step: DelegationStep,
}

/// Address and invoke a child without mutating parent state. Existing durable
/// results short-circuit executor entry. The caller owns the subsequent ordered
/// result-delivery commits.
pub(super) async fn invoke_delegation(
    runtime: &Runtime,
    parent: DelegationParent<'_>,
    call: &ToolCall,
    store: &Store,
) -> Option<std::result::Result<DelegationInvocation, DelegationExecutionError>> {
    let executor = runtime.run_delegation()?;
    if executor.tool_id() != call.tool_id {
        return None;
    }
    let origin = match derive_delegation_origin(
        parent.origin,
        parent.agent_id,
        parent.run_id,
        &call.call_id,
    ) {
        Ok(origin) => origin,
        Err(error) => return Some(Err(error)),
    };
    let id = origin.delegation_id.clone();
    match PendingChildRunResults::load(store) {
        Ok(results) => {
            if let Some(result) = results.get(&id).cloned() {
                return Some(Ok(DelegationInvocation {
                    id,
                    step: DelegationStep::Ended {
                        text: result.text,
                        usage: result.usage,
                    },
                }));
            }
        }
        Err(error) => return Some(Err(DelegationExecutionError::new(error.to_string()))),
    }
    let request = DelegationRequest {
        child_run_id: origin.child_run_id(),
        parent_thread_id: parent.thread_id.clone(),
        context: parent.context.for_child_run(),
        origin,
        arguments: call.arguments.clone(),
    };
    Some(
        executor
            .start(request)
            .await
            .map(|step| DelegationInvocation { id, step }),
    )
}

/// Retryable child interruption leaves the durable relationship and executing
/// call open for recovery. Only a terminal child failure becomes a model-visible
/// tool result and releases the relationship's parallel slot.
pub(super) fn delegation_error_output(
    call_id: &str,
    error: DelegationExecutionError,
) -> Result<ToolOutput> {
    if error.is_retryable() {
        Err(Error::Execution(error.to_string()))
    } else {
        Ok(ToolOutput::error(call_id, error.to_string()))
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
    let pending = ticket.pending_tool.as_ref().ok_or_else(|| {
        Error::Execution("awaiting delegation is missing its pending tool".to_string())
    })?;
    let call = ToolCall {
        call_id: call_id.clone(),
        tool_id: pending.tool_id.clone(),
        arguments: pending.arguments.clone(),
    };
    let Some(executor) = runtime.run_delegation() else {
        return finish(
            runtime,
            context,
            thread_id,
            run_id.clone(),
            RunStepResult::capability_bound(run_id.clone()),
        )
        .await;
    };
    let mut store = store_from_commands(reader.committed_state(thread_id), run_id);
    let step = match derive_delegation_origin(
        ticket.delegation_origin.as_ref(),
        &resolved.agent_id.0,
        run_id,
        &call_id,
    ) {
        Ok(origin) => {
            let id = origin.delegation_id.clone();
            if let Some(result) = PendingChildRunResults::load(&store)
                .map_err(|error| Error::Execution(error.to_string()))?
                .get(&id)
                .cloned()
            {
                Ok(DelegationStep::Ended {
                    text: result.text,
                    usage: result.usage,
                })
            } else {
                let continuation = RunDelegations::load(&store)
                    .map_err(|error| Error::Execution(error.to_string()))?
                    .and_then(|registry| {
                        registry
                            .get(&id)
                            .and_then(|relationship| relationship.cancellation_reference.clone())
                    })
                    .ok_or_else(|| {
                        Error::Execution(
                            "awaiting delegation is missing its durable execution reference"
                                .to_string(),
                        )
                    })?;
                let step = executor
                    .resume(DelegationResume {
                        child_run_id: origin.child_run_id(),
                        parent_thread_id: thread_id.clone(),
                        context: context.for_child_run(),
                        origin: origin.clone(),
                        continuation,
                        result,
                    })
                    .await;
                match step {
                    Ok(DelegationStep::Ended { text, usage }) => {
                        let result = persist_child_run_result(
                            context,
                            thread_id,
                            RunDisposition::awaiting(ticket.clone()),
                            &id,
                            &mut store,
                            ChildRunResult {
                                child_run_id: origin.child_run_id(),
                                text,
                                usage,
                            },
                        )
                        .await;
                        result.map(|result| DelegationStep::Ended {
                            text: result.text,
                            usage: result.usage,
                        })
                    }
                    other => other,
                }
            }
        }
        Err(error) => Err(error),
    };

    let synthetic = match step {
        Ok(DelegationStep::Ended { text, .. }) => {
            ResumeResult::ToolResult(ToolOutput::ok(&call_id, text))
        }
        Ok(DelegationStep::Awaiting { continuation }) => {
            let mut staged_state = Vec::new();
            stage_delegation_awaiting(
                runtime,
                run_id,
                &call,
                &continuation,
                &mut store,
                &mut staged_state,
            )?;
            let next_ticket = ticket.clone();
            return finish(
                runtime,
                context,
                thread_id,
                run_id.clone(),
                RunStepResult {
                    new_messages: Vec::new(),
                    staged_state,
                    audit: Vec::new(),
                    disposition: RunDisposition::awaiting(next_ticket),
                },
            )
            .await;
        }
        Err(error) => ResumeResult::ToolResult(delegation_error_output(&call_id, error)?),
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
    parent: DelegationParent<'_>,
    call: &ToolCall,
    store: &mut Store,
) -> Option<std::result::Result<DelegationStep, DelegationExecutionError>> {
    let invocation = invoke_delegation(runtime, parent, call, store).await?;
    match invocation {
        Ok(DelegationInvocation {
            id,
            step: DelegationStep::Ended { text, usage },
        }) => {
            let result = persist_child_run_result(
                parent.context,
                parent.thread_id,
                RunDisposition::running(parent.run_id.clone()),
                &id,
                store,
                ChildRunResult {
                    child_run_id: id.child_run_id(),
                    text,
                    usage,
                },
            )
            .await;
            Some(result.map(|result| DelegationStep::Ended {
                text: result.text,
                usage: result.usage,
            }))
        }
        Ok(DelegationInvocation { step, .. }) => Some(Ok(step)),
        Err(error) => Some(Err(error)),
    }
}

fn derive_delegation_origin(
    delegation_origin: Option<&DelegationOrigin>,
    parent_agent_id: &str,
    parent_run_id: &RunId,
    parent_call_id: &str,
) -> std::result::Result<DelegationOrigin, DelegationExecutionError> {
    match delegation_origin {
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

/// Redeliver every cancellation outbox entry retained by an ended parent Run.
/// The relationship status is intentionally not acknowledged locally: remote
/// cancellation is idempotent, so retaining the intent makes a later process
/// able to retry after an ambiguous network failure.
pub(crate) async fn reconcile_delegation_cancellations(
    runtime: &Runtime,
    thread_id: &ThreadId,
    reader: &dyn ThreadReader,
) -> Result<usize> {
    if runtime.run_delegation().is_none() {
        return Ok(0);
    }
    let commands = reader.committed_state(thread_id);
    let run_ids: std::collections::HashSet<RunId> = commands
        .iter()
        .filter(|command| command.key.0 == RunDelegations::KEY)
        .filter_map(|command| command.run_id.clone())
        .collect();
    let mut delivered = 0usize;
    let mut first_error = None;
    for run_id in run_ids {
        let store = store_from_commands(commands.clone(), &run_id);
        let Some(registry) =
            RunDelegations::load(&store).map_err(|error| Error::Execution(error.to_string()))?
        else {
            continue;
        };
        for cancellation in registry.pending_cancellations() {
            match runtime.deliver_child_cancellation(cancellation).await {
                Ok(true) => delivered += 1,
                Ok(false) => {}
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
    }
    match first_error {
        Some(error) => Err(Error::Execution(error.to_string())),
        None => Ok(delivered),
    }
}
