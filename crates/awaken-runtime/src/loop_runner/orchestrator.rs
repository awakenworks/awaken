//! Main agent loop orchestration.

use crate::context::{TruncationState, try_consume_compaction_event};
use crate::inbox::{inbox_payload_messages, is_pending_boundary_wake_payload};
use crate::state::StateStore;
use awaken_runtime_contract::contract::event::AgentEvent;
use awaken_runtime_contract::contract::lifecycle::{RunStatus, TerminationReason};
use awaken_runtime_contract::contract::message::{DeliveryBoundary, Message, Role, gen_message_id};
use awaken_runtime_contract::model::Phase;
use std::sync::Arc;

use super::checkpoint::{
    CheckpointPersist, StepCompletion, check_termination, complete_step, emit_state_snapshot,
    persist_checkpoint,
};
use super::resume::{
    WaitOutcome, detect_and_replay_resume, has_suspended_calls, wait_for_resume_or_cancel,
};
use super::setup::{PreparedRun, prepare_run};
use super::step::{self, StepContext, StepOutcome, execute_step};
use super::{
    AgentLoopError, AgentLoopParams, AgentRunResult, PendingBoundaryHandler, commit_update, now_ms,
};
use crate::agent::state::{RunLifecycle, RunLifecycleUpdate, ToolCallStates, ToolCallStatesUpdate};
#[cfg(feature = "handoff")]
use crate::state::MutationBatch;

/// Returns `true` when any plugin has declared pending work.
///
/// Reads [`PendingWorkKey`] from the state store. Plugins (e.g.
/// `BackgroundTaskPlugin`) set this at `Phase::StepEnd` when they
/// have outstanding work that should prevent NaturalEnd.
fn has_pending_work(store: &StateStore) -> bool {
    use crate::agent::state::PendingWorkKey;
    store
        .read::<PendingWorkKey>()
        .map(|s| s.has_pending)
        .unwrap_or(false)
}

async fn apply_inbox_payloads_at_boundary(
    pending_boundary: Option<&Arc<dyn PendingBoundaryHandler>>,
    boundary: DeliveryBoundary,
    messages: &mut Vec<Arc<Message>>,
    payloads: Vec<serde_json::Value>,
    store: &StateStore,
) -> Result<bool, AgentLoopError> {
    let mut inbox_messages = Vec::new();
    let mut changed = false;
    let mut wake_pending = false;
    for payload in payloads {
        if is_pending_boundary_wake_payload(&payload) {
            wake_pending = true;
            continue;
        }
        if try_consume_compaction_event(messages, &payload, store) {
            changed = true;
            continue;
        }
        inbox_messages.extend(inbox_payload_messages(&payload));
    }
    if inbox_messages.is_empty() {
        if wake_pending {
            let frozen = apply_pending_boundary(pending_boundary, boundary, messages).await?;
            return Ok(changed || frozen);
        }
        return Ok(changed);
    }

    let Some(handler) = pending_boundary else {
        messages.extend(inbox_messages.into_iter().map(Arc::new));
        return Ok(true);
    };

    handler
        .stage_pending_messages(boundary, inbox_messages)
        .await?;
    apply_pending_boundary(Some(handler), boundary, messages).await
}

async fn apply_pending_boundary(
    pending_boundary: Option<&Arc<dyn PendingBoundaryHandler>>,
    boundary: DeliveryBoundary,
    messages: &mut Vec<Arc<Message>>,
) -> Result<bool, AgentLoopError> {
    let Some(handler) = pending_boundary else {
        return Ok(false);
    };
    let Some(freeze) = handler.freeze_pending_boundary(boundary).await? else {
        return Ok(false);
    };
    if freeze.messages.is_empty() {
        return Ok(false);
    }
    messages.extend(freeze.messages.into_iter().map(Arc::new));
    Ok(true)
}

#[tracing::instrument(skip_all, fields(agent_id = %params.agent_id, run_id = %params.run_identity.run_id))]
pub(super) async fn run_agent_loop_impl(
    params: AgentLoopParams<'_>,
    thread_ctx: Option<crate::ThreadContextSnapshot>,
    pending_boundary: Option<Arc<dyn PendingBoundaryHandler>>,
) -> Result<AgentRunResult, AgentLoopError> {
    let AgentLoopParams {
        resolver,
        agent_id: initial_agent_id,
        runtime,
        sink,
        checkpoint_store,
        commit,
        messages: initial_messages,
        run_identity,
        cancellation_token,
        decision_rx,
        overrides: initial_overrides,
        frontend_tools,
        mut inbox,
        is_continuation,
        initial_state_seed,
    } = params;

    let store = runtime.store();
    let run_overrides = initial_overrides;
    let mut decision_rx = decision_rx;
    let run_created_at = now_ms();
    let mut total_input_tokens: u64 = 0;
    let mut total_output_tokens: u64 = 0;

    // --- Setup: resolve, trim, resume ---
    let PreparedRun {
        mut agent,
        mut messages,
    } = prepare_run(
        resolver,
        runtime,
        initial_agent_id,
        initial_messages,
        &run_identity,
    )
    .await?;
    let input_message_count = messages.len();

    if let Some(seed) = initial_state_seed {
        store
            .apply_seed(seed, awaken_runtime_contract::UnknownKeyPolicy::Error)
            .map_err(AgentLoopError::PhaseError)?;
    }

    // Inject frontend-defined tools as executable FrontEndTool instances.
    // Each suspends on execute(), so the protocol layer forwards the call
    // to the frontend for client-side handling.
    for desc in frontend_tools {
        let id = desc.id.clone();
        agent.tools.insert(
            id,
            std::sync::Arc::new(awaken_runtime_contract::contract::tool::FrontEndTool::new(
                desc,
            )),
        );
    }

    let mut steps: usize = 0;
    let mut truncation_state = TruncationState::new();

    // --- Run lifecycle: Start or resume ---
    if is_continuation {
        commit_update::<RunLifecycle>(
            store,
            RunLifecycleUpdate::SetRunning {
                updated_at: now_ms(),
            },
        )?;
    } else {
        commit_update::<RunLifecycle>(
            store,
            RunLifecycleUpdate::Start {
                run_id: run_identity.run_id.clone(),
                updated_at: now_ms(),
            },
        )?;
    }

    sink.emit(AgentEvent::RunStart {
        thread_id: run_identity.thread_id.clone(),
        run_id: run_identity.run_id.clone(),
        parent_run_id: run_identity.parent_run_id.clone(),
        identity: Some(run_identity.clone()),
    })
    .await;
    detect_and_replay_resume(&agent, runtime, &run_identity, &mut messages, sink.clone()).await?;

    match runtime
        .run_phase_with_context(
            &agent.env,
            step::make_ctx(
                Phase::RunStart,
                &messages,
                &run_identity,
                store,
                cancellation_token.as_ref(),
                &agent,
            ),
        )
        .await
    {
        Ok(_) => {}
        Err(awaken_runtime_contract::StateError::Cancelled) => {
            return Ok(AgentRunResult {
                run_id: run_identity.run_id.clone(),
                response: String::new(),
                termination: TerminationReason::Cancelled,
                steps: 0,
            });
        }
        Err(e) => return Err(AgentLoopError::PhaseError(e)),
    }

    // --- Main loop ---
    let termination = 'run_loop: loop {
        steps += 1;
        tracing::info!(step = steps, "step_start");

        // Handoff: check ActiveAgentKey for agent switch
        #[cfg(feature = "handoff")]
        if let Some(Some(active_id)) =
            store.read::<awaken_runtime_contract::contract::active_agent::ActiveAgentIdKey>()
            && active_id != agent.id()
        {
            match resolver.resolve(&active_id) {
                Ok(resolved) => {
                    if !resolved.env.key_registrations.is_empty() {
                        store
                            .register_keys(&resolved.env.key_registrations)
                            .map_err(AgentLoopError::PhaseError)?;
                    }

                    // Deactivate old plugins
                    {
                        let mut deactivate_patch = MutationBatch::new();
                        for plugin in &agent.env.plugins {
                            plugin
                                .on_deactivate(&mut deactivate_patch)
                                .map_err(AgentLoopError::PhaseError)?;
                        }
                        if !deactivate_patch.is_empty() {
                            store
                                .commit(deactivate_patch)
                                .map_err(AgentLoopError::PhaseError)?;
                        }
                    }

                    // Activate new plugins
                    {
                        let mut activate_patch = MutationBatch::new();
                        for plugin in &resolved.env.plugins {
                            plugin
                                .on_activate(&resolved.spec, &mut activate_patch)
                                .map_err(AgentLoopError::PhaseError)?;
                        }
                        if !activate_patch.is_empty() {
                            store
                                .commit(activate_patch)
                                .map_err(AgentLoopError::PhaseError)?;
                        }
                    }

                    tracing::info!(from = %agent.id(), to = %active_id, "agent_handoff");
                    agent = resolved;
                }
                Err(e) => {
                    tracing::error!(agent_id = %active_id, error = %e, "handoff_resolve_failed");
                    break TerminationReason::Blocked(format!("handoff resolve failed: {e}"));
                }
            }
        }

        sink.emit(AgentEvent::StepStart {
            message_id: gen_message_id(),
        })
        .await;

        // Clear tool call states from previous step
        commit_update::<ToolCallStates>(store, ToolCallStatesUpdate::Clear)?;

        let mut step_ctx = StepContext {
            agent: &mut agent,
            messages: &mut messages,
            runtime,
            sink: sink.clone(),
            checkpoint_store,
            commit,
            run_identity: &run_identity,
            input_message_count,
            cancellation_token: cancellation_token.as_ref(),
            run_overrides: &run_overrides,
            total_input_tokens: &mut total_input_tokens,
            total_output_tokens: &mut total_output_tokens,
            truncation_state: &mut truncation_state,
            run_created_at,
            thread_ctx: thread_ctx.as_ref(),
        };

        let step_result = match execute_step(&mut step_ctx).await {
            Ok(outcome) => outcome,
            Err(AgentLoopError::PhaseError(awaken_runtime_contract::StateError::Cancelled)) => {
                StepOutcome::Cancelled
            }
            Err(e) => return Err(e),
        };
        match step_result {
            StepOutcome::Cancelled => {
                // Close the current step before breaking.
                complete_step(StepCompletion {
                    store,
                    runtime,
                    env: &agent.env,
                    sink: sink.as_ref(),
                    checkpoint_store,
                    commit,
                    messages: &messages,
                    input_message_count,
                    run_identity: &run_identity,
                    run_created_at,
                    total_input_tokens,
                    total_output_tokens,
                    thread_ctx: thread_ctx.as_ref(),
                })
                .await?;
                break TerminationReason::Cancelled;
            }
            StepOutcome::NaturalEnd => {
                // Drain inbox: catch events that arrived during this step.
                // If new messages arrived, continue the loop so LLM can
                // process them — don't terminate with unprocessed messages.
                let mut has_new_messages = false;
                if let Some(ref mut inbox) = inbox
                    && apply_inbox_payloads_at_boundary(
                        pending_boundary.as_ref(),
                        DeliveryBoundary::OnNaturalEnd,
                        &mut messages,
                        inbox.drain(),
                        store,
                    )
                    .await?
                {
                    has_new_messages = true;
                }

                if has_new_messages {
                    // New messages arrived — let LLM process them
                    continue;
                }

                if apply_pending_boundary(
                    pending_boundary.as_ref(),
                    DeliveryBoundary::OnNaturalEnd,
                    &mut messages,
                )
                .await?
                {
                    continue;
                }

                if has_pending_work(store) {
                    // Background tasks still running but no new messages yet.
                    if run_identity.origin()
                        == awaken_runtime_contract::contract::identity::RunOrigin::Subagent
                    {
                        // Sub-agent: wait in-process for task events via inbox.
                        // This keeps the sub-agent alive until all tasks complete,
                        // so it can produce a final summary with all results.
                        if let Some(ref mut inbox) = inbox {
                            match inbox.recv_or_cancel(cancellation_token.as_ref()).await {
                                Some(msg) => {
                                    let mut payloads = vec![msg];
                                    payloads.extend(inbox.drain());
                                    apply_inbox_payloads_at_boundary(
                                        pending_boundary.as_ref(),
                                        DeliveryBoundary::OnNaturalEnd,
                                        &mut messages,
                                        payloads,
                                        store,
                                    )
                                    .await?;
                                    continue; // back to loop — LLM processes events
                                }
                                None => break TerminationReason::Cancelled,
                            }
                        }
                        // No inbox — fall through to top-level behavior
                    }

                    // Top-level agent: persist Waiting state and release worker.
                    // Mailbox continuation will resume when tasks complete.
                    commit_update::<RunLifecycle>(
                        store,
                        RunLifecycleUpdate::SetWaiting {
                            updated_at: now_ms(),
                            pause_reason: "awaiting_tasks".into(),
                        },
                    )?;
                    break TerminationReason::NaturalEnd;
                } else {
                    break TerminationReason::NaturalEnd;
                }
            }
            StepOutcome::Blocked(reason) => {
                // Close the current step before breaking.
                complete_step(StepCompletion {
                    store,
                    runtime,
                    env: &agent.env,
                    sink: sink.as_ref(),
                    checkpoint_store,
                    commit,
                    messages: &messages,
                    input_message_count,
                    run_identity: &run_identity,
                    run_created_at,
                    total_input_tokens,
                    total_output_tokens,
                    thread_ctx: thread_ctx.as_ref(),
                })
                .await?;
                break TerminationReason::Blocked(reason);
            }
            StepOutcome::Terminated(reason) => {
                // Close the current step before terminating.
                // check_termination() fires inside run_step() before complete_step(),
                // so the step is still open when we reach here.
                complete_step(StepCompletion {
                    store,
                    runtime,
                    env: &agent.env,
                    sink: sink.as_ref(),
                    checkpoint_store,
                    commit,
                    messages: &messages,
                    input_message_count,
                    run_identity: &run_identity,
                    run_created_at,
                    total_input_tokens,
                    total_output_tokens,
                    thread_ctx: thread_ctx.as_ref(),
                })
                .await?;
                break reason;
            }
            StepOutcome::Suspended => {
                // Transition run to Waiting
                commit_update::<RunLifecycle>(
                    store,
                    RunLifecycleUpdate::SetWaiting {
                        updated_at: now_ms(),
                        pause_reason: "suspended".into(),
                    },
                )?;
                complete_step(StepCompletion {
                    store,
                    runtime,
                    env: &agent.env,
                    sink: sink.as_ref(),
                    checkpoint_store,
                    commit,
                    messages: &messages,
                    input_message_count,
                    run_identity: &run_identity,
                    run_created_at,
                    total_input_tokens,
                    total_output_tokens,
                    thread_ctx: thread_ctx.as_ref(),
                })
                .await?;

                // Emit RunFinish(Suspended) so protocol encoders can send
                // the appropriate interrupt signal to the client. AG-UI
                // clients (e.g. CopilotKit) need RUN_FINISHED with
                // `outcome: "interrupt"` to activate approval UIs.
                emit_state_snapshot(store, sink.as_ref()).await;
                sink.emit(AgentEvent::RunFinish {
                    thread_id: run_identity.thread_id.clone(),
                    run_id: run_identity.run_id.clone(),
                    identity: Some(run_identity.clone()),
                    result: None,
                    termination: TerminationReason::Suspended,
                })
                .await;

                loop {
                    match wait_for_resume_or_cancel(
                        decision_rx.as_mut(),
                        inbox.as_mut(),
                        cancellation_token.as_ref(),
                        runtime,
                    )
                    .await?
                    {
                        WaitOutcome::Resumed => {
                            sink.emit(AgentEvent::RunStart {
                                thread_id: run_identity.thread_id.clone(),
                                run_id: run_identity.run_id.clone(),
                                parent_run_id: run_identity.parent_run_id.clone(),
                                identity: Some(run_identity.clone()),
                            })
                            .await;
                            detect_and_replay_resume(
                                &agent,
                                runtime,
                                &run_identity,
                                &mut messages,
                                sink.clone(),
                            )
                            .await?;

                            if has_suspended_calls(store) {
                                emit_state_snapshot(store, sink.as_ref()).await;
                                sink.emit(AgentEvent::RunFinish {
                                    thread_id: run_identity.thread_id.clone(),
                                    run_id: run_identity.run_id.clone(),
                                    identity: Some(run_identity.clone()),
                                    result: None,
                                    termination: TerminationReason::Suspended,
                                })
                                .await;
                                continue;
                            }

                            commit_update::<RunLifecycle>(
                                store,
                                RunLifecycleUpdate::SetRunning {
                                    updated_at: now_ms(),
                                },
                            )?;
                            continue 'run_loop;
                        }
                        WaitOutcome::InboxMessages(events) => {
                            let appended = apply_inbox_payloads_at_boundary(
                                pending_boundary.as_ref(),
                                DeliveryBoundary::NextStep,
                                &mut messages,
                                events,
                                store,
                            )
                            .await?;
                            if appended && pending_boundary.is_none() {
                                persist_checkpoint(CheckpointPersist {
                                    store,
                                    checkpoint_store,
                                    commit,
                                    messages: &messages,
                                    input_message_count,
                                    run_identity: &run_identity,
                                    run_created_at,
                                    total_input_tokens,
                                    total_output_tokens,
                                    termination_reason: None,
                                    final_output: None,
                                    error_payload: None,
                                    thread_ctx: thread_ctx.as_ref(),
                                })
                                .await?;
                            }
                            continue;
                        }
                        WaitOutcome::Cancelled => {
                            break 'run_loop TerminationReason::Cancelled;
                        }
                        WaitOutcome::NoDecisionChannel => {
                            break 'run_loop TerminationReason::Suspended;
                        }
                    }
                }
            }
            StepOutcome::Continue => {
                complete_step(StepCompletion {
                    store,
                    runtime,
                    env: &agent.env,
                    sink: sink.as_ref(),
                    checkpoint_store,
                    commit,
                    messages: &messages,
                    input_message_count,
                    run_identity: &run_identity,
                    run_created_at,
                    total_input_tokens,
                    total_output_tokens,
                    thread_ctx: thread_ctx.as_ref(),
                })
                .await?;
                // Drain inbox messages that arrived during step execution
                if let Some(ref mut inbox) = inbox {
                    apply_inbox_payloads_at_boundary(
                        pending_boundary.as_ref(),
                        DeliveryBoundary::NextStep,
                        &mut messages,
                        inbox.drain(),
                        store,
                    )
                    .await?;
                }
                if apply_pending_boundary(
                    pending_boundary.as_ref(),
                    DeliveryBoundary::NextStep,
                    &mut messages,
                )
                .await?
                {
                    continue;
                }
                if let Some(reason) = check_termination(store) {
                    break reason;
                }
            }
        }
    };

    // --- Run lifecycle: Done ---
    tracing::warn!(reason = ?termination, "run_terminated");

    let lifecycle_now = store.read::<RunLifecycle>().map(|s| s.status);
    let (target_status, done_reason) = termination.to_run_status();
    if target_status.is_terminal() && lifecycle_now != Some(RunStatus::Waiting) {
        commit_update::<RunLifecycle>(
            store,
            RunLifecycleUpdate::Done {
                done_reason: done_reason.unwrap_or_else(|| "unknown".into()),
                updated_at: now_ms(),
            },
        )?;
    }

    match runtime
        .run_phase_with_context(
            &agent.env,
            step::make_ctx(
                Phase::RunEnd,
                &messages,
                &run_identity,
                store,
                cancellation_token.as_ref(),
                &agent,
            ),
        )
        .await
    {
        Ok(_) | Err(awaken_runtime_contract::StateError::Cancelled) => {}
        Err(e) => return Err(AgentLoopError::PhaseError(e)),
    }

    let response = latest_run_response(&messages, input_message_count);

    persist_checkpoint(CheckpointPersist {
        store,
        checkpoint_store,
        commit,
        messages: messages.as_slice(),
        input_message_count,
        run_identity: &run_identity,
        run_created_at,
        total_input_tokens,
        total_output_tokens,
        termination_reason: Some(termination.clone()),
        final_output: (!response.is_empty()).then(|| response.clone()),
        error_payload: match &termination {
            TerminationReason::Error(message) => Some(serde_json::json!({ "message": message })),
            _ => None,
        },
        thread_ctx: thread_ctx.as_ref(),
    })
    .await?;

    emit_state_snapshot(store, sink.as_ref()).await;

    // Include run status and reason in RunFinish result so clients can
    // distinguish "done" from "waiting" and know the specific reason.
    let lifecycle_final = store.read::<RunLifecycle>();
    let run_status = lifecycle_final
        .as_ref()
        .map(|l| l.status)
        .unwrap_or(RunStatus::Done);
    let status_reason = lifecycle_final.and_then(|l| l.status_reason);
    let status_str = match run_status {
        RunStatus::Created => "created",
        RunStatus::Running => "running",
        RunStatus::Waiting => "waiting",
        RunStatus::Done => "done",
    };
    let mut result_json = serde_json::json!({"response": response, "status": status_str});
    if let Some(reason) = &status_reason {
        result_json["status_reason"] = serde_json::json!(reason);
    }

    sink.emit(AgentEvent::RunFinish {
        thread_id: run_identity.thread_id.clone(),
        run_id: run_identity.run_id.clone(),
        identity: Some(run_identity.clone()),
        result: Some(result_json),
        termination: termination.clone(),
    })
    .await;

    Ok(AgentRunResult {
        run_id: run_identity.run_id.clone(),
        response,
        termination,
        steps,
    })
}

fn latest_run_response(messages: &[std::sync::Arc<Message>], input_message_count: usize) -> String {
    messages
        .get(input_message_count..)
        .unwrap_or(&[])
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .map(|message| message.text())
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "orchestrator_tests.rs"]
mod tests;
