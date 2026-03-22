//! Main agent loop orchestration — `run_agent_loop_controlled`.

use std::sync::Arc;

use crate::contract::event::AgentEvent;
use crate::contract::event_sink::EventSink;
use crate::contract::executor::InferenceRequest;
use crate::contract::identity::RunIdentity;
use crate::contract::inference::{InferenceOverride, LLMResponse};
use crate::contract::lifecycle::TerminationReason;
use crate::contract::message::{Message, Role, gen_message_id};
use crate::contract::storage::ThreadRunStore;
use crate::contract::suspension::{ToolCallOutcome, ToolCallResume, ToolCallStatus};
use crate::contract::tool::ToolCallContext;
use crate::model::Phase;
use crate::runtime::{AgentResolver, CancellationToken, PhaseContext, PhaseRuntime, ResolvedAgent};
use crate::state::StateCommand;
use futures::channel::mpsc::UnboundedReceiver;

use super::super::state::{RunLifecycle, RunLifecycleUpdate, ToolCallStates, ToolCallStatesUpdate};
use super::actions::{
    apply_context_messages, take_accumulated_context_messages, take_accumulated_overrides,
    take_and_apply_tool_filters,
};
use super::checkpoint::{
    check_termination, complete_step, emit_state_snapshot, persist_checkpoint,
};
use super::inference::{compact_with_llm, execute_streaming};
use super::resume::{WaitOutcome, detect_and_replay_resume, wait_for_resume_or_cancel};
use super::{AgentLoopError, AgentRunResult, commit_update, now_ms, tool_result_to_content};

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
    let mut messages: Vec<Arc<Message>> = initial_messages.into_iter().map(Arc::new).collect();
    let run_overrides = initial_overrides;
    let mut decision_rx = decision_rx;
    let run_created_at = now_ms();
    let mut total_input_tokens: u64 = 0;
    let mut total_output_tokens: u64 = 0;

    // Resolve initial agent
    let ResolvedAgent {
        config: mut agent,
        mut env,
    } = resolver
        .resolve(initial_agent_id)
        .map_err(AgentLoopError::PhaseError)?;

    // Trim to latest compaction boundary — skip already-summarized history
    if agent.context_policy.is_some() {
        super::super::context::trim_to_compaction_boundary(&mut messages);
    }

    // --- State-driven resume detection ---
    // If any tool calls are in Resuming state, replay them before starting the loop.
    detect_and_replay_resume(&agent, store, &run_identity, &mut messages).await?;

    let mut steps: usize = 0;

    // Helper to build PhaseContext with current state
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

    let termination = loop {
        steps += 1;
        tracing::info!(step = steps, "step_start");

        // --- Cancellation check ---
        if cancellation_token
            .as_ref()
            .is_some_and(|t| t.is_cancelled())
        {
            commit_update::<RunLifecycle>(
                store,
                RunLifecycleUpdate::Done {
                    done_reason: "cancelled".into(),
                    updated_at: now_ms(),
                },
            )?;
            break TerminationReason::Cancelled;
        }

        // --- Handoff: check ActiveAgentKey for agent switch ---
        if let Some(Some(active_id)) = store.read::<crate::contract::profile::ActiveAgentKey>() {
            if active_id != agent.id {
                if let Ok(resolved) = resolver.resolve(&active_id) {
                    agent = resolved.config;
                    env = resolved.env;
                }
            }
        }

        sink.emit(AgentEvent::StepStart {
            message_id: gen_message_id(),
        })
        .await;

        // Clear tool call states from previous step
        commit_update::<ToolCallStates>(store, ToolCallStatesUpdate::Clear)?;

        runtime
            .run_phase_with_context(&env, make_ctx(Phase::StepStart, &messages, &run_identity))
            .await?;
        if let Some(reason) = check_termination(store) {
            break reason;
        }

        runtime
            .run_phase_with_context(
                &env,
                make_ctx(Phase::BeforeInference, &messages, &run_identity),
            )
            .await?;
        if let Some(reason) = check_termination(store) {
            break reason;
        }

        // LLM compaction: if token count exceeds autocompact threshold,
        // call LLM to generate summary and replace old messages.
        if let Some(ref policy) = agent.context_policy {
            if let Some(threshold) = policy.autocompact_threshold {
                let token_est = crate::contract::transform::estimate_tokens_arc(&messages);
                if token_est >= threshold {
                    compact_with_llm(&agent, &mut messages, policy).await?;
                }
            }
        }

        // Read accumulated results from action handlers (populated during run_phase above)
        let mut overrides = run_overrides.clone();
        if let Some(runtime_overrides) = take_accumulated_overrides(store)? {
            if let Some(merged) = overrides.as_mut() {
                merged.merge(runtime_overrides);
            } else {
                overrides = Some(runtime_overrides);
            }
        }
        let context_msgs = take_accumulated_context_messages(store)?;

        // Build message list: system prompt + conversation history
        let has_system_prompt = !agent.system_prompt.is_empty();
        let mut request_messages: Vec<Message> = Vec::new();
        if has_system_prompt {
            request_messages.push(Message::system(&agent.system_prompt));
        }
        request_messages.extend(messages.iter().map(|m| (**m).clone()));

        // Apply context messages at their target positions
        if !context_msgs.is_empty() {
            apply_context_messages(&mut request_messages, context_msgs, has_system_prompt);
        }

        // Apply accumulated tool filters from action handlers
        let mut tools = agent.tool_descriptors();
        take_and_apply_tool_filters(store, &mut tools)?;
        let request_messages = crate::contract::transform::apply_transforms(
            request_messages,
            &tools,
            &env.request_transforms,
        );

        let start = std::time::Instant::now();
        let enable_prompt_cache = agent
            .context_policy
            .as_ref()
            .map_or(false, |p| p.enable_prompt_cache);
        let request = InferenceRequest {
            model: agent.model.clone(),
            messages: request_messages,
            tools,
            system: vec![],
            overrides,
            enable_prompt_cache,
        };

        let mut stream_result = execute_streaming(
            &agent,
            request,
            sink,
            cancellation_token.as_ref(),
            &mut total_input_tokens,
            &mut total_output_tokens,
        )
        .await?;

        // --- Truncation recovery ---
        // When the LLM hits MaxTokens mid-response with incomplete tool calls,
        // inject a continuation prompt and re-invoke inference up to
        // `max_continuation_retries` times.
        if stream_result.needs_truncation_recovery() && agent.max_continuation_retries > 0 {
            let mut continuation_attempts = 0;
            while stream_result.needs_truncation_recovery()
                && continuation_attempts < agent.max_continuation_retries
            {
                continuation_attempts += 1;

                // Add the partial assistant message to the conversation
                let partial_text = stream_result.text();
                messages.push(Arc::new(Message::assistant(&partial_text)));

                // Add a continuation user message
                messages.push(Arc::new(Message::user(
                    "Please continue from where you left off.",
                )));

                // Rebuild request with updated messages
                let has_sys = !agent.system_prompt.is_empty();
                let mut cont_messages: Vec<Message> = Vec::new();
                if has_sys {
                    cont_messages.push(Message::system(&agent.system_prompt));
                }
                cont_messages.extend(messages.iter().map(|m| (**m).clone()));
                let cont_messages = crate::contract::transform::apply_transforms(
                    cont_messages,
                    &agent.tool_descriptors(),
                    &env.request_transforms,
                );

                let cont_request = InferenceRequest {
                    model: agent.model.clone(),
                    messages: cont_messages,
                    tools: agent.tool_descriptors(),
                    system: vec![],
                    overrides: run_overrides.clone(),
                    enable_prompt_cache: false,
                };

                stream_result = execute_streaming(
                    &agent,
                    cont_request,
                    sink,
                    cancellation_token.as_ref(),
                    &mut total_input_tokens,
                    &mut total_output_tokens,
                )
                .await?;
            }
        }

        let duration_ms = start.elapsed().as_millis() as u64;
        tracing::info!(
            model = %agent.model,
            input_tokens = total_input_tokens,
            output_tokens = total_output_tokens,
            duration_ms,
            "inference_complete"
        );

        // Check if cancellation occurred mid-stream
        if cancellation_token
            .as_ref()
            .is_some_and(|t| t.is_cancelled())
        {
            sink.emit(AgentEvent::InferenceComplete {
                model: agent.model.clone(),
                usage: stream_result.usage.clone(),
                duration_ms,
            })
            .await;
            commit_update::<RunLifecycle>(
                store,
                RunLifecycleUpdate::Done {
                    done_reason: "cancelled".into(),
                    updated_at: now_ms(),
                },
            )?;
            break TerminationReason::Cancelled;
        }
        sink.emit(AgentEvent::InferenceComplete {
            model: agent.model.clone(),
            usage: stream_result.usage.clone(),
            duration_ms,
        })
        .await;

        let llm_response = LLMResponse::success(stream_result.clone());
        let after_inf_ctx = make_ctx(Phase::AfterInference, &messages, &run_identity)
            .with_llm_response(llm_response);
        runtime.run_phase_with_context(&env, after_inf_ctx).await?;
        if let Some(reason) = check_termination(store) {
            break reason;
        }

        if !stream_result.needs_tools() {
            messages.push(Arc::new(Message::assistant(&stream_result.text())));
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
            break TerminationReason::NaturalEnd;
        }

        // Add assistant message with tool calls
        messages.push(Arc::new(Message::assistant_with_tool_calls(
            &stream_result.text(),
            stream_result.tool_calls.clone(),
        )));

        // Check tool permissions and execute allowed tool calls.
        //
        // Permission check runs per tool call before execution:
        // - Allow → execute the tool
        // - Deny → skip execution, add error message
        // - Suspend → skip execution, mark as suspended
        let mut allowed_calls = Vec::new();
        let mut suspended = false;
        let mut blocked: Option<String> = None;
        let mut tool_commands = Vec::new();

        for call in &stream_result.tool_calls {
            let perm_ctx = make_ctx(Phase::BeforeToolExecute, &messages, &run_identity)
                .with_tool_info(&call.name, &call.id, Some(call.arguments.clone()));
            let perm_result = runtime.check_tool_permission(&env, &perm_ctx).await?;

            match perm_result {
                crate::runtime::ToolPermissionResult::Allow => {
                    allowed_calls.push(call.clone());
                }
                crate::runtime::ToolPermissionResult::Deny { reason, message } => {
                    let mut lifecycle_cmd = StateCommand::new();
                    lifecycle_cmd.update::<ToolCallStates>(ToolCallStatesUpdate::Upsert {
                        call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        status: ToolCallStatus::Failed,
                        updated_at: now_ms(),
                    });
                    tool_commands.push(lifecycle_cmd);
                    let tool_msg =
                        message.unwrap_or_else(|| format!("Permission denied: {reason}"));
                    messages.push(Arc::new(Message::tool(&call.id, tool_msg)));
                }
                crate::runtime::ToolPermissionResult::Block { reason } => {
                    let mut lifecycle_cmd = StateCommand::new();
                    lifecycle_cmd.update::<ToolCallStates>(ToolCallStatesUpdate::Upsert {
                        call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        status: ToolCallStatus::Failed,
                        updated_at: now_ms(),
                    });
                    tool_commands.push(lifecycle_cmd);
                    blocked = Some(reason);
                    break;
                }
                crate::runtime::ToolPermissionResult::Suspend => {
                    let mut lifecycle_cmd = StateCommand::new();
                    lifecycle_cmd.update::<ToolCallStates>(ToolCallStatesUpdate::Upsert {
                        call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        status: ToolCallStatus::Suspended,
                        updated_at: now_ms(),
                    });
                    tool_commands.push(lifecycle_cmd);
                    messages.push(Arc::new(Message::tool(
                        &call.id,
                        "Tool call suspended: awaiting approval".to_string(),
                    )));
                    suspended = true;
                }
            }
        }

        // If a tool call was blocked, submit state updates and terminate the run.
        if let Some(block_reason) = blocked {
            if !tool_commands.is_empty() {
                let merged = store.merge_all_commands(tool_commands)?;
                runtime.submit_command(&env, merged).await?;
            }
            commit_update::<RunLifecycle>(
                store,
                RunLifecycleUpdate::Done {
                    done_reason: format!("blocked:{block_reason}"),
                    updated_at: now_ms(),
                },
            )?;
            break TerminationReason::Blocked(block_reason);
        }

        // Execute allowed tool calls via ToolExecutor
        let activity_buffer = Arc::new(crate::contract::event_sink::VecEventSink::new());
        let tool_ctx = ToolCallContext {
            call_id: String::new(), // filled per-call by executor
            run_identity: run_identity.clone(),
            agent_spec: make_ctx(Phase::BeforeToolExecute, &messages, &run_identity).agent_spec,
            snapshot: store.snapshot(),
            activity_sink: Some(
                activity_buffer.clone() as Arc<dyn crate::contract::event_sink::EventSink>
            ),
        };
        let exec_results = agent
            .tool_executor
            .execute(&agent.tools, &allowed_calls, &tool_ctx)
            .await
            .map_err(|e| AgentLoopError::InferenceFailed(e.to_string()))?;
        // Flush buffered activity events to the real sink
        for activity_event in activity_buffer.take() {
            sink.emit(activity_event).await;
        }

        // Process tool results: collect phase commands, merge, commit once.
        for exec_result in &exec_results {
            let call = &exec_result.call;
            let tool_result = &exec_result.result;

            sink.emit(AgentEvent::ToolCallStart {
                id: call.id.clone(),
                name: call.name.clone(),
            })
            .await;

            // Collect BeforeToolExecute hook commands (no commit)
            let before_ctx = make_ctx(Phase::BeforeToolExecute, &messages, &run_identity)
                .with_tool_info(&call.name, &call.id, Some(call.arguments.clone()));
            let before_cmd = runtime.collect_commands(&env, before_ctx).await?;
            if !before_cmd.is_empty() {
                tool_commands.push(before_cmd);
            }

            // Build tool call state transitions as a command
            let terminal_status = match exec_result.outcome {
                ToolCallOutcome::Suspended => ToolCallStatus::Suspended,
                ToolCallOutcome::Succeeded => ToolCallStatus::Succeeded,
                ToolCallOutcome::Failed => ToolCallStatus::Failed,
            };
            let mut lifecycle_cmd = StateCommand::new();
            lifecycle_cmd.update::<ToolCallStates>(ToolCallStatesUpdate::Upsert {
                call_id: call.id.clone(),
                tool_name: call.name.clone(),
                arguments: call.arguments.clone(),
                status: ToolCallStatus::Running,
                updated_at: now_ms(),
            });
            lifecycle_cmd.update::<ToolCallStates>(ToolCallStatesUpdate::Upsert {
                call_id: call.id.clone(),
                tool_name: call.name.clone(),
                arguments: call.arguments.clone(),
                status: terminal_status,
                updated_at: now_ms(),
            });
            tool_commands.push(lifecycle_cmd);

            tracing::info!(
                tool_name = %call.name,
                call_id = %call.id,
                outcome = ?exec_result.outcome,
                "tool_call_done"
            );

            sink.emit(AgentEvent::ToolCallDone {
                id: call.id.clone(),
                message_id: String::new(),
                result: tool_result.clone(),
                outcome: exec_result.outcome,
            })
            .await;

            // Collect AfterToolExecute hook commands (no commit)
            let after_ctx = make_ctx(Phase::AfterToolExecute, &messages, &run_identity)
                .with_tool_info(&call.name, &call.id, Some(call.arguments.clone()))
                .with_tool_result(tool_result.clone());
            let after_cmd = runtime.collect_commands(&env, after_ctx).await?;
            if !after_cmd.is_empty() {
                tool_commands.push(after_cmd);
            }

            let tool_content = tool_result_to_content(tool_result);
            messages.push(Arc::new(Message::tool(&call.id, tool_content)));

            if exec_result.outcome == ToolCallOutcome::Suspended {
                suspended = true;
            }
        }

        // Merge all tool call commands and submit once
        if !tool_commands.is_empty() {
            let merged = store.merge_all_commands(tool_commands)?;
            runtime.submit_command(&env, merged).await?;
        }

        // Check termination after tool execution
        if let Some(reason) = check_termination(store) {
            break reason;
        }

        if suspended {
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
    };

    tracing::warn!(reason = ?termination, "run_terminated");

    // --- Run lifecycle: Done (unless Suspended → Waiting, not Done) ---
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
