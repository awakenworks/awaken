//! Main agent loop orchestration.

use crate::context::TruncationState;
use crate::state::StateStore;
use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::lifecycle::{RunStatus, TerminationReason};
use awaken_contract::contract::message::{Role, gen_message_id};
use awaken_contract::model::Phase;

use super::checkpoint::{
    StepCompletion, check_termination, complete_step, emit_state_snapshot, persist_checkpoint,
};
use super::resume::{
    WaitOutcome, detect_and_replay_resume, has_suspended_calls, wait_for_resume_or_cancel,
};
use super::setup::{PreparedRun, prepare_run};
use super::step::{self, StepContext, StepOutcome, execute_step};
use super::{AgentLoopError, AgentLoopParams, AgentRunResult, commit_update, now_ms};
use crate::agent::state::{RunLifecycle, RunLifecycleUpdate, ToolCallStates, ToolCallStatesUpdate};
use crate::state::MutationBatch;

/// Create an internal user message from inbox event JSON.
/// Uses User role (not System) so LLM providers treat it as context,
/// with Internal visibility so it's hidden from API consumers.
fn inbox_event_to_message(json: &serde_json::Value) -> awaken_contract::contract::message::Message {
    let kind = json.get("kind").and_then(|k| k.as_str()).unwrap_or("event");
    let task_id = json
        .get("task_id")
        .and_then(|t| t.as_str())
        .unwrap_or("unknown");
    let text = format!(
        "<background-task-event kind=\"{kind}\" task_id=\"{task_id}\">\n{}\n</background-task-event>",
        json
    );
    let mut msg = awaken_contract::contract::message::Message::user(text);
    msg.visibility = awaken_contract::contract::message::Visibility::Internal;
    msg
}

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
        frontend_tools,
        mut inbox,
        is_continuation,
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

    // Inject frontend-defined tools as executable FrontEndTool instances.
    // Each suspends on execute(), so the protocol layer forwards the call
    // to the frontend for client-side handling.
    for desc in frontend_tools {
        let id = desc.id.clone();
        agent.tools.insert(
            id,
            std::sync::Arc::new(awaken_contract::contract::tool::FrontEndTool::new(desc)),
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
    let termination = 'run_loop: loop {
        steps += 1;
        tracing::info!(step = steps, "step_start");

        // Handoff: check ActiveAgentKey for agent switch
        #[cfg(feature = "handoff")]
        if let Some(Some(active_id)) =
            store.read::<awaken_contract::contract::active_agent::ActiveAgentIdKey>()
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
                // Close the current step before breaking.
                complete_step(StepCompletion {
                    store,
                    runtime,
                    env: &agent.env,
                    sink: sink.as_ref(),
                    checkpoint_store,
                    messages: &messages,
                    run_identity: &run_identity,
                    run_created_at,
                    total_input_tokens,
                    total_output_tokens,
                })
                .await?;
                break TerminationReason::Cancelled;
            }
            StepOutcome::NaturalEnd => {
                // Drain inbox: catch events that arrived during this step.
                // If new messages arrived, continue the loop so LLM can
                // process them — don't terminate with unprocessed messages.
                let mut has_new_messages = false;
                if let Some(ref mut inbox) = inbox {
                    for msg in inbox.drain() {
                        messages.push(std::sync::Arc::new(inbox_event_to_message(&msg)));
                        has_new_messages = true;
                    }
                }

                if has_new_messages {
                    // New messages arrived — let LLM process them
                    continue;
                }

                if has_pending_work(store) {
                    // Background tasks still running but no new messages yet.
                    if run_identity.origin
                        == awaken_contract::contract::identity::RunOrigin::Subagent
                    {
                        // Sub-agent: wait in-process for task events via inbox.
                        // This keeps the sub-agent alive until all tasks complete,
                        // so it can produce a final summary with all results.
                        if let Some(ref mut inbox) = inbox {
                            match inbox.recv_or_cancel(cancellation_token.as_ref()).await {
                                Some(msg) => {
                                    messages
                                        .push(std::sync::Arc::new(inbox_event_to_message(&msg)));
                                    for extra in inbox.drain() {
                                        messages.push(std::sync::Arc::new(inbox_event_to_message(
                                            &extra,
                                        )));
                                    }
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
                    complete_step(StepCompletion {
                        store,
                        runtime,
                        env: &agent.env,
                        sink: sink.as_ref(),
                        checkpoint_store,
                        messages: &messages,
                        run_identity: &run_identity,
                        run_created_at,
                        total_input_tokens,
                        total_output_tokens,
                    })
                    .await?;
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
                    messages: &messages,
                    run_identity: &run_identity,
                    run_created_at,
                    total_input_tokens,
                    total_output_tokens,
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
                    messages: &messages,
                    run_identity: &run_identity,
                    run_created_at,
                    total_input_tokens,
                    total_output_tokens,
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
                    messages: &messages,
                    run_identity: &run_identity,
                    run_created_at,
                    total_input_tokens,
                    total_output_tokens,
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
                    result: None,
                    termination: TerminationReason::Suspended,
                })
                .await;

                loop {
                    match wait_for_resume_or_cancel(
                        decision_rx.as_mut(),
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
                    messages: &messages,
                    run_identity: &run_identity,
                    run_created_at,
                    total_input_tokens,
                    total_output_tokens,
                })
                .await?;
                // Drain inbox messages that arrived during step execution
                if let Some(ref mut inbox) = inbox {
                    for msg in inbox.drain() {
                        messages.push(std::sync::Arc::new(inbox_event_to_message(&msg)));
                    }
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

    // If lifecycle is Waiting (awaiting_tasks), signal it in the result
    // so clients/protocol layers can distinguish "done" from "waiting".
    let is_awaiting = lifecycle_now == Some(RunStatus::Waiting);
    let result_json = if is_awaiting {
        serde_json::json!({
            "response": response,
            "awaiting_tasks": true,
        })
    } else {
        serde_json::json!({"response": response})
    };

    sink.emit(AgentEvent::RunFinish {
        thread_id: run_identity.thread_id.clone(),
        run_id: run_identity.run_id.clone(),
        result: Some(result_json),
        termination: termination.clone(),
    })
    .await;

    Ok(AgentRunResult {
        response,
        termination,
        steps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    mod pending_work_tests {
        use super::*;
        use crate::agent::state::PendingWorkKey;

        fn store_with_loop_state() -> StateStore {
            let store = StateStore::new();
            store
                .install_plugin(crate::loop_runner::LoopStatePlugin)
                .unwrap();
            store
        }

        #[test]
        fn default_no_pending_work() {
            let store = store_with_loop_state();
            assert!(!has_pending_work(&store));
        }

        #[test]
        fn pending_work_set_true() {
            let store = store_with_loop_state();
            let mut batch = MutationBatch::new();
            batch.update::<PendingWorkKey>(true);
            store.commit(batch).unwrap();
            assert!(has_pending_work(&store));
        }

        #[test]
        fn pending_work_cleared() {
            let store = store_with_loop_state();
            let mut batch = MutationBatch::new();
            batch.update::<PendingWorkKey>(true);
            store.commit(batch).unwrap();
            assert!(has_pending_work(&store));

            let mut batch2 = MutationBatch::new();
            batch2.update::<PendingWorkKey>(false);
            store.commit(batch2).unwrap();
            assert!(!has_pending_work(&store));
        }
    }

    mod check_termination_tests {
        use super::*;
        use crate::agent::state::{RunLifecycle, RunLifecycleUpdate};
        use crate::loop_runner::checkpoint::check_termination;
        use awaken_contract::contract::lifecycle::{RunStatus, TerminationReason};

        fn store_with_lifecycle() -> StateStore {
            let store = StateStore::new();
            store
                .install_plugin(crate::loop_runner::LoopStatePlugin)
                .unwrap();
            store
        }

        #[test]
        fn running_returns_none() {
            let store = store_with_lifecycle();
            crate::loop_runner::commit_update::<RunLifecycle>(
                &store,
                RunLifecycleUpdate::Start {
                    run_id: "r1".into(),
                    updated_at: 100,
                },
            )
            .unwrap();
            assert!(check_termination(&store).is_none());
        }

        #[test]
        fn done_returns_termination_reason() {
            let store = store_with_lifecycle();
            crate::loop_runner::commit_update::<RunLifecycle>(
                &store,
                RunLifecycleUpdate::Start {
                    run_id: "r1".into(),
                    updated_at: 100,
                },
            )
            .unwrap();
            crate::loop_runner::commit_update::<RunLifecycle>(
                &store,
                RunLifecycleUpdate::Done {
                    done_reason: "natural".into(),
                    updated_at: 200,
                },
            )
            .unwrap();
            assert!(matches!(
                check_termination(&store),
                Some(TerminationReason::NaturalEnd)
            ));
        }

        #[test]
        fn waiting_suspended_returns_suspended() {
            let store = store_with_lifecycle();
            crate::loop_runner::commit_update::<RunLifecycle>(
                &store,
                RunLifecycleUpdate::Start {
                    run_id: "r1".into(),
                    updated_at: 100,
                },
            )
            .unwrap();
            crate::loop_runner::commit_update::<RunLifecycle>(
                &store,
                RunLifecycleUpdate::SetWaiting {
                    updated_at: 200,
                    pause_reason: "suspended".into(),
                },
            )
            .unwrap();
            assert!(matches!(
                check_termination(&store),
                Some(TerminationReason::Suspended)
            ));
        }

        #[test]
        fn waiting_awaiting_tasks_returns_none() {
            let store = store_with_lifecycle();
            crate::loop_runner::commit_update::<RunLifecycle>(
                &store,
                RunLifecycleUpdate::Start {
                    run_id: "r1".into(),
                    updated_at: 100,
                },
            )
            .unwrap();
            crate::loop_runner::commit_update::<RunLifecycle>(
                &store,
                RunLifecycleUpdate::SetWaiting {
                    updated_at: 200,
                    pause_reason: "awaiting_tasks".into(),
                },
            )
            .unwrap();
            // awaiting_tasks is handled by orchestrator, not check_termination
            assert!(
                check_termination(&store).is_none(),
                "awaiting_tasks should return None"
            );
        }
    }

    mod termination_sequence_tests {
        use super::*;
        use crate::agent::state::{RunLifecycle, RunLifecycleUpdate};
        use awaken_contract::contract::lifecycle::RunStatus;

        fn store_with_lifecycle() -> StateStore {
            let store = StateStore::new();
            store
                .install_plugin(crate::loop_runner::LoopStatePlugin)
                .unwrap();
            store
        }

        #[test]
        fn waiting_state_not_overwritten_by_done() {
            let store = store_with_lifecycle();
            crate::loop_runner::commit_update::<RunLifecycle>(
                &store,
                RunLifecycleUpdate::Start {
                    run_id: "r1".into(),
                    updated_at: 100,
                },
            )
            .unwrap();
            // Simulate orchestrator setting Waiting before break
            crate::loop_runner::commit_update::<RunLifecycle>(
                &store,
                RunLifecycleUpdate::SetWaiting {
                    updated_at: 200,
                    pause_reason: "awaiting_tasks".into(),
                },
            )
            .unwrap();

            // Termination sequence: should NOT overwrite Waiting with Done
            let lifecycle_now = store.read::<RunLifecycle>().map(|s| s.status);
            let termination = TerminationReason::NaturalEnd;
            let (target_status, _) = termination.to_run_status();
            if target_status.is_terminal() && lifecycle_now != Some(RunStatus::Waiting) {
                panic!("should not reach here — lifecycle is Waiting");
            }
            // Verify state is still Waiting
            let state = store.read::<RunLifecycle>().unwrap();
            assert_eq!(state.status, RunStatus::Waiting);
            assert_eq!(state.status_reason.as_deref(), Some("awaiting_tasks"));
        }
    }

    mod persist_checkpoint_tests {
        use super::*;
        use crate::agent::state::{RunLifecycle, RunLifecycleUpdate};
        use awaken_contract::contract::lifecycle::RunStatus;

        fn store_with_lifecycle() -> StateStore {
            let store = StateStore::new();
            store
                .install_plugin(crate::loop_runner::LoopStatePlugin)
                .unwrap();
            store
        }

        #[test]
        fn termination_code_stores_status_reason_for_waiting() {
            let store = store_with_lifecycle();
            crate::loop_runner::commit_update::<RunLifecycle>(
                &store,
                RunLifecycleUpdate::Start {
                    run_id: "r1".into(),
                    updated_at: 100,
                },
            )
            .unwrap();
            crate::loop_runner::commit_update::<RunLifecycle>(
                &store,
                RunLifecycleUpdate::SetWaiting {
                    updated_at: 200,
                    pause_reason: "awaiting_tasks".into(),
                },
            )
            .unwrap();

            let lifecycle = store.read::<RunLifecycle>().unwrap();
            assert_eq!(lifecycle.status_reason.as_deref(), Some("awaiting_tasks"));
        }

        #[test]
        fn termination_code_stores_status_reason_for_done() {
            let store = store_with_lifecycle();
            crate::loop_runner::commit_update::<RunLifecycle>(
                &store,
                RunLifecycleUpdate::Start {
                    run_id: "r1".into(),
                    updated_at: 100,
                },
            )
            .unwrap();
            crate::loop_runner::commit_update::<RunLifecycle>(
                &store,
                RunLifecycleUpdate::Done {
                    done_reason: "natural".into(),
                    updated_at: 200,
                },
            )
            .unwrap();

            let lifecycle = store.read::<RunLifecycle>().unwrap();
            assert_eq!(lifecycle.status_reason.as_deref(), Some("natural"));
        }
    }

    mod inbox_drain_tests {
        use crate::inbox::inbox_channel;

        #[test]
        fn drain_returns_empty_when_no_messages() {
            let (_tx, mut rx) = inbox_channel();
            let msgs = rx.drain();
            assert!(msgs.is_empty());
        }

        #[test]
        fn drain_returns_all_pending_messages() {
            let (tx, mut rx) = inbox_channel();
            tx.send(serde_json::json!({"event": "a"}));
            tx.send(serde_json::json!({"event": "b"}));
            tx.send(serde_json::json!({"event": "c"}));

            let msgs = rx.drain();
            assert_eq!(msgs.len(), 3);
            assert_eq!(msgs[0]["event"], "a");
            assert_eq!(msgs[2]["event"], "c");

            // Second drain is empty
            assert!(rx.drain().is_empty());
        }

        #[test]
        fn drain_after_sender_drop_returns_buffered() {
            let (tx, mut rx) = inbox_channel();
            tx.send(serde_json::json!("buffered"));
            drop(tx);

            let msgs = rx.drain();
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0], "buffered");
        }

        #[test]
        fn inbox_events_injected_as_internal_user_messages() {
            let (tx, mut rx) = inbox_channel();
            tx.send(
                serde_json::json!({"kind": "custom", "event_type": "progress", "task_id": "bg_0"}),
            );

            let msgs = rx.drain();
            for msg in &msgs {
                let m = super::inbox_event_to_message(msg);
                assert_eq!(m.role, awaken_contract::contract::message::Role::User);
                assert_eq!(
                    m.visibility,
                    awaken_contract::contract::message::Visibility::Internal
                );
            }
        }

        #[test]
        fn inbox_events_wrapped_in_background_task_event_tag() {
            let event = serde_json::json!({
                "kind": "custom",
                "task_id": "bg_42",
                "event_type": "data_ready",
                "payload": {"rows": 100}
            });
            let m = super::inbox_event_to_message(&event);
            let text = m.text();
            assert!(
                text.contains("<background-task-event"),
                "should have opening tag: {text}"
            );
            assert!(
                text.contains("</background-task-event>"),
                "should have closing tag: {text}"
            );
            assert!(
                text.contains("kind=\"custom\""),
                "tag should contain kind: {text}"
            );
            assert!(
                text.contains("task_id=\"bg_42\""),
                "tag should contain task_id: {text}"
            );
        }
    }
}
