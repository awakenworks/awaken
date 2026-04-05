//! Resume detection, preparation, and wait logic for suspended tool calls.

use std::sync::Arc;

use crate::cancellation::CancellationToken;
use awaken_contract::StateError;
use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::event_sink::{EventSink, VecEventSink};
use awaken_contract::contract::identity::RunIdentity;
use awaken_contract::contract::message::{Message, ToolCall};
use awaken_contract::contract::suspension::{
    ResumeDecisionAction, ToolCallResume, ToolCallResumeMode, ToolCallStatus,
};
use awaken_contract::contract::tool::ToolResult;
use futures::StreamExt;
use futures::channel::mpsc::UnboundedReceiver;
use serde_json::Value;

use super::step::{StepContext, execute_tools_with_interception};
use super::{AgentLoopError, commit_update, now_ms};
use crate::agent::state::{OptionalField, ToolCallState, ToolCallStates, ToolCallStatesUpdate};
use crate::context::TruncationState;
use crate::phase::PhaseRuntime;
use crate::registry::ResolvedAgent;

pub(super) enum WaitOutcome {
    Resumed {
        effects: ResumeEffects,
        still_suspended: bool,
    },
    Cancelled,
    NoDecisionChannel,
}

#[derive(Debug, Default)]
pub(super) struct ResumeEffects {
    pub events: Vec<AgentEvent>,
}

impl ResumeEffects {
    fn push(&mut self, event: AgentEvent) {
        self.events.push(event);
    }
}

fn resolve_call_target<'a>(
    tool_call_states: &'a crate::agent::state::ToolCallStateMap,
    target_id: &str,
) -> Option<(String, &'a ToolCallState)> {
    if let Some(call_state) = tool_call_states.calls.get(target_id) {
        return Some((target_id.to_string(), call_state));
    }

    tool_call_states
        .calls
        .iter()
        .find(|(_, call_state)| call_state.suspension_id.as_deref() == Some(target_id))
        .map(|(call_id, call_state)| (call_id.clone(), call_state))
}

/// Prepare tool call states for resume.
///
/// For each decision:
/// - `Cancel` → status = Cancelled
/// - `Resume` → status = Resuming
///
/// The runtime stores the full `ToolCallResume` and re-enters the normal tool
/// pipeline on replay. For resume modes that explicitly project decision
/// payloads into tool execution (`PassDecisionToTool` /
/// `UseDecisionAsToolResult`), the stored arguments are updated for backward
/// compatibility while hooks/tools still receive the full `resume_input`.
pub fn prepare_resume(
    store: &crate::state::StateStore,
    decisions: Vec<(String, ToolCallResume)>,
    resume_mode_override: Option<ToolCallResumeMode>,
) -> Result<(), StateError> {
    let tool_call_states = store.read::<ToolCallStates>().unwrap_or_default();
    for (target_id, decision) in decisions {
        let (call_id, call_state) =
            resolve_call_target(&tool_call_states, &target_id).ok_or_else(|| {
                StateError::UnknownKey {
                    key: format!("tool call or suspension {target_id} not found"),
                }
            })?;

        // Use the override if provided, otherwise read from the stored state.
        // Stored default is ReplayToolCall (for tools that suspended without a ticket).
        let resume_mode = resume_mode_override.unwrap_or(call_state.resume_mode);
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
                suspension_id: OptionalField::Preserve,
                suspension_reason: OptionalField::Preserve,
                resume_input: OptionalField::Set(decision),
            },
        )?;
    }
    Ok(())
}

fn normalize_decision_result(
    response: &serde_json::Value,
    fallback_arguments: &serde_json::Value,
) -> serde_json::Value {
    match response {
        serde_json::Value::Bool(_) => fallback_arguments.clone(),
        value => value.clone(),
    }
}

fn tool_result_to_resume_payload(tool_result: &ToolResult) -> Value {
    match tool_result.status {
        awaken_contract::contract::tool::ToolStatus::Success => {
            if tool_result.metadata.is_empty() {
                tool_result.data.clone()
            } else {
                serde_json::json!({
                    "data": tool_result.data,
                    "metadata": tool_result.metadata,
                })
            }
        }
        awaken_contract::contract::tool::ToolStatus::Error => {
            if let Some(message) = tool_result.message.as_ref() {
                serde_json::json!({ "error": message })
            } else {
                tool_result.data.clone()
            }
        }
        awaken_contract::contract::tool::ToolStatus::Pending => Value::Null,
    }
}

fn drain_resume_effects(raw_events: Vec<AgentEvent>) -> Vec<AgentEvent> {
    raw_events
        .into_iter()
        .map(|event| match event {
            AgentEvent::ToolCallDone {
                id,
                result,
                outcome,
                ..
            } if outcome != awaken_contract::contract::suspension::ToolCallOutcome::Suspended => {
                AgentEvent::ToolCallResumed {
                    target_id: id,
                    result: tool_result_to_resume_payload(&result),
                }
            }
            other => other,
        })
        .collect()
}

fn emit_cancelled_resumes(
    store: &crate::state::StateStore,
    messages: &mut Vec<Arc<Message>>,
) -> Result<ResumeEffects, AgentLoopError> {
    let tool_call_states = store.read::<ToolCallStates>().unwrap_or_default();
    let mut cancelled: Vec<_> = tool_call_states
        .calls
        .iter()
        .filter(|(_, state)| {
            state.status == ToolCallStatus::Cancelled && state.resume_input.is_some()
        })
        .map(|(call_id, state)| (call_id.clone(), state.clone()))
        .collect();
    cancelled.sort_by(|left, right| left.0.cmp(&right.0));

    let mut effects = ResumeEffects::default();

    for (call_id, call_state) in cancelled {
        let result = call_state
            .resume_input
            .as_ref()
            .map(|resume| resume.result.clone())
            .unwrap_or(Value::Null);
        effects.push(AgentEvent::ToolCallResumed {
            target_id: call_id.clone(),
            result: result.clone(),
        });
        messages.push(Arc::new(Message::tool(
            &call_id,
            serde_json::to_string(&result).unwrap_or_else(|_| "null".into()),
        )));
    }

    Ok(effects)
}

pub(super) async fn detect_and_replay_resume(
    agent: &ResolvedAgent,
    runtime: &PhaseRuntime,
    run_identity: &RunIdentity,
    messages: &mut Vec<Arc<Message>>,
) -> Result<ResumeEffects, AgentLoopError> {
    let store = runtime.store();
    let mut effects = emit_cancelled_resumes(store, messages)?;
    let tool_call_states = store.read::<ToolCallStates>().unwrap_or_default();

    let mut resuming: Vec<_> = tool_call_states
        .calls
        .iter()
        .filter(|(_, state)| state.status == ToolCallStatus::Resuming)
        .map(|(call_id, state)| (call_id.clone(), state.clone()))
        .collect();
    resuming.sort_by(|left, right| left.0.cmp(&right.0));

    if resuming.is_empty() {
        return Ok(effects);
    }

    let mut agent = agent.clone();
    let run_overrides = None;
    let mut total_input_tokens = 0;
    let mut total_output_tokens = 0;
    let mut truncation_state = TruncationState::new();
    let run_created_at = now_ms();

    for (call_id, call_state) in resuming {
        let sink = Arc::new(VecEventSink::new());
        let sink_dyn: Arc<dyn EventSink> = sink.clone();
        let call = ToolCall::new(
            &call_id,
            &call_state.tool_name,
            call_state.arguments.clone(),
        );
        let mut step_ctx = StepContext {
            agent: &mut agent,
            messages,
            runtime,
            sink: sink_dyn,
            checkpoint_store: None,
            run_identity,
            cancellation_token: None,
            run_overrides: &run_overrides,
            total_input_tokens: &mut total_input_tokens,
            total_output_tokens: &mut total_output_tokens,
            truncation_state: &mut truncation_state,
            run_created_at,
        };

        let _ = execute_tools_with_interception(&mut step_ctx, std::slice::from_ref(&call)).await?;
        effects.events.extend(drain_resume_effects(sink.take()));
    }

    Ok(effects)
}

pub(super) async fn wait_for_resume_or_cancel(
    decision_rx: Option<&mut UnboundedReceiver<Vec<(String, ToolCallResume)>>>,
    cancellation_token: Option<&CancellationToken>,
    runtime: &PhaseRuntime,
    agent: &ResolvedAgent,
    run_identity: &RunIdentity,
    messages: &mut Vec<Arc<Message>>,
) -> Result<WaitOutcome, AgentLoopError> {
    let store = runtime.store();
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
        loop {
            match rx.try_recv() {
                Ok(batch) => decisions.extend(batch),
                Err(_) => break,
            }
        }

        if decisions.is_empty() {
            continue;
        }

        prepare_resume(store, decisions, None)?;
        let effects = detect_and_replay_resume(agent, runtime, run_identity, messages).await?;
        return Ok(WaitOutcome::Resumed {
            still_suspended: has_suspended_calls(store),
            effects,
        });
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
    use crate::loop_runner::LoopStatePlugin;
    use crate::state::{MutationBatch, StateStore};
    use serde_json::json;

    #[test]
    fn normalize_decision_result_falls_back_for_boolean() {
        let fallback = json!({"original": "args"});
        assert_eq!(normalize_decision_result(&json!(true), &fallback), fallback);
        assert_eq!(
            normalize_decision_result(&json!(false), &fallback),
            fallback
        );
    }

    #[test]
    fn normalize_decision_result_uses_non_boolean_payload() {
        let fallback = json!({"original": true});
        assert_eq!(
            normalize_decision_result(&json!({"key": "value"}), &fallback),
            json!({"key": "value"})
        );
        assert_eq!(
            normalize_decision_result(&json!("custom"), &fallback),
            json!("custom")
        );
    }

    #[test]
    fn prepare_resume_accepts_suspension_id_targets() {
        let store = StateStore::new();
        store
            .install_plugin(LoopStatePlugin)
            .expect("install loop state plugin");

        let mut patch = MutationBatch::new();
        patch.update::<ToolCallStates>(ToolCallStatesUpdate::Upsert {
            call_id: "c1".into(),
            tool_name: "dangerous".into(),
            arguments: json!({"path": "/tmp/demo"}),
            status: ToolCallStatus::Suspended,
            updated_at: 1,
            resume_mode: ToolCallResumeMode::ReplayToolCall,
            suspension_id: OptionalField::Set("perm_c1".into()),
            suspension_reason: OptionalField::Set("tool:PermissionConfirm".into()),
            resume_input: OptionalField::Clear,
        });
        store.commit(patch).expect("seed suspended tool call");

        prepare_resume(
            &store,
            vec![(
                "perm_c1".into(),
                ToolCallResume {
                    decision_id: "d1".into(),
                    action: awaken_contract::contract::suspension::ResumeDecisionAction::Resume,
                    result: json!({"approved": true}),
                    reason: None,
                    updated_at: 2,
                },
            )],
            None,
        )
        .expect("prepare resume");

        let tool_call_states = store.read::<ToolCallStates>().unwrap_or_default();
        let call = tool_call_states.calls.get("c1").expect("tool call state");
        assert_eq!(call.status, ToolCallStatus::Resuming);
        assert_eq!(call.suspension_id.as_deref(), Some("perm_c1"));
        assert_eq!(
            call.suspension_reason.as_deref(),
            Some("tool:PermissionConfirm")
        );
        assert_eq!(
            call.resume_input.as_ref().map(|resume| &resume.result),
            Some(&json!({"approved": true}))
        );
        assert_eq!(call.arguments, json!({"path": "/tmp/demo"}));
    }
}
