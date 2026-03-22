//! Resume detection, preparation, and wait logic for suspended tool calls.

use std::sync::Arc;

use crate::runtime::CancellationToken;
use awaken_contract::StateError;
use awaken_contract::contract::identity::RunIdentity;
use awaken_contract::contract::message::{Message, ToolCall};
use awaken_contract::contract::suspension::{ToolCallResume, ToolCallResumeMode, ToolCallStatus};
use awaken_contract::contract::tool::ToolCallContext;
use futures::StreamExt;
use futures::channel::mpsc::UnboundedReceiver;

use super::super::config::AgentConfig;
use super::super::state::{ToolCallStates, ToolCallStatesUpdate};
use super::{AgentLoopError, commit_update, now_ms, tool_result_to_content};

pub(super) enum WaitOutcome {
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
    use awaken_contract::contract::suspension::ResumeDecisionAction;

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
pub(super) async fn detect_and_replay_resume(
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
        tool_name: String::new(),
        run_identity: run_identity.clone(),
        agent_spec: std::sync::Arc::new(awaken_contract::registry_spec::AgentSpec::default()),
        snapshot: store.snapshot(),
        activity_sink: None,
    };

    for (call_id, call_state) in resuming {
        // Re-execute with the arguments stored in state (may be original or decision payload)
        let call = ToolCall::new(call_id, &call_state.tool_name, call_state.arguments.clone());
        let mut tool_ctx = resume_tool_ctx.clone();
        tool_ctx.call_id = call_id.to_string();
        tool_ctx.tool_name = call_state.tool_name.clone();
        let result =
            crate::agent::executor::execute_single_tool(&agent.tools, &call, &tool_ctx).await;

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

pub(super) async fn wait_for_resume_or_cancel(
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

pub(super) fn has_suspended_calls(store: &crate::state::StateStore) -> bool {
    store
        .read::<ToolCallStates>()
        .map(|s| {
            s.calls
                .values()
                .any(|v| v.status == ToolCallStatus::Suspended)
        })
        .unwrap_or(false)
}
