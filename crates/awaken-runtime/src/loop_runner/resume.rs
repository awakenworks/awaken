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
use futures::StreamExt;
use futures::channel::mpsc::UnboundedReceiver;
use serde_json::Value;

use super::step::{StepContext, execute_tools_with_interception};
use super::{AgentLoopError, commit_update, now_ms};
use crate::agent::state::{ToolCallState, ToolCallStates, ToolCallStatesUpdate};
use crate::context::TruncationState;
use crate::phase::PhaseRuntime;
use crate::registry::ResolvedAgent;

pub(super) enum WaitOutcome {
    Resumed,
    Cancelled,
    NoDecisionChannel,
    InboxMessages(Vec<Value>),
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
/// pipeline on replay. Hooks and tools read `resume_input` from the injected
/// context instead of relying on stored argument rewrites.
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

        commit_update::<ToolCallStates>(
            store,
            ToolCallStatesUpdate::Put(
                ToolCallState::new(
                    call_id.clone(),
                    call_state.tool_name.clone(),
                    call_state.arguments.clone(),
                    match decision.action {
                        ResumeDecisionAction::Resume => ToolCallStatus::Resuming,
                        ResumeDecisionAction::Cancel => ToolCallStatus::Cancelled,
                    },
                    now_ms(),
                )
                .with_resume_mode(resume_mode)
                .with_suspension(
                    call_state.suspension_id.clone(),
                    call_state.suspension_reason.clone(),
                )
                .with_resume_input(Some(decision)),
            ),
        )?;
    }
    Ok(())
}

async fn emit_cancelled_resumes(
    sink: Arc<dyn EventSink>,
    store: &crate::state::StateStore,
    messages: &mut Vec<Arc<Message>>,
) -> Result<(), AgentLoopError> {
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

    for (call_id, call_state) in cancelled {
        let result = call_state
            .resume_input
            .as_ref()
            .map(|resume| resume.result.clone())
            .unwrap_or(Value::Null);
        sink.emit(AgentEvent::ToolCallResumed {
            target_id: call_id.clone(),
            result: result.clone(),
        })
        .await;
        messages.push(Arc::new(Message::tool(
            &call_id,
            serde_json::to_string(&result).unwrap_or_else(|_| "null".into()),
        )));

        commit_update::<ToolCallStates>(
            store,
            ToolCallStatesUpdate::Put(
                ToolCallState::new(
                    call_state.call_id,
                    call_state.tool_name,
                    call_state.arguments,
                    ToolCallStatus::Cancelled,
                    now_ms(),
                )
                .with_resume_mode(call_state.resume_mode),
            ),
        )?;
    }

    Ok(())
}

pub(super) async fn detect_and_replay_resume(
    agent: &ResolvedAgent,
    runtime: &PhaseRuntime,
    run_identity: &RunIdentity,
    messages: &mut Vec<Arc<Message>>,
    sink: Arc<dyn EventSink>,
) -> Result<(), AgentLoopError> {
    let store = runtime.store();
    emit_cancelled_resumes(sink.clone(), store, messages).await?;
    let tool_call_states = store.read::<ToolCallStates>().unwrap_or_default();

    let mut resuming: Vec<_> = tool_call_states
        .calls
        .iter()
        .filter(|(_, state)| state.status == ToolCallStatus::Resuming)
        .map(|(call_id, state)| (call_id.clone(), state.clone()))
        .collect();
    resuming.sort_by(|left, right| left.0.cmp(&right.0));

    if resuming.is_empty() {
        return Ok(());
    }

    let mut agent = agent.clone();
    let run_overrides = None;
    let mut total_input_tokens = 0;
    let mut total_output_tokens = 0;
    let mut truncation_state = TruncationState::new();
    let run_created_at = now_ms();

    for (call_id, call_state) in resuming {
        let call = ToolCall::new(
            &call_id,
            &call_state.tool_name,
            call_state.arguments.clone(),
        );
        let mut step_ctx = StepContext {
            agent: &mut agent,
            messages,
            runtime,
            sink: sink.clone(),
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
    }

    Ok(())
}

pub(super) async fn wait_for_resume_or_cancel(
    decision_rx: Option<&mut UnboundedReceiver<Vec<(String, ToolCallResume)>>>,
    inbox: Option<&mut crate::inbox::InboxReceiver>,
    cancellation_token: Option<&CancellationToken>,
    runtime: &PhaseRuntime,
) -> Result<WaitOutcome, AgentLoopError> {
    let store = runtime.store();
    let Some(rx) = decision_rx else {
        return Ok(WaitOutcome::NoDecisionChannel);
    };
    let mut inbox = inbox;

    loop {
        let first_batch = if let Some(inbox_rx) = inbox.as_deref_mut() {
            enum WaitInput {
                Decisions(Option<Vec<(String, ToolCallResume)>>),
                Inbox(Option<Value>),
            }

            let next = if let Some(token) = cancellation_token {
                tokio::select! {
                    biased;
                    _ = token.cancelled() => return Ok(WaitOutcome::Cancelled),
                    next = rx.next() => WaitInput::Decisions(next),
                    msg = inbox_rx.recv_or_cancel(None) => WaitInput::Inbox(msg),
                }
            } else {
                tokio::select! {
                    next = rx.next() => WaitInput::Decisions(next),
                    msg = inbox_rx.recv_or_cancel(None) => WaitInput::Inbox(msg),
                }
            };

            match next {
                WaitInput::Decisions(Some(v)) => v,
                WaitInput::Decisions(None) => return Ok(WaitOutcome::NoDecisionChannel),
                WaitInput::Inbox(Some(msg)) => {
                    let mut messages = vec![msg];
                    messages.extend(inbox_rx.drain());
                    return Ok(WaitOutcome::InboxMessages(messages));
                }
                WaitInput::Inbox(None) => {
                    inbox = None;
                    continue;
                }
            }
        } else if let Some(token) = cancellation_token {
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
        return Ok(WaitOutcome::Resumed);
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
    fn prepare_resume_accepts_suspension_id_targets() {
        let store = StateStore::new();
        store
            .install_plugin(LoopStatePlugin)
            .expect("install loop state plugin");

        let mut patch = MutationBatch::new();
        patch.update::<ToolCallStates>(ToolCallStatesUpdate::Put(
            ToolCallState::new(
                "c1",
                "dangerous",
                json!({"path": "/tmp/demo"}),
                ToolCallStatus::Suspended,
                1,
            )
            .with_resume_mode(ToolCallResumeMode::ReplayToolCall)
            .with_suspension(
                Some("perm_c1".into()),
                Some("tool:PermissionConfirm".into()),
            ),
        ));
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
