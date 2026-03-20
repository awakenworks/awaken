//! Minimal sequential agent loop driven by state machines.
//!
//! Run lifecycle: RunLifecycleSlot (Running → StepCompleted → Done/Waiting)
//! Tool call lifecycle: ToolCallStatesSlot (New → Running → Succeeded/Failed/Suspended)

use std::sync::{Arc, Mutex};

use crate::contract::event::AgentEvent;
use crate::contract::executor::InferenceRequest;
use crate::contract::identity::RunIdentity;
use crate::contract::inference::LLMResponse;
use crate::contract::lifecycle::TerminationReason;
use crate::contract::message::{Message, Role, ToolCall, gen_message_id};
use crate::contract::suspension::{ToolCallOutcome, ToolCallStatus};
use crate::contract::tool::{ToolResult, ToolStatus};
use crate::model::{Phase, RuntimeEffect};
use crate::runtime::{PhaseContext, PhaseRuntime, TypedEffectHandler};
use crate::state::{MutationBatch, Snapshot};

use super::config::AgentConfig;
use super::state::{
    RunLifecycleSlot, RunLifecycleUpdate, ToolCallStatesSlot, ToolCallStatesUpdate,
};
use super::stop_conditions::MaxRoundsPlugin;

/// Errors from the agent loop.
#[derive(Debug, thiserror::Error)]
pub enum AgentLoopError {
    #[error("inference failed: {0}")]
    InferenceFailed(String),
    #[error("phase error: {0}")]
    PhaseError(#[from] crate::error::StateError),
}

/// Result of running the agent loop.
#[derive(Debug)]
pub struct AgentRunResult {
    pub response: String,
    pub termination: TerminationReason,
    pub events: Vec<AgentEvent>,
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

#[derive(Clone, Default)]
struct TerminationFlag {
    inner: Arc<Mutex<Option<TerminationReason>>>,
}

impl TerminationFlag {
    fn is_set(&self) -> bool {
        self.inner
            .lock()
            .expect("termination flag poisoned")
            .is_some()
    }

    fn take(&self) -> Option<TerminationReason> {
        self.inner.lock().expect("termination flag poisoned").take()
    }
}

impl TypedEffectHandler<RuntimeEffect> for TerminationFlag {
    fn handle_typed(&self, payload: RuntimeEffect, _snapshot: &Snapshot) -> Result<(), String> {
        if let RuntimeEffect::Terminate { reason } = payload {
            let mut guard = self.inner.lock().expect("termination flag poisoned");
            if guard.is_none() {
                *guard = Some(reason);
            }
        }
        Ok(())
    }
}

/// Run the agent loop to completion.
///
/// State-machine driven: every lifecycle transition is committed via StateStore.
/// Phase hooks are dispatched through PhaseRuntime.
pub async fn run_agent_loop(
    agent: &AgentConfig,
    runtime: &PhaseRuntime,
    initial_messages: Vec<Message>,
    run_identity: RunIdentity,
) -> Result<AgentRunResult, AgentLoopError> {
    let store = runtime.store();
    let mut events = Vec::new();
    let mut messages: Vec<Arc<Message>> = initial_messages.into_iter().map(Arc::new).collect();
    let mut steps: usize = 0;

    // Register termination effect handler and stop condition plugin
    let termination_flag = TerminationFlag::default();
    runtime.register_effect::<RuntimeEffect, _>(termination_flag.clone())?;
    runtime.install_plugin(MaxRoundsPlugin::new(agent.max_rounds))?;

    // Helper to build PhaseContext with current state
    let make_ctx = |phase: Phase, msgs: &[Arc<Message>], identity: &RunIdentity| -> PhaseContext {
        PhaseContext::new(phase, store.snapshot())
            .with_run_identity(identity.clone())
            .with_messages(msgs.to_vec())
    };

    // --- Run lifecycle: Start ---
    commit_update::<RunLifecycleSlot>(
        store,
        RunLifecycleUpdate::Start {
            run_id: run_identity.run_id.clone(),
            updated_at: now_ms(),
        },
    )?;

    events.push(AgentEvent::RunStart {
        thread_id: run_identity.thread_id.clone(),
        run_id: run_identity.run_id.clone(),
        parent_run_id: run_identity.parent_run_id.clone(),
    });

    runtime.run_phase_with_context(make_ctx(Phase::RunStart, &messages, &run_identity))?;

    let termination = loop {
        steps += 1;

        events.push(AgentEvent::StepStart {
            message_id: gen_message_id(),
        });

        // Clear tool call states from previous step
        commit_update::<ToolCallStatesSlot>(store, ToolCallStatesUpdate::Clear)?;

        runtime.run_phase_with_context(make_ctx(Phase::StepStart, &messages, &run_identity))?;
        if let Some(reason) = termination_flag.take() {
            break reason;
        }

        runtime.run_phase_with_context(make_ctx(
            Phase::BeforeInference,
            &messages,
            &run_identity,
        ))?;
        if let Some(reason) = termination_flag.take() {
            break reason;
        }

        // LLM inference
        let start = std::time::Instant::now();
        let request = InferenceRequest {
            model: agent.model.clone(),
            messages: messages.iter().map(|m| (**m).clone()).collect(),
            tools: agent.tool_descriptors(),
            system_prompt: Some(agent.system_prompt.clone()),
            overrides: None,
        };

        let stream_result = agent
            .llm_executor
            .execute(request)
            .await
            .map_err(|e| AgentLoopError::InferenceFailed(e.to_string()))?;

        let duration_ms = start.elapsed().as_millis() as u64;
        events.push(AgentEvent::InferenceComplete {
            model: agent.model.clone(),
            usage: stream_result.usage.clone(),
            duration_ms,
        });

        let llm_response = LLMResponse::success(stream_result.clone());
        let after_inf_ctx = make_ctx(Phase::AfterInference, &messages, &run_identity)
            .with_llm_response(llm_response);
        runtime.run_phase_with_context(after_inf_ctx)?;
        if let Some(reason) = termination_flag.take() {
            break reason;
        }

        if !stream_result.needs_tools() {
            messages.push(Arc::new(Message::assistant(&stream_result.text)));
            complete_step(store, runtime, &mut events, &messages, &run_identity)?;
            break TerminationReason::NaturalEnd;
        }

        // Add assistant message with tool calls
        messages.push(Arc::new(Message::assistant_with_tool_calls(
            &stream_result.text,
            stream_result.tool_calls.clone(),
        )));

        // Execute each tool call sequentially with state machine transitions
        let mut suspended = false;
        for call in &stream_result.tool_calls {
            events.push(AgentEvent::ToolCallStart {
                id: call.id.clone(),
                name: call.name.clone(),
            });

            // Tool call lifecycle: New → Running
            commit_tool_call_transition(store, call, ToolCallStatus::Running)?;

            let before_ctx = make_ctx(Phase::BeforeToolExecute, &messages, &run_identity)
                .with_tool_info(&call.name, &call.id, Some(call.arguments.clone()));
            runtime.run_phase_with_context(before_ctx)?;
            if termination_flag.is_set() {
                break;
            }

            let tool_result = execute_tool(agent, call).await;

            // Handle suspension: tool returned Pending
            if tool_result.status == ToolStatus::Pending {
                commit_tool_call_transition(store, call, ToolCallStatus::Suspended)?;

                events.push(AgentEvent::ToolCallDone {
                    id: call.id.clone(),
                    result: tool_result.clone(),
                    outcome: ToolCallOutcome::Suspended,
                });

                let after_ctx = make_ctx(Phase::AfterToolExecute, &messages, &run_identity)
                    .with_tool_info(&call.name, &call.id, Some(call.arguments.clone()))
                    .with_tool_result(tool_result.clone());
                runtime.run_phase_with_context(after_ctx)?;

                let tool_content = tool_result_to_content(&tool_result);
                messages.push(Arc::new(Message::tool(&call.id, tool_content)));

                suspended = true;
                break;
            }

            // Tool call lifecycle: Running → Succeeded/Failed
            let terminal_status = if tool_result.is_success() {
                ToolCallStatus::Succeeded
            } else {
                ToolCallStatus::Failed
            };
            commit_tool_call_transition(store, call, terminal_status)?;

            events.push(AgentEvent::ToolCallDone {
                id: call.id.clone(),
                result: tool_result.clone(),
                outcome: ToolCallOutcome::from_tool_result(&tool_result),
            });

            let after_ctx = make_ctx(Phase::AfterToolExecute, &messages, &run_identity)
                .with_tool_info(&call.name, &call.id, Some(call.arguments.clone()))
                .with_tool_result(tool_result.clone());
            runtime.run_phase_with_context(after_ctx)?;
            if termination_flag.is_set() {
                break;
            }

            let tool_content = tool_result_to_content(&tool_result);
            messages.push(Arc::new(Message::tool(&call.id, tool_content)));
        }

        // Check termination after tool loop
        if let Some(reason) = termination_flag.take() {
            break reason;
        }

        if suspended {
            // Transition run to Waiting
            commit_update::<RunLifecycleSlot>(
                store,
                RunLifecycleUpdate::SetWaiting {
                    updated_at: now_ms(),
                },
            )?;
            complete_step(store, runtime, &mut events, &messages, &run_identity)?;
            break TerminationReason::Suspended;
        }

        complete_step(store, runtime, &mut events, &messages, &run_identity)?;
        if let Some(reason) = termination_flag.take() {
            break reason;
        }
    };

    // --- Run lifecycle: Done (unless Suspended → Waiting, not Done) ---
    let (target_status, done_reason) = termination.to_run_status();
    if target_status.is_terminal() {
        commit_update::<RunLifecycleSlot>(
            store,
            RunLifecycleUpdate::Done {
                done_reason: done_reason.unwrap_or_else(|| "unknown".into()),
                updated_at: now_ms(),
            },
        )?;
    }

    runtime.run_phase_with_context(make_ctx(Phase::RunEnd, &messages, &run_identity))?;

    let response = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| m.content.clone())
        .unwrap_or_default();

    events.push(AgentEvent::RunFinish {
        thread_id: run_identity.thread_id.clone(),
        run_id: run_identity.run_id.clone(),
        result: Some(serde_json::json!({"response": response})),
        termination: termination.clone(),
    });

    Ok(AgentRunResult {
        response,
        termination,
        events,
        steps,
    })
}

// -- Helpers --

fn complete_step(
    store: &crate::state::StateStore,
    runtime: &PhaseRuntime,
    events: &mut Vec<AgentEvent>,
    messages: &[Arc<Message>],
    run_identity: &RunIdentity,
) -> Result<(), AgentLoopError> {
    commit_update::<RunLifecycleSlot>(
        store,
        RunLifecycleUpdate::StepCompleted {
            updated_at: now_ms(),
        },
    )?;
    let ctx = PhaseContext::new(Phase::StepEnd, store.snapshot())
        .with_run_identity(run_identity.clone())
        .with_messages(messages.to_vec());
    runtime.run_phase_with_context(ctx)?;
    events.push(AgentEvent::StepEnd);
    Ok(())
}

fn commit_update<S: crate::state::StateSlot>(
    store: &crate::state::StateStore,
    update: S::Update,
) -> Result<(), crate::error::StateError> {
    let mut patch = MutationBatch::new();
    patch.update::<S>(update);
    store.commit(patch)?;
    Ok(())
}

fn commit_tool_call_transition(
    store: &crate::state::StateStore,
    call: &ToolCall,
    status: ToolCallStatus,
) -> Result<(), crate::error::StateError> {
    commit_update::<ToolCallStatesSlot>(
        store,
        ToolCallStatesUpdate::Upsert {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            arguments: call.arguments.clone(),
            status,
            updated_at: now_ms(),
        },
    )
}

/// Execute a tool, returning ToolResult (never crashes the loop).
async fn execute_tool(agent: &AgentConfig, call: &ToolCall) -> ToolResult {
    let Some(tool) = agent.tools.get(&call.name) else {
        return ToolResult::error(&call.name, format!("tool '{}' not found", call.name));
    };

    if let Err(e) = tool.validate_args(&call.arguments) {
        return ToolResult::error(&call.name, e.to_string());
    }

    match tool.execute(call.arguments.clone()).await {
        Ok(result) => result,
        Err(e) => ToolResult::error(&call.name, e.to_string()),
    }
}

fn tool_result_to_content(result: &ToolResult) -> String {
    match &result.message {
        Some(msg) => msg.clone(),
        None => serde_json::to_string(&result.data).unwrap_or_default(),
    }
}
