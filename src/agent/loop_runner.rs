//! Minimal sequential agent loop driven by state machines.
//!
//! Run lifecycle: RunLifecycle (Running → StepCompleted → Done/Waiting)
//! Tool call lifecycle: ToolCallStates (New → Running → Succeeded/Failed/Suspended)

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::contract::event::AgentEvent;
use crate::contract::event_sink::EventSink;
use crate::contract::executor::InferenceRequest;
use crate::contract::identity::RunIdentity;
use crate::contract::inference::LLMResponse;
use crate::contract::lifecycle::{RunStatus, TerminationReason};
use crate::contract::message::{Message, Role, ToolCall, gen_message_id};
use crate::contract::suspension::{
    ResumeDecisionAction, ToolCallOutcome, ToolCallResume, ToolCallResumeMode, ToolCallStatus,
};
use crate::contract::tool::{ToolCallContext, ToolResult};
use crate::error::StateError;
use crate::model::{Phase, RuntimeEffect};
use crate::runtime::{ExecutionEnv, PhaseContext, PhaseRuntime, TypedEffectHandler};
use crate::state::{MutationBatch, Snapshot, StateCommand};

use super::config::AgentConfig;
use super::state::{RunLifecycle, RunLifecycleUpdate, ToolCallStates, ToolCallStatesUpdate};

/// Errors from the agent loop.
#[derive(Debug, thiserror::Error)]
pub enum AgentLoopError {
    #[error("inference failed: {0}")]
    InferenceFailed(String),
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

// ---------------------------------------------------------------------------
// Termination flag — set by RuntimeEffect::Terminate effect handler
// ---------------------------------------------------------------------------

/// Shared termination flag. Checked at phase boundaries to break the step loop.
///
/// Implements `Plugin` so it can be included in `ExecutionEnv::from_plugins()`.
/// The loop runner creates one and reads it; the effect handler sets it when
/// `RuntimeEffect::Terminate` is dispatched.
#[derive(Clone, Default)]
pub struct TerminationFlag {
    inner: Arc<Mutex<Option<TerminationReason>>>,
}

impl TerminationFlag {
    pub(crate) fn take(&self) -> Option<TerminationReason> {
        self.inner.lock().expect("termination flag poisoned").take()
    }
}

struct TerminationFlagHandler(TerminationFlag);

#[async_trait]
impl TypedEffectHandler<RuntimeEffect> for TerminationFlagHandler {
    async fn handle_typed(
        &self,
        payload: RuntimeEffect,
        _snapshot: &Snapshot,
    ) -> Result<(), String> {
        if let RuntimeEffect::Terminate { reason } = payload {
            let mut guard = self.0.inner.lock().expect("termination flag poisoned");
            if guard.is_none() {
                *guard = Some(reason);
            }
        }
        Ok(())
    }
}

impl crate::plugins::Plugin for TerminationFlagHandler {
    fn descriptor(&self) -> crate::plugins::PluginDescriptor {
        crate::plugins::PluginDescriptor {
            name: "__termination_flag",
        }
    }

    fn register(
        &self,
        registrar: &mut crate::plugins::PluginRegistrar,
    ) -> Result<(), crate::error::StateError> {
        registrar.register_effect::<RuntimeEffect, _>(TerminationFlagHandler(self.0.clone()))
    }
}

/// Build an execution environment for the agent loop from a list of plugins.
///
/// Adds internal plugins (termination handler, stop conditions, default permission).
/// Returns the env and a termination flag that the loop reads.
pub fn build_agent_env(
    plugins: &[Arc<dyn crate::plugins::Plugin>],
    max_rounds: usize,
) -> Result<(ExecutionEnv, TerminationFlag), StateError> {
    use super::stop_conditions::MaxRoundsPlugin;
    use super::tool_permission::AllowAllToolsPlugin;

    let flag = TerminationFlag::default();

    let mut all_plugins: Vec<Arc<dyn crate::plugins::Plugin>> = plugins.to_vec();
    all_plugins.push(Arc::new(TerminationFlagHandler(flag.clone())));
    all_plugins.push(Arc::new(MaxRoundsPlugin::new(max_rounds)));
    all_plugins.push(Arc::new(AllowAllToolsPlugin));

    let env = ExecutionEnv::from_plugins(&all_plugins)?;
    Ok((env, flag))
}

pub async fn run_agent_loop(
    agent: &AgentConfig,
    runtime: &PhaseRuntime,
    env: &ExecutionEnv,
    termination_flag: &TerminationFlag,
    sink: &dyn EventSink,
    initial_messages: Vec<Message>,
    run_identity: RunIdentity,
) -> Result<AgentRunResult, AgentLoopError> {
    let store = runtime.store();
    let mut messages: Vec<Arc<Message>> = initial_messages.into_iter().map(Arc::new).collect();
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
        .run_phase_with_context(env, make_ctx(Phase::RunStart, &messages, &run_identity))
        .await?;

    let termination = loop {
        steps += 1;

        sink.emit(AgentEvent::StepStart {
            message_id: gen_message_id(),
        })
        .await;

        // Clear tool call states from previous step
        commit_update::<ToolCallStates>(store, ToolCallStatesUpdate::Clear)?;

        runtime
            .run_phase_with_context(env, make_ctx(Phase::StepStart, &messages, &run_identity))
            .await?;
        if let Some(reason) = termination_flag.take() {
            break reason;
        }

        runtime
            .run_phase_with_context(
                env,
                make_ctx(Phase::BeforeInference, &messages, &run_identity),
            )
            .await?;
        if let Some(reason) = termination_flag.take() {
            break reason;
        }

        // LLM inference — read per-inference overrides from state, then clear
        let overrides = store
            .read::<super::state::InferenceOverrides>()
            .filter(|o| !o.is_empty());
        if overrides.is_some() {
            commit_update::<super::state::InferenceOverrides>(
                store,
                super::state::InferenceOverridesUpdate::Clear,
            )?;
        }

        let start = std::time::Instant::now();
        let request = InferenceRequest {
            model: agent.model.clone(),
            messages: messages.iter().map(|m| (**m).clone()).collect(),
            tools: agent.tool_descriptors(),
            system_prompt: Some(agent.system_prompt.clone()),
            overrides,
        };

        let stream_result = agent
            .llm_executor
            .execute(request)
            .await
            .map_err(|e| AgentLoopError::InferenceFailed(e.to_string()))?;

        let duration_ms = start.elapsed().as_millis() as u64;
        sink.emit(AgentEvent::InferenceComplete {
            model: agent.model.clone(),
            usage: stream_result.usage.clone(),
            duration_ms,
        })
        .await;

        let llm_response = LLMResponse::success(stream_result.clone());
        let after_inf_ctx = make_ctx(Phase::AfterInference, &messages, &run_identity)
            .with_llm_response(llm_response);
        runtime.run_phase_with_context(env, after_inf_ctx).await?;
        if let Some(reason) = termination_flag.take() {
            break reason;
        }

        if !stream_result.needs_tools() {
            messages.push(Arc::new(Message::assistant(&stream_result.text)));
            complete_step(store, runtime, env, sink, &messages, &run_identity).await?;
            break TerminationReason::NaturalEnd;
        }

        // Add assistant message with tool calls
        messages.push(Arc::new(Message::assistant_with_tool_calls(
            &stream_result.text,
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
        let mut tool_commands = Vec::new();

        for call in &stream_result.tool_calls {
            let perm_ctx = make_ctx(Phase::BeforeToolExecute, &messages, &run_identity)
                .with_tool_info(&call.name, &call.id, Some(call.arguments.clone()));
            let perm_result = runtime.check_tool_permission(env, &perm_ctx).await?;

            match perm_result {
                crate::runtime::ToolPermissionResult::Allow => {
                    allowed_calls.push(call.clone());
                }
                crate::runtime::ToolPermissionResult::Deny { reason } => {
                    let mut lifecycle_cmd = StateCommand::new();
                    lifecycle_cmd.update::<ToolCallStates>(ToolCallStatesUpdate::Upsert {
                        call_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        status: ToolCallStatus::Failed,
                        updated_at: now_ms(),
                    });
                    tool_commands.push(lifecycle_cmd);
                    messages.push(Arc::new(Message::tool(
                        &call.id,
                        format!("Permission denied: {reason}"),
                    )));
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

        // Execute allowed tool calls via ToolExecutor
        let tool_ctx = ToolCallContext {
            call_id: String::new(), // filled per-call by executor
            run_identity: run_identity.clone(),
            profile: make_ctx(Phase::BeforeToolExecute, &messages, &run_identity).profile,
            snapshot: store.snapshot(),
        };
        let exec_results = agent
            .tool_executor
            .execute(&agent.tools, &allowed_calls, &tool_ctx)
            .await
            .map_err(|e| AgentLoopError::InferenceFailed(e.to_string()))?;

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
            let before_cmd = runtime.collect_commands(env, before_ctx).await?;
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
            let after_cmd = runtime.collect_commands(env, after_ctx).await?;
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
            runtime.submit_command(env, merged).await?;
        }

        // Check termination after tool execution
        if let Some(reason) = termination_flag.take() {
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
            complete_step(store, runtime, env, sink, &messages, &run_identity).await?;
            break TerminationReason::Suspended;
        }

        complete_step(store, runtime, env, sink, &messages, &run_identity).await?;
        if let Some(reason) = termination_flag.take() {
            break reason;
        }
    };

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
        .run_phase_with_context(env, make_ctx(Phase::RunEnd, &messages, &run_identity))
        .await?;

    let response = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| m.content.clone())
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

/// External decisions for suspended tool calls, used to resume a suspended run.
pub struct ResumeInput {
    /// Decisions for each suspended tool call, keyed by call_id.
    pub decisions: Vec<(String, ToolCallResume)>,
    /// Resume mode to apply (defaults to ReplayToolCall).
    pub resume_mode: ToolCallResumeMode,
}

/// Resume a suspended agent run.
///
/// Transitions Waiting → Running, applies decisions to suspended tool calls,
/// and re-enters the step loop. The three resume modes:
/// - **ReplayToolCall**: re-execute the tool with original arguments
/// - **UseDecisionAsToolResult**: use the decision payload as the tool result
/// - **PassDecisionToTool**: re-execute the tool with decision payload as new arguments
pub async fn resume_agent_loop(
    agent: &AgentConfig,
    runtime: &PhaseRuntime,
    env: &ExecutionEnv,
    termination_flag: &TerminationFlag,
    sink: &dyn EventSink,
    messages: Vec<Message>,
    run_identity: RunIdentity,
    input: ResumeInput,
) -> Result<AgentRunResult, AgentLoopError> {
    let store = runtime.store();

    // Validate: run must be in Waiting state
    let lifecycle = store
        .read::<RunLifecycle>()
        .ok_or_else(|| AgentLoopError::InvalidResume("no run lifecycle state found".into()))?;
    if lifecycle.status != RunStatus::Waiting {
        return Err(AgentLoopError::InvalidResume(format!(
            "run is {:?}, expected Waiting",
            lifecycle.status
        )));
    }

    // Transition Waiting → Running
    commit_update::<RunLifecycle>(
        store,
        RunLifecycleUpdate::Start {
            run_id: run_identity.run_id.clone(),
            updated_at: now_ms(),
        },
    )?;

    // Apply decisions: transition each suspended tool call
    let tool_call_states = store.read::<ToolCallStates>().unwrap_or_default();
    let resume_tool_ctx = ToolCallContext {
        call_id: String::new(),
        run_identity: run_identity.clone(),
        profile: std::sync::Arc::new(crate::contract::profile::AgentProfile::default()),
        snapshot: store.snapshot(),
    };

    let mut resume_messages: Vec<Arc<Message>> = messages.into_iter().map(Arc::new).collect();

    for (call_id, decision) in &input.decisions {
        let call_state = tool_call_states.calls.get(call_id).ok_or_else(|| {
            AgentLoopError::InvalidResume(format!("tool call {call_id} not found in state"))
        })?;
        if call_state.status != ToolCallStatus::Suspended {
            return Err(AgentLoopError::InvalidResume(format!(
                "tool call {call_id} is {:?}, expected Suspended",
                call_state.status
            )));
        }

        match decision.action {
            ResumeDecisionAction::Cancel => {
                // Suspended → Cancelled
                commit_update::<ToolCallStates>(
                    store,
                    ToolCallStatesUpdate::Upsert {
                        call_id: call_id.clone(),
                        tool_name: call_state.tool_name.clone(),
                        arguments: call_state.arguments.clone(),
                        status: ToolCallStatus::Cancelled,
                        updated_at: now_ms(),
                    },
                )?;
                resume_messages.push(Arc::new(Message::tool(
                    call_id,
                    format!(
                        "Tool call cancelled: {}",
                        decision.reason.as_deref().unwrap_or("user cancelled")
                    ),
                )));
            }
            ResumeDecisionAction::Resume => {
                // Suspended → Resuming
                commit_update::<ToolCallStates>(
                    store,
                    ToolCallStatesUpdate::Upsert {
                        call_id: call_id.clone(),
                        tool_name: call_state.tool_name.clone(),
                        arguments: call_state.arguments.clone(),
                        status: ToolCallStatus::Resuming,
                        updated_at: now_ms(),
                    },
                )?;

                // Apply resume mode
                let tool_result = match input.resume_mode {
                    ToolCallResumeMode::UseDecisionAsToolResult => {
                        // Decision payload becomes the tool result directly
                        let content = if decision.result.is_null() {
                            "approved".to_string()
                        } else {
                            serde_json::to_string(&decision.result).unwrap_or_default()
                        };

                        commit_update::<ToolCallStates>(
                            store,
                            ToolCallStatesUpdate::Upsert {
                                call_id: call_id.clone(),
                                tool_name: call_state.tool_name.clone(),
                                arguments: call_state.arguments.clone(),
                                status: ToolCallStatus::Succeeded,
                                updated_at: now_ms(),
                            },
                        )?;

                        content
                    }
                    ToolCallResumeMode::ReplayToolCall => {
                        // Re-execute with original arguments
                        let call = ToolCall::new(
                            call_id,
                            &call_state.tool_name,
                            call_state.arguments.clone(),
                        );
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

                        tool_result_to_content(&result)
                    }
                    ToolCallResumeMode::PassDecisionToTool => {
                        // Re-execute with decision payload as new arguments
                        let call =
                            ToolCall::new(call_id, &call_state.tool_name, decision.result.clone());
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
                                arguments: decision.result.clone(),
                                status,
                                updated_at: now_ms(),
                            },
                        )?;

                        tool_result_to_content(&result)
                    }
                };

                resume_messages.push(Arc::new(Message::tool(call_id, tool_result)));
            }
        }
    }

    // Continue the main loop from where we left off
    run_agent_loop(
        agent,
        runtime,
        env,
        termination_flag,
        sink,
        resume_messages.into_iter().map(|m| (*m).clone()).collect(),
        run_identity,
    )
    .await
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
    messages: &[Arc<Message>],
    run_identity: &RunIdentity,
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
    runtime.run_phase_with_context(env, ctx).await?;
    sink.emit(AgentEvent::StepEnd).await;
    Ok(())
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

fn tool_result_to_content(result: &ToolResult) -> String {
    match &result.message {
        Some(msg) => msg.clone(),
        None => serde_json::to_string(&result.data).unwrap_or_default(),
    }
}
