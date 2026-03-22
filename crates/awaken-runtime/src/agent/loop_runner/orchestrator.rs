//! Main agent loop orchestration — `run_agent_loop_controlled`.

use std::sync::Arc;

use crate::runtime::{AgentResolver, CancellationToken, PhaseContext, PhaseRuntime};
use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::identity::RunIdentity;
use awaken_contract::contract::inference::InferenceOverride;
use awaken_contract::contract::lifecycle::TerminationReason;
use awaken_contract::contract::message::{Message, Role, gen_message_id};
use awaken_contract::contract::storage::ThreadRunStore;
use awaken_contract::contract::suspension::ToolCallResume;
use awaken_contract::model::Phase;
use futures::channel::mpsc::UnboundedReceiver;

use super::super::state::{RunLifecycle, RunLifecycleUpdate, ToolCallStates, ToolCallStatesUpdate};
use super::checkpoint::{
    check_termination, complete_step, emit_state_snapshot, persist_checkpoint,
};
use super::resume::{WaitOutcome, wait_for_resume_or_cancel};
use super::setup::{PreparedRun, prepare_run};
use super::step::{StepContext, StepOutcome, execute_step};
use super::truncation_recovery::TruncationState;
use super::{AgentLoopError, AgentRunResult, commit_update, now_ms};

/// Agent loop implementation with runtime control channels.
///
/// Prefer calling through `AgentRuntime::run()` in production code.
#[tracing::instrument(skip_all, fields(agent_id = %initial_agent_id, run_id = %run_identity.run_id))]
pub async fn run_agent_loop_controlled(
    resolver: &dyn AgentResolver,
    initial_agent_id: &str,
    runtime: &PhaseRuntime,
    sink: &dyn EventSink,
    checkpoint_store: Option<&dyn ThreadRunStore>,
    initial_messages: Vec<Message>,
    run_identity: RunIdentity,
    cancellation_token: Option<CancellationToken>,
    decision_rx: Option<UnboundedReceiver<(String, ToolCallResume)>>,
    initial_overrides: Option<InferenceOverride>,
) -> Result<AgentRunResult, AgentLoopError> {
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

    let make_ctx = |phase: Phase, msgs: &[Arc<Message>], identity: &RunIdentity| -> PhaseContext {
        PhaseContext::new(phase, store.snapshot())
            .with_run_identity(identity.clone())
            .with_messages(msgs.to_vec())
    };

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

    runtime
        .run_phase_with_context(&env, make_ctx(Phase::RunStart, &messages, &run_identity))
        .await?;

    // --- Main loop ---
    let termination = loop {
        steps += 1;
        tracing::info!(step = steps, "step_start");

        // Handoff: check ActiveAgentKey for agent switch
        if let Some(Some(active_id)) =
            store.read::<awaken_contract::contract::profile::ActiveAgentIdKey>()
            && active_id != agent.id
            && let Ok(resolved) = resolver.resolve(&active_id)
        {
            agent = resolved.config;
            env = resolved.env;
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
            sink,
            checkpoint_store,
            run_identity: &run_identity,
            cancellation_token: cancellation_token.as_ref(),
            run_overrides: &run_overrides,
            total_input_tokens: &mut total_input_tokens,
            total_output_tokens: &mut total_output_tokens,
            truncation_state: &mut truncation_state,
            run_created_at,
        };

        match execute_step(&mut step_ctx).await? {
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
                    sink,
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
                    sink,
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

    runtime
        .run_phase_with_context(&env, make_ctx(Phase::RunEnd, &messages, &run_identity))
        .await?;

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

    emit_state_snapshot(store, sink).await;

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
