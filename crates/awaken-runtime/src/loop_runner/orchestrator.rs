//! Main agent loop orchestration.

use crate::context::TruncationState;
use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::lifecycle::TerminationReason;
use awaken_contract::contract::message::{Role, gen_message_id};
use awaken_contract::model::Phase;

use super::checkpoint::{
    check_termination, complete_step, emit_state_snapshot, persist_checkpoint,
};
use super::resume::{WaitOutcome, wait_for_resume_or_cancel};
use super::setup::{PreparedRun, prepare_run};
use super::step::{self, StepContext, StepOutcome, execute_step};
use super::{AgentLoopError, AgentLoopParams, AgentRunResult, commit_update, now_ms};
use crate::agent::state::{RunLifecycle, RunLifecycleUpdate, ToolCallStates, ToolCallStatesUpdate};
use crate::state::MutationBatch;

#[tracing::instrument(skip_all, fields(agent_id = %params.agent_id, run_id = %params.run_identity.run_id))]
pub(super) async fn run_agent_loop_impl(
    params: AgentLoopParams<'_>,
) -> Result<AgentRunResult, AgentLoopError> {
    let AgentLoopParams {
        resolver,
        agent_id: initial_agent_id,
        runtime,
        sink,
        checkpoint_store,
        messages: initial_messages,
        run_identity,
        cancellation_token,
        decision_rx,
        overrides: initial_overrides,
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
        mut env,
        mut messages,
    } = prepare_run(
        resolver,
        runtime,
        initial_agent_id,
        initial_messages,
        &run_identity,
    )
    .await?;

    let mut steps: usize = 0;
    let mut truncation_state = TruncationState::new();

    // --- Run lifecycle: Start ---
    commit_update::<RunLifecycle>(
        store,
        RunLifecycleUpdate::Start {
            run_id: run_identity.run_id.clone(),
            updated_at: now_ms(),
        },
    )?;

    sink.emit(AgentEvent::RunStart {
        thread_id: run_identity.thread_id.clone(),
        run_id: run_identity.run_id.clone(),
        parent_run_id: run_identity.parent_run_id.clone(),
    })
    .await;

    match runtime
        .run_phase_with_context(
            &env,
            step::make_ctx(
                Phase::RunStart,
                &messages,
                &run_identity,
                store,
                cancellation_token.as_ref(),
            ),
        )
        .await
    {
        Ok(_) => {}
        Err(awaken_contract::StateError::Cancelled) => {
            return Ok(AgentRunResult {
                response: String::new(),
                termination: TerminationReason::Cancelled,
                steps: 0,
            });
        }
        Err(e) => return Err(AgentLoopError::PhaseError(e)),
    }

    // --- Main loop ---
    let termination = loop {
        steps += 1;
        tracing::info!(step = steps, "step_start");

        // Handoff: check ActiveAgentKey for agent switch
        #[cfg(feature = "handoff")]
        if let Some(Some(active_id)) =
            store.read::<awaken_contract::contract::run_input::ActiveAgentIdKey>()
            && active_id != agent.id
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
                        for plugin in &env.plugins {
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
                    if let Some(ref spec) = resolved.env.agent_spec {
                        let mut activate_patch = MutationBatch::new();
                        for plugin in &resolved.env.plugins {
                            plugin
                                .on_activate(spec, &mut activate_patch)
                                .map_err(AgentLoopError::PhaseError)?;
                        }
                        if !activate_patch.is_empty() {
                            store
                                .commit(activate_patch)
                                .map_err(AgentLoopError::PhaseError)?;
                        }
                    }

                    tracing::info!(from = %agent.id, to = %active_id, "agent_handoff");
                    agent = resolved.config;
                    env = resolved.env;
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
            env: &mut env,
            messages: &mut messages,
            runtime,
            sink: sink.clone(),
            checkpoint_store,
            run_identity: &run_identity,
            cancellation_token: cancellation_token.as_ref(),
            run_overrides: &run_overrides,
            total_input_tokens: &mut total_input_tokens,
            total_output_tokens: &mut total_output_tokens,
            truncation_state: &mut truncation_state,
            run_created_at,
        };

        let step_result = match execute_step(&mut step_ctx).await {
            Ok(outcome) => outcome,
            Err(AgentLoopError::PhaseError(awaken_contract::StateError::Cancelled)) => {
                StepOutcome::Cancelled
            }
            Err(e) => return Err(e),
        };
        match step_result {
            StepOutcome::Cancelled => {
                break TerminationReason::Cancelled;
            }
            StepOutcome::NaturalEnd => {
                break TerminationReason::NaturalEnd;
            }
            StepOutcome::Blocked(reason) => {
                break TerminationReason::Blocked(reason);
            }
            StepOutcome::Terminated(reason) => {
                break reason;
            }
            StepOutcome::Suspended => {
                // Transition run to Waiting
                commit_update::<RunLifecycle>(
                    store,
                    RunLifecycleUpdate::SetWaiting {
                        updated_at: now_ms(),
                    },
                )?;
                complete_step(
                    store,
                    runtime,
                    &env,
                    sink.as_ref(),
                    checkpoint_store,
                    &messages,
                    &run_identity,
                    run_created_at,
                    total_input_tokens,
                    total_output_tokens,
                )
                .await?;

                match wait_for_resume_or_cancel(
                    decision_rx.as_mut(),
                    cancellation_token.as_ref(),
                    store,
                    &agent,
                    &run_identity,
                    &mut messages,
                )
                .await?
                {
                    WaitOutcome::Resumed => {
                        commit_update::<RunLifecycle>(
                            store,
                            RunLifecycleUpdate::SetRunning {
                                updated_at: now_ms(),
                            },
                        )?;
                        continue;
                    }
                    WaitOutcome::Cancelled => break TerminationReason::Cancelled,
                    WaitOutcome::NoDecisionChannel => break TerminationReason::Suspended,
                }
            }
            StepOutcome::Continue => {
                complete_step(
                    store,
                    runtime,
                    &env,
                    sink.as_ref(),
                    checkpoint_store,
                    &messages,
                    &run_identity,
                    run_created_at,
                    total_input_tokens,
                    total_output_tokens,
                )
                .await?;
                if let Some(reason) = check_termination(store) {
                    break reason;
                }
            }
        }
    };

    // --- Run lifecycle: Done ---
    tracing::warn!(reason = ?termination, "run_terminated");

    let (target_status, done_reason) = termination.to_run_status();
    if target_status.is_terminal() {
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
            &env,
            step::make_ctx(
                Phase::RunEnd,
                &messages,
                &run_identity,
                store,
                cancellation_token.as_ref(),
            ),
        )
        .await
    {
        Ok(_) | Err(awaken_contract::StateError::Cancelled) => {}
        Err(e) => return Err(AgentLoopError::PhaseError(e)),
    }

    persist_checkpoint(
        store,
        checkpoint_store,
        messages.as_slice(),
        &run_identity,
        run_created_at,
        total_input_tokens,
        total_output_tokens,
    )
    .await?;

    emit_state_snapshot(store, sink.as_ref()).await;

    let response = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| m.text())
        .unwrap_or_default();

    sink.emit(AgentEvent::RunFinish {
        thread_id: run_identity.thread_id.clone(),
        run_id: run_identity.run_id.clone(),
        result: Some(serde_json::json!({"response": response})),
        termination: termination.clone(),
    })
    .await;

    Ok(AgentRunResult {
        response,
        termination,
        steps,
    })
}
