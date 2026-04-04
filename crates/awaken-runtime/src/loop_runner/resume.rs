//! Resume detection, preparation, and wait logic for suspended tool calls.

use std::sync::Arc;

use crate::cancellation::CancellationToken;
use awaken_contract::StateError;
use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::identity::RunIdentity;
use awaken_contract::contract::message::{Message, ToolCall};
use awaken_contract::contract::suspension::{
    ResumeDecisionAction, ToolCallResume, ToolCallResumeMode, ToolCallStatus,
};
use awaken_contract::contract::tool::ToolCallContext;
use futures::StreamExt;
use futures::channel::mpsc::UnboundedReceiver;

use super::{AgentLoopError, commit_update, now_ms, tool_result_to_content};
use crate::agent::state::{ToolCallStates, ToolCallStatesUpdate};
use crate::registry::ResolvedAgent;
use crate::state::StateCommand;

pub(super) enum WaitOutcome {
    Resumed,
    Cancelled,
    NoDecisionChannel,
}

/// Prepare tool call states for resume.
///
/// For each decision:
/// - `Cancel` → status = Cancelled
/// - `Resume` → status = Resuming, arguments adjusted per `resume_mode`:
///   - `ReplayToolCall`: keep original arguments
///   - `PassDecisionToTool` / `UseDecisionAsToolResult`: arguments = decision.result
///
/// When `resume_mode_override` is `None`, the resume mode is read from each
/// tool call's stored state (set when the `SuspendTicket` was applied).
/// When `Some(mode)`, the override is used for all decisions.
///
/// `detect_and_replay_resume` then re-executes all Resuming calls through the
/// full tool pipeline (BeforeToolExecute → execute → AfterToolExecute).
/// For `UseDecisionAsToolResult`, the frontend tool plugin intercepts in
/// BeforeToolExecute and returns `SetResult` to skip actual execution.
pub fn prepare_resume(
    store: &crate::state::StateStore,
    decisions: Vec<(String, ToolCallResume)>,
    resume_mode_override: Option<ToolCallResumeMode>,
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

        // Use the override if provided, otherwise read from the stored state.
        // Stored default is ReplayToolCall (for tools that suspended without a ticket).
        let resume_mode = resume_mode_override.unwrap_or(call_state.resume_mode);

        // Adjust arguments based on resume mode
        let arguments = match (&resume_mode, &decision.action) {
            (
                ToolCallResumeMode::PassDecisionToTool
                | ToolCallResumeMode::UseDecisionAsToolResult,
                ResumeDecisionAction::Resume,
            ) => normalize_decision_result(&decision.result, &call_state.arguments),
            _ => call_state.arguments.clone(),
        };

        commit_update::<ToolCallStates>(
            store,
            ToolCallStatesUpdate::Upsert {
                call_id: call_id.clone(),
                tool_name: call_state.tool_name.clone(),
                arguments,
                status: match decision.action {
                    ResumeDecisionAction::Resume => ToolCallStatus::Resuming,
                    ResumeDecisionAction::Cancel => ToolCallStatus::Cancelled,
                },
                updated_at: now_ms(),
                resume_mode,
            },
        )?;
    }
    Ok(())
}

/// Normalize decision result for use as tool arguments.
///
/// If the decision result is a boolean (simple approve/reject), fall back to
/// the original arguments. Otherwise use the decision result as-is.
/// Mirrors uncarve's `normalize_decision_tool_result`.
fn normalize_decision_result(
    response: &serde_json::Value,
    fallback_arguments: &serde_json::Value,
) -> serde_json::Value {
    match response {
        serde_json::Value::Bool(_) => fallback_arguments.clone(),
        value => value.clone(),
    }
}

/// Detect Resuming tool calls in state and replay them.
///
/// Called at loop startup. All Resuming calls are re-executed through the
/// standard tool pipeline. The `arguments` field already reflects the
/// resume mode (set by `prepare_resume`).
pub(super) async fn detect_and_replay_resume(
    agent: &ResolvedAgent,
    store: &crate::state::StateStore,
    run_identity: &RunIdentity,
    messages: &mut Vec<Arc<Message>>,
) -> Result<(), AgentLoopError> {
    let tool_call_states = store.read::<ToolCallStates>().unwrap_or_default();

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
        agent_spec: agent.spec.clone(),
        snapshot: store.snapshot(),
        activity_sink: None,
        cancellation_token: None,
    };

    for (call_id, call_state) in resuming {
        // Re-execute with stored arguments (original or decision-adjusted)
        let call = ToolCall::new(call_id, &call_state.tool_name, call_state.arguments.clone());
        let mut tool_ctx = resume_tool_ctx.clone();
        tool_ctx.call_id = call_id.to_string();
        tool_ctx.tool_name = call_state.tool_name.clone();
        let output =
            crate::execution::executor::execute_single_tool(&agent.tools, &call, &tool_ctx).await;

        let status = if output.result.is_success() {
            ToolCallStatus::Succeeded
        } else {
            ToolCallStatus::Failed
        };

        // Merge tool command + lifecycle update
        let mut cmd = StateCommand::new();
        cmd.update::<ToolCallStates>(ToolCallStatesUpdate::Upsert {
            call_id: call_id.clone(),
            tool_name: call_state.tool_name.clone(),
            arguments: call_state.arguments.clone(),
            status,
            updated_at: now_ms(),
            resume_mode: call_state.resume_mode,
        });
        if !output.command.is_empty() {
            cmd.extend(output.command)
                .map_err(AgentLoopError::PhaseError)?;
        }
        store
            .commit(cmd.patch)
            .map_err(AgentLoopError::PhaseError)?;

        messages.push(Arc::new(Message::tool(
            call_id,
            tool_result_to_content(&output.result),
        )));
    }

    Ok(())
}

async fn emit_decision_events_and_messages(
    store: &crate::state::StateStore,
    sink: &dyn EventSink,
    messages: &mut Vec<Arc<Message>>,
    decisions: &[(String, ToolCallResume)],
) -> Result<(), AgentLoopError> {
    let tool_call_states = store.read::<ToolCallStates>().unwrap_or_default();

    for (call_id, decision) in decisions {
        let Some(call_state) = tool_call_states.calls.get(call_id) else {
            continue;
        };

        match decision.action {
            ResumeDecisionAction::Cancel => {
                sink.emit(AgentEvent::ToolCallResumed {
                    target_id: call_id.clone(),
                    result: decision.result.clone(),
                })
                .await;
                messages.push(Arc::new(Message::tool(
                    call_id,
                    serde_json::to_string(&decision.result).unwrap_or_else(|_| "null".into()),
                )));
            }
            ResumeDecisionAction::Resume
                if call_state.resume_mode != ToolCallResumeMode::ReplayToolCall =>
            {
                sink.emit(AgentEvent::ToolCallResumed {
                    target_id: call_id.clone(),
                    result: decision.result.clone(),
                })
                .await;
            }
            ResumeDecisionAction::Resume => {}
        }
    }

    Ok(())
}

pub(super) async fn wait_for_resume_or_cancel(
    decision_rx: Option<&mut UnboundedReceiver<Vec<(String, ToolCallResume)>>>,
    cancellation_token: Option<&CancellationToken>,
    store: &crate::state::StateStore,
    agent: &ResolvedAgent,
    sink: &dyn EventSink,
    run_identity: &RunIdentity,
    messages: &mut Vec<Arc<Message>>,
) -> Result<WaitOutcome, AgentLoopError> {
    let Some(rx) = decision_rx else {
        return Ok(WaitOutcome::NoDecisionChannel);
    };

    loop {
        let first_batch = if let Some(token) = cancellation_token {
            tokio::select! {
                biased;
                _ = token.cancelled() => return Ok(WaitOutcome::Cancelled),
                next = rx.next() => match next {
                    Some(v) => v,
                    None => return Ok(WaitOutcome::NoDecisionChannel),
                },
            }
        } else {
            match rx.next().await {
                Some(v) => v,
                None => return Ok(WaitOutcome::NoDecisionChannel),
            }
        };

        let mut decisions = first_batch;
        while let Ok(batch) = rx.try_recv() {
            decisions.extend(batch);
        }

        if decisions.is_empty() {
            continue;
        }

        // Each tool call's resume_mode is read from its stored state (set by SuspendTicket).
        // Defaults to ReplayToolCall if no ticket was stored (legacy path).
        emit_decision_events_and_messages(store, sink, messages, &decisions).await?;
        prepare_resume(store, decisions, None)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_decision_result_uses_value_for_object() {
        let decision = json!({"key": "value"});
        let fallback = json!({"original": true});
        let result = normalize_decision_result(&decision, &fallback);
        assert_eq!(result, json!({"key": "value"}));
    }

    #[test]
    fn normalize_decision_result_falls_back_for_boolean() {
        let decision = json!(true);
        let fallback = json!({"original": "args"});
        let result = normalize_decision_result(&decision, &fallback);
        assert_eq!(result, json!({"original": "args"}));

        // Also test false
        let decision_false = json!(false);
        let result_false = normalize_decision_result(&decision_false, &fallback);
        assert_eq!(result_false, json!({"original": "args"}));
    }

    #[test]
    fn normalize_decision_result_uses_value_for_string() {
        let decision = json!("custom result");
        let fallback = json!({"original": true});
        let result = normalize_decision_result(&decision, &fallback);
        assert_eq!(result, json!("custom result"));
    }

    #[test]
    fn normalize_decision_result_uses_value_for_null() {
        let decision = json!(null);
        let fallback = json!({"original": true});
        let result = normalize_decision_result(&decision, &fallback);
        assert_eq!(result, json!(null));
    }
}
