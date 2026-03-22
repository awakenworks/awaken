//! Minimal sequential agent loop driven by state machines.
//!
//! Run lifecycle: RunLifecycle (Running → StepCompleted → Done/Waiting)
//! Tool call lifecycle: ToolCallStates (New → Running → Succeeded/Failed/Suspended)

use std::sync::Arc;

use crate::contract::event::AgentEvent;
use crate::contract::event_sink::EventSink;
use crate::contract::executor::InferenceRequest;
use crate::contract::identity::RunIdentity;
use crate::contract::inference::{InferenceOverride, LLMResponse};
use crate::contract::lifecycle::{RunStatus, TerminationReason};
use crate::contract::message::{Message, Role, ToolCall, gen_message_id};
use crate::contract::storage::{RunRecord, ThreadRunStore};
use crate::contract::suspension::{
    ResumeDecisionAction, ToolCallOutcome, ToolCallResume, ToolCallResumeMode, ToolCallStatus,
};
use crate::contract::tool::{ToolCallContext, ToolResult};
use crate::error::StateError;
use crate::model::{PendingScheduledActions, Phase, ScheduledActionQueueUpdate};
use crate::runtime::{
    AgentResolver, CancellationToken, ExecutionEnv, PhaseContext, PhaseRuntime, ResolvedAgent,
};
use crate::state::{MutationBatch, StateCommand};
use futures::StreamExt;
use futures::channel::mpsc::UnboundedReceiver;

use super::config::AgentConfig;
use super::state::{
    ContextThrottleState, RunLifecycle, RunLifecycleUpdate, ToolCallStates, ToolCallStatesUpdate,
};

/// Plugin that registers the core state keys required by the loop runner.
///
/// Must be installed on the `StateStore` before running the loop.
pub struct LoopStatePlugin;

impl crate::plugins::Plugin for LoopStatePlugin {
    fn descriptor(&self) -> crate::plugins::PluginDescriptor {
        crate::plugins::PluginDescriptor {
            name: "__loop_state",
        }
    }

    fn register(
        &self,
        r: &mut crate::plugins::PluginRegistrar,
    ) -> Result<(), crate::error::StateError> {
        r.register_key::<RunLifecycle>(crate::state::StateKeyOptions::default())?;
        r.register_key::<ToolCallStates>(crate::state::StateKeyOptions::default())?;
        r.register_key::<ContextThrottleState>(crate::state::StateKeyOptions::default())?;
        Ok(())
    }
}

/// Errors from the agent loop.
#[derive(Debug, thiserror::Error)]
pub enum AgentLoopError {
    #[error("inference failed: {0}")]
    InferenceFailed(String),
    #[error("storage failed: {0}")]
    StorageError(String),
    #[error("phase error: {0}")]
    PhaseError(#[from] crate::error::StateError),
    #[error("invalid resume: {0}")]
    InvalidResume(String),
}

/// Result of running the agent loop.
#[derive(Debug)]
pub struct AgentRunResult {
    pub response: String,
    pub termination: TerminationReason,
    pub steps: usize,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Build an execution environment for the agent loop.
///
/// Adds internal plugins (stop conditions, default permission) and registers
/// built-in request transforms (context truncation when a policy is provided).
/// Build an execution environment. Prefer `AgentRuntime::run()` for production use.
pub fn build_agent_env(
    plugins: &[Arc<dyn crate::plugins::Plugin>],
    agent: &super::config::AgentConfig,
) -> Result<ExecutionEnv, StateError> {
    use super::context::ContextTransform;
    use super::stop_conditions::MaxRoundsPlugin;
    use super::tool_permission::AllowAllToolsPlugin;

    let mut all_plugins: Vec<Arc<dyn crate::plugins::Plugin>> = plugins.to_vec();
    all_plugins.push(Arc::new(MaxRoundsPlugin::new(agent.max_rounds)));
    all_plugins.push(Arc::new(AllowAllToolsPlugin));

    let mut env = ExecutionEnv::from_plugins(&all_plugins)?;
    env.register_loop_consumed_action::<super::state::SetInferenceOverride>();
    env.register_loop_consumed_action::<super::state::AddContextMessage>();
    env.register_loop_consumed_action::<super::state::ExcludeTool>();
    env.register_loop_consumed_action::<super::state::IncludeOnlyTools>();

    // Register built-in context truncation transform when policy is set
    if let Some(ref policy) = agent.context_policy {
        env.request_transforms
            .push(Arc::new(ContextTransform::new(policy.clone())));
    }

    Ok(env)
}

/// Agent loop implementation. Prefer `AgentRuntime::run()` for production use.
///
/// Handles both fresh runs and resumed runs (state-driven detection).
/// Supports dynamic agent handoff via `ActiveAgentKey` re-resolve at step boundaries.
/// Cooperative cancellation via `CancellationToken`.
pub async fn run_agent_loop(
    resolver: &dyn AgentResolver,
    initial_agent_id: &str,
    runtime: &PhaseRuntime,
    sink: &dyn EventSink,
    checkpoint_store: Option<&dyn ThreadRunStore>,
    initial_messages: Vec<Message>,
    run_identity: RunIdentity,
    cancellation_token: Option<CancellationToken>,
) -> Result<AgentRunResult, AgentLoopError> {
    run_agent_loop_controlled(
        resolver,
        initial_agent_id,
        runtime,
        sink,
        checkpoint_store,
        initial_messages,
        run_identity,
        cancellation_token,
        None,
        None,
    )
    .await
}

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
        super::context::trim_to_compaction_boundary(&mut messages);
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

        // Consume loop actions from PendingScheduledActions before building request
        let mut overrides = run_overrides.clone();
        if let Some(runtime_overrides) = consume_inference_overrides(store)? {
            if let Some(merged) = overrides.as_mut() {
                merged.merge(runtime_overrides);
            } else {
                overrides = Some(runtime_overrides);
            }
        }
        let context_msgs = consume_context_messages(store, steps)?;

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

        // Apply request transforms (e.g., hard truncation to token budget)
        let mut tools = agent.tool_descriptors();
        apply_tool_filter_actions(store, &mut tools)?;
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
            profile: make_ctx(Phase::BeforeToolExecute, &messages, &run_identity).profile,
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

enum WaitOutcome {
    Resumed,
    Cancelled,
    NoDecisionChannel,
}

/// Prepare tool call states for resume. Call before `run_agent_loop`.
///
/// Writes resume decisions into `ToolCallStates` so the loop detects them at startup.
pub fn prepare_resume(
    store: &crate::state::StateStore,
    decisions: Vec<(String, ToolCallResume)>,
    resume_mode: ToolCallResumeMode,
) -> Result<(), StateError> {
    let tool_call_states = store.read::<ToolCallStates>().unwrap_or_default();
    for (call_id, decision) in decisions {
        let call_state =
            tool_call_states
                .calls
                .get(&call_id)
                .ok_or_else(|| StateError::UnknownKey {
                    key: format!("tool call {call_id} not found"),
                })?;
        // Write resume payload into state
        commit_update::<ToolCallStates>(
            store,
            ToolCallStatesUpdate::Upsert {
                call_id: call_id.clone(),
                tool_name: call_state.tool_name.clone(),
                arguments: match (&resume_mode, &decision.action) {
                    (ToolCallResumeMode::PassDecisionToTool, ResumeDecisionAction::Resume) => {
                        decision.result.clone()
                    }
                    _ => call_state.arguments.clone(),
                },
                status: match decision.action {
                    ResumeDecisionAction::Resume => ToolCallStatus::Resuming,
                    ResumeDecisionAction::Cancel => ToolCallStatus::Cancelled,
                },
                updated_at: now_ms(),
            },
        )?;
    }
    Ok(())
}

/// Detect Resuming tool calls in state and replay them.
///
/// Called at loop startup. If any tool calls are in Resuming state,
/// execute them and append results to messages.
async fn detect_and_replay_resume(
    agent: &AgentConfig,
    store: &crate::state::StateStore,
    run_identity: &RunIdentity,
    messages: &mut Vec<Arc<Message>>,
) -> Result<(), AgentLoopError> {
    let tool_call_states = store.read::<ToolCallStates>().unwrap_or_default();

    // Find all Resuming tool calls
    let resuming: Vec<_> = tool_call_states
        .calls
        .iter()
        .filter(|(_, state)| state.status == ToolCallStatus::Resuming)
        .collect();

    if resuming.is_empty() {
        return Ok(());
    }

    let resume_tool_ctx = ToolCallContext {
        call_id: String::new(),
        run_identity: run_identity.clone(),
        profile: std::sync::Arc::new(crate::contract::profile::AgentProfile::default()),
        snapshot: store.snapshot(),
        activity_sink: None,
    };

    for (call_id, call_state) in resuming {
        // Re-execute with the arguments stored in state (may be original or decision payload)
        let call = ToolCall::new(call_id, &call_state.tool_name, call_state.arguments.clone());
        let mut tool_ctx = resume_tool_ctx.clone();
        tool_ctx.call_id = call_id.to_string();
        let result = execute_single_tool(agent, &call, &tool_ctx).await;

        let status = if result.is_success() {
            ToolCallStatus::Succeeded
        } else {
            ToolCallStatus::Failed
        };
        commit_update::<ToolCallStates>(
            store,
            ToolCallStatesUpdate::Upsert {
                call_id: call_id.clone(),
                tool_name: call_state.tool_name.clone(),
                arguments: call_state.arguments.clone(),
                status,
                updated_at: now_ms(),
            },
        )?;

        messages.push(Arc::new(Message::tool(
            call_id,
            tool_result_to_content(&result),
        )));
    }

    Ok(())
}

async fn wait_for_resume_or_cancel(
    decision_rx: Option<&mut UnboundedReceiver<(String, ToolCallResume)>>,
    cancellation_token: Option<&CancellationToken>,
    store: &crate::state::StateStore,
    agent: &AgentConfig,
    run_identity: &RunIdentity,
    messages: &mut Vec<Arc<Message>>,
) -> Result<WaitOutcome, AgentLoopError> {
    let Some(rx) = decision_rx else {
        return Ok(WaitOutcome::NoDecisionChannel);
    };

    loop {
        if cancellation_token.is_some_and(|t| t.is_cancelled()) {
            return Ok(WaitOutcome::Cancelled);
        }

        let Some(first) = rx.next().await else {
            return Ok(WaitOutcome::NoDecisionChannel);
        };
        let mut decisions = vec![first];
        loop {
            match rx.try_recv() {
                Ok(v) => decisions.push(v),
                Err(_) => break,
            }
        }

        prepare_resume(store, decisions, ToolCallResumeMode::ReplayToolCall)?;
        detect_and_replay_resume(agent, store, run_identity, messages).await?;
        if !has_suspended_calls(store) {
            return Ok(WaitOutcome::Resumed);
        }
    }
}

fn has_suspended_calls(store: &crate::state::StateStore) -> bool {
    store
        .read::<ToolCallStates>()
        .map(|s| {
            s.calls
                .values()
                .any(|v| v.status == ToolCallStatus::Suspended)
        })
        .unwrap_or(false)
}

// -- Helpers --

/// Execute a single tool, returning ToolResult (never crashes the loop).
async fn execute_single_tool(
    agent: &AgentConfig,
    call: &ToolCall,
    ctx: &ToolCallContext,
) -> ToolResult {
    let Some(tool) = agent.tools.get(&call.name) else {
        return ToolResult::error(&call.name, format!("tool '{}' not found", call.name));
    };

    if let Err(e) = tool.validate_args(&call.arguments) {
        return ToolResult::error(&call.name, e.to_string());
    }

    match tool.execute(call.arguments.clone(), ctx).await {
        Ok(result) => result,
        Err(e) => ToolResult::error(&call.name, e.to_string()),
    }
}

async fn complete_step(
    store: &crate::state::StateStore,
    runtime: &PhaseRuntime,
    env: &ExecutionEnv,
    sink: &dyn EventSink,
    checkpoint_store: Option<&dyn ThreadRunStore>,
    messages: &[Arc<Message>],
    run_identity: &RunIdentity,
    run_created_at: u64,
    total_input_tokens: u64,
    total_output_tokens: u64,
) -> Result<(), AgentLoopError> {
    commit_update::<RunLifecycle>(
        store,
        RunLifecycleUpdate::StepCompleted {
            updated_at: now_ms(),
        },
    )?;
    let ctx = PhaseContext::new(Phase::StepEnd, store.snapshot())
        .with_run_identity(run_identity.clone())
        .with_messages(messages.to_vec());
    runtime.run_phase_with_context(&env, ctx).await?;

    persist_checkpoint(
        store,
        checkpoint_store,
        messages,
        run_identity,
        run_created_at,
        total_input_tokens,
        total_output_tokens,
    )
    .await?;

    emit_state_snapshot(store, sink).await;

    sink.emit(AgentEvent::StepEnd).await;
    Ok(())
}

async fn persist_checkpoint(
    store: &crate::state::StateStore,
    checkpoint_store: Option<&dyn ThreadRunStore>,
    messages: &[Arc<Message>],
    run_identity: &RunIdentity,
    run_created_at: u64,
    total_input_tokens: u64,
    total_output_tokens: u64,
) -> Result<(), AgentLoopError> {
    let Some(storage) = checkpoint_store else {
        return Ok(());
    };

    let lifecycle = store.read::<RunLifecycle>().unwrap_or_default();
    let state = store
        .export_persisted()
        .map_err(AgentLoopError::PhaseError)?;
    let record = RunRecord {
        run_id: run_identity.run_id.clone(),
        thread_id: run_identity.thread_id.clone(),
        agent_id: run_identity.agent_id.clone(),
        parent_run_id: run_identity.parent_run_id.clone(),
        status: lifecycle.status,
        termination_code: lifecycle.done_reason.clone(),
        created_at: run_created_at / 1000,
        updated_at: if lifecycle.updated_at == 0 {
            run_created_at / 1000
        } else {
            lifecycle.updated_at / 1000
        },
        steps: lifecycle.step_count as usize,
        input_tokens: total_input_tokens,
        output_tokens: total_output_tokens,
        state: Some(state),
    };
    let msgs: Vec<Message> = messages.iter().map(|m| (**m).clone()).collect();
    storage
        .checkpoint(&run_identity.thread_id, &msgs, &record)
        .await
        .map_err(|e| AgentLoopError::StorageError(e.to_string()))
}

fn commit_update<S: crate::state::StateKey>(
    store: &crate::state::StateStore,
    update: S::Update,
) -> Result<(), crate::error::StateError> {
    let mut patch = MutationBatch::new();
    patch.update::<S>(update);
    store.commit(patch)?;
    Ok(())
}

/// Emit a `StateSnapshot` event with the current persisted state.
async fn emit_state_snapshot(store: &crate::state::StateStore, sink: &dyn EventSink) {
    match store.export_persisted() {
        Ok(persisted) => {
            if let Ok(snapshot) = serde_json::to_value(persisted) {
                sink.emit(AgentEvent::StateSnapshot { snapshot }).await;
            }
        }
        Err(_) => {
            // State export failed; skip snapshot emission rather than breaking the loop.
        }
    }
}

/// Check if the run lifecycle has left Running state.
///
/// Returns `Some(TerminationReason)` if the run should stop.
fn check_termination(store: &crate::state::StateStore) -> Option<TerminationReason> {
    let lifecycle = store.read::<RunLifecycle>()?;
    match lifecycle.status {
        RunStatus::Running => None,
        RunStatus::Done => {
            let reason = lifecycle.done_reason.as_deref().unwrap_or("unknown");
            Some(TerminationReason::from_done_reason(reason))
        }
        RunStatus::Waiting => Some(TerminationReason::Suspended),
    }
}

/// Consume `SetInferenceOverride` actions from the pending queue.
///
/// Loop-consumed action: no handler registered, EXECUTE skips it.
/// Multiple overrides are merged with last-wins semantics per field.
fn consume_inference_overrides(
    store: &crate::state::StateStore,
) -> Result<Option<crate::contract::inference::InferenceOverride>, crate::error::StateError> {
    use super::state::SetInferenceOverride;
    use crate::model::ScheduledActionSpec;

    let pending = store.read::<PendingScheduledActions>().unwrap_or_default();

    let matching: Vec<_> = pending
        .iter()
        .filter(|e| e.action.key == SetInferenceOverride::KEY)
        .collect();

    if matching.is_empty() {
        return Ok(None);
    }

    let mut merged = crate::contract::inference::InferenceOverride::default();
    let mut ids = Vec::new();
    for envelope in matching {
        let payload = SetInferenceOverride::decode_payload(envelope.action.payload.clone())?;
        merged.merge(payload);
        ids.push(envelope.id);
    }

    // Dequeue consumed actions
    let mut patch = MutationBatch::new();
    for id in ids {
        patch.update::<PendingScheduledActions>(ScheduledActionQueueUpdate::Remove { id });
    }
    store.commit(patch)?;

    if merged.is_empty() {
        Ok(None)
    } else {
        Ok(Some(merged))
    }
}

/// Consume `AddContextMessage` actions from the pending queue with throttle filtering.
///
/// Reads `ContextThrottleState` to enforce cooldown rules:
/// - `cooldown_turns == 0`: always inject
/// - Content hash changed since last injection: inject
/// - Steps since last injection >= cooldown_turns: inject
/// - Otherwise: skip (throttled)
///
/// All matching actions are dequeued regardless of throttle outcome.
fn consume_context_messages(
    store: &crate::state::StateStore,
    current_step: usize,
) -> Result<Vec<crate::contract::context_message::ContextMessage>, crate::error::StateError> {
    use super::state::{AddContextMessage, ContextThrottleState, ContextThrottleUpdate};
    use crate::model::ScheduledActionSpec;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let pending = store.read::<PendingScheduledActions>().unwrap_or_default();

    let matching: Vec<_> = pending
        .iter()
        .filter(|e| e.action.key == AddContextMessage::KEY)
        .collect();

    if matching.is_empty() {
        return Ok(vec![]);
    }

    // Decode all payloads and collect action IDs
    let mut candidates = Vec::new();
    let mut action_ids = Vec::new();
    for envelope in matching {
        let payload = AddContextMessage::decode_payload(envelope.action.payload.clone())?;
        candidates.push(payload);
        action_ids.push(envelope.id);
    }

    // Dequeue all matching actions (consumed regardless of throttle)
    let mut patch = MutationBatch::new();
    for id in &action_ids {
        patch.update::<PendingScheduledActions>(ScheduledActionQueueUpdate::Remove { id: *id });
    }
    store.commit(patch)?;

    // Apply throttle filtering
    let throttle_state = store.read::<ContextThrottleState>().unwrap_or_default();

    let mut accepted = Vec::new();
    let mut throttle_updates = Vec::new();

    for msg in candidates {
        let content_hash = {
            let mut hasher = DefaultHasher::new();
            // Hash the serialized content for change detection
            if let Ok(json) = serde_json::to_string(&msg.content) {
                json.hash(&mut hasher);
            }
            hasher.finish()
        };

        let should_inject = if msg.cooldown_turns == 0 {
            true
        } else {
            match throttle_state.entries.get(&msg.key) {
                None => true,
                Some(entry) => {
                    entry.content_hash != content_hash
                        || current_step.saturating_sub(entry.last_step)
                            >= msg.cooldown_turns as usize
                }
            }
        };

        if should_inject {
            throttle_updates.push(ContextThrottleUpdate::Injected {
                key: msg.key.clone(),
                step: current_step,
                content_hash,
            });
            accepted.push(msg);
        }
    }

    // Update throttle state
    if !throttle_updates.is_empty() {
        let mut patch = MutationBatch::new();
        for update in throttle_updates {
            patch.update::<ContextThrottleState>(update);
        }
        store.commit(patch)?;
    }

    Ok(accepted)
}

/// Consume `ExcludeTool` and `IncludeOnlyTools` actions, then filter tool descriptors.
///
/// - All `ExcludeTool` payloads are collected; matching tool IDs are removed.
/// - If any `IncludeOnlyTools` payloads exist, their union forms an allow-list;
///   only tools whose IDs appear in the allow-list are kept.
/// - Exclusions are applied after inclusion filtering.
fn apply_tool_filter_actions(
    store: &crate::state::StateStore,
    tools: &mut Vec<crate::contract::tool::ToolDescriptor>,
) -> Result<(), crate::error::StateError> {
    use super::state::{ExcludeTool, IncludeOnlyTools};
    use crate::model::ScheduledActionSpec;
    use std::collections::HashSet;

    let pending = store.read::<PendingScheduledActions>().unwrap_or_default();

    // Collect ExcludeTool actions
    let exclude_matching: Vec<_> = pending
        .iter()
        .filter(|e| e.action.key == ExcludeTool::KEY)
        .collect();

    let mut exclude_ids: HashSet<String> = HashSet::new();
    let mut action_ids: Vec<u64> = Vec::new();

    for envelope in &exclude_matching {
        let payload = ExcludeTool::decode_payload(envelope.action.payload.clone())?;
        exclude_ids.insert(payload);
        action_ids.push(envelope.id);
    }

    // Collect IncludeOnlyTools actions
    let include_matching: Vec<_> = pending
        .iter()
        .filter(|e| e.action.key == IncludeOnlyTools::KEY)
        .collect();

    let mut include_ids: Option<HashSet<String>> = None;

    for envelope in &include_matching {
        let payload = IncludeOnlyTools::decode_payload(envelope.action.payload.clone())?;
        let set = include_ids.get_or_insert_with(HashSet::new);
        set.extend(payload);
        action_ids.push(envelope.id);
    }

    // Dequeue all consumed actions
    if !action_ids.is_empty() {
        let mut patch = MutationBatch::new();
        for id in action_ids {
            patch.update::<PendingScheduledActions>(ScheduledActionQueueUpdate::Remove { id });
        }
        store.commit(patch)?;
    }

    // Apply include-only filter first
    if let Some(ref allowed) = include_ids {
        tools.retain(|t| allowed.contains(&t.id));
    }

    // Apply exclusions
    if !exclude_ids.is_empty() {
        tools.retain(|t| !exclude_ids.contains(&t.id));
    }

    Ok(())
}

/// Insert context messages into the message list at their declared target positions.
fn apply_context_messages(
    messages: &mut Vec<Message>,
    context_messages: Vec<crate::contract::context_message::ContextMessage>,
    has_system_prompt: bool,
) {
    use crate::contract::context_message::ContextMessageTarget;

    let mut system = Vec::new();
    let mut session = Vec::new();
    let mut conversation = Vec::new();
    let mut suffix = Vec::new();

    for entry in context_messages {
        let msg = Message {
            id: Some(crate::contract::message::gen_message_id()),
            role: entry.role,
            content: entry.content,
            tool_calls: None,
            tool_call_id: None,
            visibility: entry.visibility,
            metadata: None,
        };
        match entry.target {
            ContextMessageTarget::System => system.push(msg),
            ContextMessageTarget::Session => session.push(msg),
            ContextMessageTarget::Conversation => conversation.push(msg),
            ContextMessageTarget::SuffixSystem => suffix.push(msg),
        }
    }

    // System: insert after base system prompt
    let system_insert_pos = usize::from(has_system_prompt);
    for (offset, msg) in system.into_iter().enumerate() {
        messages.insert(system_insert_pos + offset, msg);
    }

    // Session: insert after all system-role messages
    let session_insert_pos = messages
        .iter()
        .take_while(|m| m.role == Role::System)
        .count();
    for (offset, msg) in session.into_iter().enumerate() {
        messages.insert(session_insert_pos + offset, msg);
    }

    // Conversation: insert after system messages, before history
    let conversation_insert_pos = messages
        .iter()
        .take_while(|m| m.role == Role::System)
        .count();
    for (offset, msg) in conversation.into_iter().enumerate() {
        messages.insert(conversation_insert_pos + offset, msg);
    }

    // Suffix: append at end
    messages.extend(suffix);
}

/// Compact messages using the configured ContextSummarizer.
///
/// Finds a safe compaction boundary, renders messages as transcript (filtering
/// Internal messages), extracts any previous summary for cumulative updates,
/// calls the summarizer, and replaces old messages with the summary.
///
/// Skips compaction if the estimated token savings are below `MIN_COMPACTION_GAIN_TOKENS`.
async fn compact_with_llm(
    agent: &super::config::AgentConfig,
    messages: &mut Vec<Arc<Message>>,
    policy: &crate::contract::inference::ContextWindowPolicy,
) -> Result<(), AgentLoopError> {
    use super::context::{
        MIN_COMPACTION_GAIN_TOKENS, extract_previous_summary, find_compaction_boundary,
        render_transcript,
    };

    let summarizer = match agent.context_summarizer {
        Some(ref s) => s,
        None => return Ok(()),
    };

    if messages.len() < 2 {
        return Ok(());
    }

    let keep_suffix = policy.compaction_raw_suffix_messages.min(messages.len());
    let search_end = messages.len().saturating_sub(keep_suffix);
    if search_end < 2 {
        return Ok(());
    }

    let boundary = match find_compaction_boundary(messages, 0, search_end) {
        Some(b) => b,
        None => return Ok(()),
    };

    // Check minimum gain threshold
    let compactable_tokens: usize = messages[..=boundary]
        .iter()
        .map(|m| crate::contract::transform::estimate_message_tokens(m))
        .sum();
    if compactable_tokens < MIN_COMPACTION_GAIN_TOKENS {
        return Ok(());
    }

    // Render transcript (excludes Internal messages)
    let transcript = render_transcript(&messages[..=boundary]);
    if transcript.is_empty() {
        return Ok(());
    }

    // Extract previous summary for cumulative update
    let previous_summary = extract_previous_summary(messages);

    let summary_text = summarizer
        .summarize(
            &transcript,
            previous_summary.as_deref(),
            agent.llm_executor.as_ref(),
        )
        .await
        .map_err(|e| AgentLoopError::InferenceFailed(format!("compaction failed: {e}")))?;

    // Replace messages up to boundary with the summary
    let post_tokens = crate::contract::transform::estimate_tokens_arc(&messages[boundary + 1..]);
    messages.drain(..=boundary);
    messages.insert(
        0,
        Arc::new(Message::internal_system(format!(
            "<conversation-summary>\n{summary_text}\n</conversation-summary>"
        ))),
    );

    tracing::info!(
        pre_tokens = compactable_tokens,
        post_tokens,
        boundary,
        "compaction_complete"
    );

    Ok(())
}

/// Execute LLM inference with streaming, emitting delta events via sink.
///
/// Consumes the token stream from `execute_stream()`, forwards deltas to sink,
/// and collects the final `StreamResult`.
///
/// Supports mid-stream cancellation: if the `CancellationToken` is signalled while
/// waiting for the next token, the stream is dropped and the partially accumulated
/// result is returned with `StopReason::EndTurn` (graceful cancel — no error).
async fn execute_streaming(
    agent: &AgentConfig,
    request: InferenceRequest,
    sink: &dyn EventSink,
    cancellation_token: Option<&CancellationToken>,
    total_input_tokens: &mut u64,
    total_output_tokens: &mut u64,
) -> Result<crate::contract::inference::StreamResult, AgentLoopError> {
    use crate::contract::content::ContentBlock;
    use crate::contract::executor::StreamEvent;
    use crate::contract::inference::{StopReason, StreamResult, TokenUsage};
    use futures::StreamExt;

    let mut token_stream = agent
        .llm_executor
        .execute_stream(request)
        .await
        .map_err(|e| AgentLoopError::InferenceFailed(e.to_string()))?;

    let mut content_blocks: Vec<ContentBlock> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut usage: Option<TokenUsage> = None;
    let mut stop_reason: Option<StopReason> = None;
    let mut current_text = String::new();
    let mut current_tool_args: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut tool_names: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut cancelled = false;

    loop {
        let event = if let Some(token) = cancellation_token {
            tokio::select! {
                biased;
                _ = token.cancelled() => {
                    cancelled = true;
                    break;
                }
                next = token_stream.next() => next,
            }
        } else {
            token_stream.next().await
        };

        let Some(event_result) = event else {
            break; // stream ended
        };

        let event = event_result.map_err(|e| AgentLoopError::InferenceFailed(e.to_string()))?;

        match event {
            StreamEvent::TextDelta(delta) => {
                current_text.push_str(&delta);
                sink.emit(AgentEvent::TextDelta { delta }).await;
            }
            StreamEvent::ReasoningDelta(delta) => {
                sink.emit(AgentEvent::ReasoningDelta { delta }).await;
            }
            StreamEvent::ToolCallStart { id, name } => {
                sink.emit(AgentEvent::ToolCallStart {
                    id: id.clone(),
                    name: name.clone(),
                })
                .await;
                tool_names.insert(id.clone(), name);
                current_tool_args.insert(id, String::new());
            }
            StreamEvent::ToolCallDelta { id, args_delta } => {
                if let Some(buf) = current_tool_args.get_mut(&id) {
                    buf.push_str(&args_delta);
                }
                sink.emit(AgentEvent::ToolCallDelta { id, args_delta })
                    .await;
            }
            StreamEvent::ContentBlockStop => {
                if !current_text.is_empty() {
                    content_blocks.push(ContentBlock::text(std::mem::take(&mut current_text)));
                }
            }
            StreamEvent::Usage(u) => {
                if let Some(v) = u.prompt_tokens {
                    *total_input_tokens = total_input_tokens.saturating_add(v.max(0) as u64);
                }
                if let Some(v) = u.completion_tokens {
                    *total_output_tokens = total_output_tokens.saturating_add(v.max(0) as u64);
                }
                usage = Some(u);
            }
            StreamEvent::Stop(reason) => {
                stop_reason = Some(reason);
            }
        }
    }

    // Flush remaining text
    if !current_text.is_empty() {
        content_blocks.push(ContentBlock::text(current_text));
    }

    // Collect tool calls from accumulated args (drop incomplete on cancel)
    let mut has_incomplete_tool_calls = false;
    if !cancelled {
        for (id, args_json) in current_tool_args {
            let name = tool_names.get(&id).cloned().unwrap_or_default();
            let arguments = serde_json::from_str(&args_json).unwrap_or(serde_json::Value::Null);
            if arguments.is_null() && !args_json.is_empty() {
                has_incomplete_tool_calls = true;
                continue; // truncated JSON, skip
            }
            tool_calls.push(ToolCall::new(id, name, arguments));
        }
    }

    Ok(StreamResult {
        content: content_blocks,
        tool_calls,
        usage,
        stop_reason: if cancelled {
            Some(StopReason::EndTurn)
        } else {
            stop_reason
        },
        has_incomplete_tool_calls,
    })
}

fn tool_result_to_content(result: &ToolResult) -> String {
    match &result.message {
        Some(msg) => msg.clone(),
        None => serde_json::to_string(&result.data).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::content::ContentBlock;
    use crate::contract::context_message::ContextMessage;
    use crate::contract::message::{Message, Role};
    use crate::model::{
        PendingScheduledActions, ScheduledAction, ScheduledActionEnvelope,
        ScheduledActionQueueUpdate, ScheduledActionSpec,
    };
    use crate::state::{StateKeyOptions, StateStore};

    use super::super::state::AddContextMessage;

    /// Test-only plugin that registers the PendingScheduledActions key.
    struct TestQueuePlugin;

    impl crate::plugins::Plugin for TestQueuePlugin {
        fn descriptor(&self) -> crate::plugins::PluginDescriptor {
            crate::plugins::PluginDescriptor {
                name: "__test_queue",
            }
        }

        fn register(
            &self,
            r: &mut crate::plugins::PluginRegistrar,
        ) -> Result<(), crate::error::StateError> {
            r.register_key::<PendingScheduledActions>(StateKeyOptions::default())?;
            Ok(())
        }
    }

    /// Helper: create a StateStore with all keys needed by context message machinery.
    fn test_store() -> StateStore {
        let store = StateStore::new();
        store
            .install_plugin(TestQueuePlugin)
            .expect("install TestQueuePlugin");
        store
            .install_plugin(LoopStatePlugin)
            .expect("install LoopStatePlugin");
        store
    }

    /// Helper: push a context message action into the pending queue.
    fn enqueue_context_message(store: &StateStore, id: u64, msg: ContextMessage) {
        let payload = AddContextMessage::encode_payload(&msg).expect("encode payload");
        let mut batch = crate::state::MutationBatch::new();
        batch.update::<PendingScheduledActions>(ScheduledActionQueueUpdate::Push(
            ScheduledActionEnvelope {
                id,
                action: ScheduledAction::new(
                    AddContextMessage::PHASE,
                    AddContextMessage::KEY,
                    payload,
                ),
            },
        ));
        store.commit(batch).expect("commit enqueue");
    }

    /// Helper: extract all text from a message's content blocks.
    fn text_of(msg: &Message) -> String {
        msg.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    // -----------------------------------------------------------------------
    // apply_context_messages tests (message placement)
    // -----------------------------------------------------------------------

    #[test]
    fn context_message_injected_at_system_target() {
        let mut messages = vec![
            Message::system("base system prompt"),
            Message::user("hello"),
        ];
        let ctx = vec![ContextMessage::system("reminder", "remember the rules")];
        apply_context_messages(&mut messages, ctx, true);

        // System-target message should be inserted after the base system prompt (index 1)
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, Role::System);
        assert_eq!(text_of(&messages[0]), "base system prompt");
        assert_eq!(messages[1].role, Role::System);
        assert_eq!(text_of(&messages[1]), "remember the rules");
        assert_eq!(messages[2].role, Role::User);
    }

    #[test]
    fn context_message_injected_at_suffix_target() {
        let mut messages = vec![
            Message::system("system"),
            Message::user("hello"),
            Message::system(""), // simulate assistant-like; using system for simplicity
        ];
        let original_len = messages.len();
        let ctx = vec![ContextMessage::suffix_system(
            "suffix.key",
            "final reminder",
        )];
        apply_context_messages(&mut messages, ctx, true);

        // Suffix messages should be appended at the end
        assert_eq!(messages.len(), original_len + 1);
        let last = messages.last().unwrap();
        assert_eq!(last.role, Role::System);
        assert_eq!(text_of(last), "final reminder");
    }

    #[test]
    fn multiple_context_messages_sorted_by_target() {
        let mut messages = vec![Message::system("system prompt"), Message::user("user msg")];

        let ctx = vec![
            ContextMessage::suffix_system("s1", "suffix text"),
            ContextMessage::system("sys1", "after-system text"),
            ContextMessage::conversation("conv1", Role::User, "conversation text"),
        ];
        apply_context_messages(&mut messages, ctx, true);

        // Expected order:
        // [0] system prompt (original)
        // [1] after-system text (System target, after base system prompt)
        // [2] conversation text (Conversation target, after system messages)
        // [3] user msg (original)
        // [4] suffix text (SuffixSystem target, at end)
        assert_eq!(messages.len(), 5);
        assert_eq!(text_of(&messages[0]), "system prompt");
        assert_eq!(text_of(&messages[1]), "after-system text");
        assert_eq!(text_of(&messages[2]), "conversation text");
        assert_eq!(text_of(&messages[3]), "user msg");
        assert_eq!(text_of(&messages[4]), "suffix text");
    }

    // -----------------------------------------------------------------------
    // consume_context_messages tests (throttle logic)
    // -----------------------------------------------------------------------

    #[test]
    fn throttle_zero_cooldown_always_injects() {
        let store = test_store();

        for step in 0..5 {
            enqueue_context_message(
                &store,
                step as u64,
                ContextMessage::system("always", "inject me").with_cooldown(0),
            );
            let accepted = consume_context_messages(&store, step).expect("consume");
            assert_eq!(
                accepted.len(),
                1,
                "cooldown=0 should inject at every step, failed at step {step}"
            );
        }
    }

    #[test]
    fn throttle_skips_within_cooldown() {
        let store = test_store();

        // Step 0: first injection, should be accepted
        enqueue_context_message(
            &store,
            1,
            ContextMessage::system("throttled", "content").with_cooldown(3),
        );
        let accepted = consume_context_messages(&store, 0).expect("step 0");
        assert_eq!(accepted.len(), 1, "first injection at step 0 should pass");

        // Steps 1 and 2: within cooldown, should be skipped
        for step in 1..=2 {
            enqueue_context_message(
                &store,
                10 + step as u64,
                ContextMessage::system("throttled", "content").with_cooldown(3),
            );
            let accepted = consume_context_messages(&store, step)
                .unwrap_or_else(|e| panic!("step {step}: {e}"));
            assert_eq!(
                accepted.len(),
                0,
                "should be throttled at step {step} (cooldown=3, last_step=0)"
            );
        }

        // Step 3: cooldown expired (3 - 0 >= 3), should inject
        enqueue_context_message(
            &store,
            20,
            ContextMessage::system("throttled", "content").with_cooldown(3),
        );
        let accepted = consume_context_messages(&store, 3).expect("step 3");
        assert_eq!(
            accepted.len(),
            1,
            "cooldown expired at step 3, should inject"
        );
    }

    #[test]
    fn throttle_bypassed_on_content_change() {
        let store = test_store();

        // Step 0: initial injection
        enqueue_context_message(
            &store,
            1,
            ContextMessage::system("changing", "original content").with_cooldown(10),
        );
        let accepted = consume_context_messages(&store, 0).expect("step 0");
        assert_eq!(accepted.len(), 1);

        // Step 1: same content, within cooldown — should be throttled
        enqueue_context_message(
            &store,
            2,
            ContextMessage::system("changing", "original content").with_cooldown(10),
        );
        let accepted = consume_context_messages(&store, 1).expect("step 1 same content");
        assert_eq!(
            accepted.len(),
            0,
            "same content within cooldown should be throttled"
        );

        // Step 2: different content, within cooldown — should bypass
        enqueue_context_message(
            &store,
            3,
            ContextMessage::system("changing", "updated content").with_cooldown(10),
        );
        let accepted = consume_context_messages(&store, 2).expect("step 2 new content");
        assert_eq!(
            accepted.len(),
            1,
            "different content should bypass cooldown"
        );
        assert_eq!(text_of_ctx(&accepted[0]), "updated content");
    }

    /// Verify that tracing instrumentation does not panic when no subscriber is installed.
    ///
    /// Exercises the context transform (which emits `truncation_applied`) and
    /// direct tracing macro calls matching those added to the loop runner and engine.
    #[test]
    fn tracing_does_not_panic_without_subscriber() {
        use crate::agent::context::ContextTransform;
        use crate::contract::inference::ContextWindowPolicy;
        use crate::contract::transform::InferenceRequestTransform;

        // Exercise ContextTransform truncation path (emits tracing::debug!)
        let policy = ContextWindowPolicy {
            max_context_tokens: 40,
            max_output_tokens: 0,
            min_recent_messages: 1,
            enable_prompt_cache: false,
            autocompact_threshold: None,
            compaction_mode: Default::default(),
            compaction_raw_suffix_messages: 2,
        };
        let transform = ContextTransform::new(policy);
        let mut msgs = vec![Message::system("sys")];
        for i in 0..10 {
            msgs.push(Message::user(format!("msg {i}")));
            msgs.push(Message::assistant(format!("reply {i}")));
        }
        // This triggers the truncation_applied tracing call — must not panic
        let _output = transform.transform(msgs, &[]);

        // Exercise loop-runner-style tracing macros directly — must not panic
        tracing::info!(step = 1u64, "step_start");
        tracing::info!(
            model = "test-model",
            input_tokens = 100u64,
            output_tokens = 50u64,
            duration_ms = 42u64,
            "inference_complete"
        );
        tracing::info!(
            tool_name = "calculator",
            call_id = "c1",
            outcome = "Succeeded",
            "tool_call_done"
        );
        tracing::warn!(reason = "NaturalEnd", "run_terminated");

        // Exercise engine-style tracing macros — must not panic
        tracing::debug!(phase = "StepStart", hooks = 3usize, "gather_start");
        tracing::debug!(phase = "StepStart", actions = 2usize, "execute_start");
        tracing::warn!(phase = "StepStart", "exclusive_conflict_fallback");

        // Exercise context compaction tracing — must not panic
        tracing::info!(
            pre_tokens = 2000usize,
            post_tokens = 500usize,
            boundary = 10usize,
            "compaction_complete"
        );
        tracing::debug!(dropped = 5usize, kept = 8usize, "truncation_applied");
    }

    /// Helper: extract text from a ContextMessage's content blocks.
    fn text_of_ctx(msg: &ContextMessage) -> String {
        msg.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    // -----------------------------------------------------------------------
    // Tool filter action tests (ExcludeTool / IncludeOnlyTools)
    // -----------------------------------------------------------------------

    use super::super::state::{ExcludeTool, IncludeOnlyTools};
    use crate::contract::tool::ToolDescriptor;

    /// Helper: push an ExcludeTool action into the pending queue.
    fn enqueue_exclude_tool(store: &StateStore, id: u64, tool_id: &str) {
        let payload = ExcludeTool::encode_payload(&tool_id.to_string()).expect("encode");
        let mut batch = crate::state::MutationBatch::new();
        batch.update::<PendingScheduledActions>(ScheduledActionQueueUpdate::Push(
            ScheduledActionEnvelope {
                id,
                action: ScheduledAction::new(ExcludeTool::PHASE, ExcludeTool::KEY, payload),
            },
        ));
        store.commit(batch).expect("commit enqueue");
    }

    /// Helper: push an IncludeOnlyTools action into the pending queue.
    fn enqueue_include_only_tools(store: &StateStore, id: u64, tool_ids: Vec<String>) {
        let payload = IncludeOnlyTools::encode_payload(&tool_ids).expect("encode");
        let mut batch = crate::state::MutationBatch::new();
        batch.update::<PendingScheduledActions>(ScheduledActionQueueUpdate::Push(
            ScheduledActionEnvelope {
                id,
                action: ScheduledAction::new(
                    IncludeOnlyTools::PHASE,
                    IncludeOnlyTools::KEY,
                    payload,
                ),
            },
        ));
        store.commit(batch).expect("commit enqueue");
    }

    /// Helper: create a simple tool descriptor with the given id.
    fn tool(id: &str) -> ToolDescriptor {
        ToolDescriptor::new(id, id, format!("{id} tool"))
    }

    #[test]
    fn exclude_tool_removes_from_request() {
        let store = test_store();
        let mut tools = vec![tool("search"), tool("calculator"), tool("browser")];

        enqueue_exclude_tool(&store, 1, "search");

        apply_tool_filter_actions(&store, &mut tools).expect("apply");

        let ids: Vec<_> = tools.iter().map(|t| t.id.as_str()).collect();
        assert!(!ids.contains(&"search"), "search should be excluded");
        assert!(ids.contains(&"calculator"));
        assert!(ids.contains(&"browser"));
        assert_eq!(tools.len(), 2);

        // Actions should be consumed from the queue
        let pending = store.read::<PendingScheduledActions>().unwrap_or_default();
        assert!(
            pending.is_empty(),
            "actions should be dequeued after consumption"
        );
    }

    #[test]
    fn include_only_tools_filters_to_subset() {
        let store = test_store();
        let mut tools = vec![
            tool("search"),
            tool("calculator"),
            tool("browser"),
            tool("code_exec"),
        ];

        enqueue_include_only_tools(&store, 1, vec!["calculator".into(), "browser".into()]);

        apply_tool_filter_actions(&store, &mut tools).expect("apply");

        let ids: Vec<_> = tools.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"calculator"));
        assert!(ids.contains(&"browser"));
        assert!(!ids.contains(&"search"));
        assert!(!ids.contains(&"code_exec"));
    }

    #[test]
    fn exclude_and_include_only_combined() {
        let store = test_store();
        let mut tools = vec![tool("search"), tool("calculator"), tool("browser")];

        // Include only search + calculator, then exclude search
        enqueue_include_only_tools(&store, 1, vec!["search".into(), "calculator".into()]);
        enqueue_exclude_tool(&store, 2, "search");

        apply_tool_filter_actions(&store, &mut tools).expect("apply");

        let ids: Vec<_> = tools.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["calculator"]);
    }

    #[test]
    fn multiple_exclude_tool_actions() {
        let store = test_store();
        let mut tools = vec![tool("a"), tool("b"), tool("c"), tool("d")];

        enqueue_exclude_tool(&store, 1, "a");
        enqueue_exclude_tool(&store, 2, "c");

        apply_tool_filter_actions(&store, &mut tools).expect("apply");

        let ids: Vec<_> = tools.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["b", "d"]);
    }

    #[test]
    fn no_filter_actions_leaves_tools_unchanged() {
        let store = test_store();
        let mut tools = vec![tool("search"), tool("calculator")];

        apply_tool_filter_actions(&store, &mut tools).expect("apply");

        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn multiple_include_only_actions_union() {
        let store = test_store();
        let mut tools = vec![tool("a"), tool("b"), tool("c"), tool("d")];

        // Two separate include-only actions; their union should be used
        enqueue_include_only_tools(&store, 1, vec!["a".into()]);
        enqueue_include_only_tools(&store, 2, vec!["c".into()]);

        apply_tool_filter_actions(&store, &mut tools).expect("apply");

        let ids: Vec<_> = tools.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "c"]);
    }
}
