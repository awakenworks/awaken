use super::AgentLoopError;
use crate::contracts::runtime::phase::StepContext;
use crate::contracts::runtime::state::{reduce_state_actions, AnyStateAction, ScopeContext};
use crate::contracts::runtime::tool_call::Tool;
use crate::contracts::runtime::SuspendedCall;
use crate::contracts::thread::{Message, Role};
use crate::contracts::RunAction;
use crate::contracts::RunContext;
use crate::runtime::control::{
    ToolCallResume, ToolCallState,
    ToolCallStatus,
};
use crate::contracts::runtime::tool_call::tool_call_states_from_state;
use tirea_state::{parse_path, Op, Patch, TrackedPatch};

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

fn is_pending_approval_placeholder(msg: &Message) -> bool {
    msg.role == Role::Tool
        && msg
            .content
            .contains("is awaiting approval. Execution paused.")
}

pub(super) fn build_messages(step: &StepContext<'_>, system_prompt: &str) -> Vec<Message> {
    use crate::contracts::runtime::inference::InferenceContext;

    let mut messages = Vec::new();

    let inf = step.extensions.get::<InferenceContext>();
    let system_ctx = inf.map(|i| &i.system_context[..]).unwrap_or(&[]);
    let session_ctx = inf.map(|i| &i.session_context[..]).unwrap_or(&[]);

    let system = if system_ctx.is_empty() {
        system_prompt.to_string()
    } else {
        format!("{}\n\n{}", system_prompt, system_ctx.join("\n"))
    };

    if !system.is_empty() {
        messages.push(Message::system(system));
    }

    for ctx in session_ctx {
        messages.push(Message::system(ctx.clone()));
    }

    // Collect all tool_call IDs issued by the assistant so we can filter
    // orphaned tool results (e.g. from intercepted pseudo-tool invocations
    // like PermissionConfirm whose call IDs the LLM never issued).
    let known_tool_call_ids: HashSet<&str> = step
        .messages()
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .filter_map(|m| m.tool_calls.as_ref())
        .flatten()
        .map(|tc| tc.id.as_str())
        .collect();

    // When a frontend tool pending placeholder is followed by a real tool result
    // for the same call_id, keep only the real result in inference context.
    // This preserves append-only persisted history while avoiding stale
    // "awaiting approval" text from biasing subsequent model turns.
    let mut pending_placeholder_ids = HashSet::new();
    let mut resolved_result_ids = HashSet::new();
    for msg in step.messages() {
        let Some(tc_id) = msg.tool_call_id.as_deref() else {
            continue;
        };
        if !known_tool_call_ids.contains(tc_id) {
            continue;
        }
        if is_pending_approval_placeholder(msg) {
            pending_placeholder_ids.insert(tc_id.to_string());
        } else if msg.role == Role::Tool {
            resolved_result_ids.insert(tc_id.to_string());
        }
    }
    let superseded_pending_ids: HashSet<String> = pending_placeholder_ids
        .intersection(&resolved_result_ids)
        .cloned()
        .collect();

    for msg in step.messages() {
        if msg.role == Role::Tool {
            if let Some(ref tc_id) = msg.tool_call_id {
                if !known_tool_call_ids.contains(tc_id.as_str()) {
                    continue;
                }
                if superseded_pending_ids.contains(tc_id) && is_pending_approval_placeholder(msg) {
                    continue;
                }
            }
        }
        messages.push((**msg).clone());
    }

    messages
}

pub(super) type InferenceInputs = (Vec<Message>, Vec<String>, RunAction);

pub(super) fn inference_inputs_from_step(
    step: &mut StepContext<'_>,
    system_prompt: &str,
) -> InferenceInputs {
    use crate::contracts::runtime::inference::InferenceContext;

    let messages = build_messages(step, system_prompt);
    let inf = step.extensions.get::<InferenceContext>();
    let tools = inf.map(|i| &i.tools[..]).unwrap_or(&[]);
    let filtered_tools = tools.iter().map(|td| td.id.clone()).collect::<Vec<_>>();
    let run_action = step.run_action();
    (messages, filtered_tools, run_action)
}

pub(super) fn build_request_for_filtered_tools(
    messages: &[Message],
    tools: &HashMap<String, Arc<dyn Tool>>,
    filtered_tools: &[String],
) -> genai::chat::ChatRequest {
    let filtered: HashSet<&str> = filtered_tools.iter().map(String::as_str).collect();
    let filtered_tool_refs: Vec<&dyn Tool> = tools
        .values()
        .filter(|t| filtered.contains(t.descriptor().id.as_str()))
        .map(|t| t.as_ref())
        .collect();
    crate::engine::convert::build_request(messages, &filtered_tool_refs)
}

#[allow(dead_code)]
pub(super) fn suspended_calls_from_ctx(run_ctx: &RunContext) -> HashMap<String, SuspendedCall> {
    run_ctx.suspended_calls()
}

pub(super) fn tool_call_states_from_ctx(run_ctx: &RunContext) -> HashMap<String, ToolCallState> {
    run_ctx
        .snapshot()
        .map(|s| tool_call_states_from_state(&s))
        .unwrap_or_default()
}

pub(super) struct ToolCallStateSeed<'a> {
    pub(super) call_id: &'a str,
    pub(super) tool_name: &'a str,
    pub(super) arguments: &'a Value,
    pub(super) status: ToolCallStatus,
    pub(super) resume_token: Option<String>,
}

pub(super) struct ToolCallStateTransition {
    pub(super) status: ToolCallStatus,
    pub(super) resume_token: Option<String>,
    pub(super) resume: Option<ToolCallResume>,
    pub(super) updated_at: u64,
}

pub(super) fn transition_tool_call_state(
    current: Option<ToolCallState>,
    seed: ToolCallStateSeed<'_>,
    transition: ToolCallStateTransition,
) -> Option<ToolCallState> {
    let mut tool_state = current.unwrap_or_else(|| ToolCallState {
        call_id: seed.call_id.to_string(),
        tool_name: seed.tool_name.to_string(),
        arguments: seed.arguments.clone(),
        status: seed.status,
        resume_token: seed.resume_token.clone(),
        resume: None,
        scratch: Value::Null,
        updated_at: transition.updated_at,
    });
    if !tool_state.status.can_transition_to(transition.status) {
        return None;
    }

    tool_state.call_id = seed.call_id.to_string();
    tool_state.tool_name = seed.tool_name.to_string();
    tool_state.arguments = seed.arguments.clone();
    tool_state.status = transition.status;
    tool_state.resume_token = transition.resume_token;
    tool_state.resume = transition.resume;
    tool_state.updated_at = transition.updated_at;

    Some(tool_state)
}

pub(super) fn upsert_tool_call_state(
    base_state: &Value,
    call_id: &str,
    tool_state: ToolCallState,
) -> Result<TrackedPatch, AgentLoopError> {
    if call_id.trim().is_empty() {
        return Err(AgentLoopError::StateError(
            "failed to upsert tool call state: call_id must not be empty".to_string(),
        ));
    }

    let scope_path = format!("__tool_call_scope.{}.tool_call_state", call_id);
    let path = parse_path(&scope_path);
    let value = serde_json::to_value(tool_state).map_err(|e| {
        AgentLoopError::StateError(format!(
            "failed to serialize tool call state for '{call_id}': {e}"
        ))
    })?;
    let raw = TrackedPatch::new(tirea_state::Patch::with_ops(vec![Op::set(path, value)]))
        .with_source("agent_loop");
    let mut reduced = reduce_state_actions(
        vec![AnyStateAction::Patch(raw)],
        base_state,
        "agent_loop",
        &ScopeContext::run(),
    )
    .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
    match reduced.pop() {
        Some(patch) => Ok(patch),
        None => Ok(TrackedPatch::new(Patch::new()).with_source("agent_loop")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::control::{ResumeDecisionAction, ToolCallStatus};
    use serde_json::json;
    use tirea_state::apply_patch;

    fn sample_state(call_id: &str, status: ToolCallStatus) -> ToolCallState {
        ToolCallState {
            call_id: call_id.to_string(),
            tool_name: "echo".to_string(),
            arguments: json!({"msg": call_id}),
            status,
            resume_token: None,
            resume: None,
            scratch: Value::Null,
            updated_at: 1,
        }
    }

    #[test]
    fn upsert_tool_call_state_generates_single_call_scoped_patch() {
        let state = json!({
            "__tool_call_scope": {
                "call_a": {
                    "tool_call_state": sample_state("call_a", ToolCallStatus::Suspended)
                },
                "call_b": {
                    "tool_call_state": sample_state("call_b", ToolCallStatus::Suspended)
                }
            }
        });

        let updated = sample_state("call_a", ToolCallStatus::Resuming);
        let patch = upsert_tool_call_state(&state, "call_a", updated).expect("patch should build");
        let ops = patch.patch().ops();
        assert_eq!(ops.len(), 1);
        assert_eq!(
            ops[0].path().to_string(),
            "$.__tool_call_scope.call_a.tool_call_state"
        );

        let merged = apply_patch(&state, patch.patch()).expect("patch should apply");
        assert_eq!(
            merged["__tool_call_scope"]["call_a"]["tool_call_state"]["status"],
            json!("resuming")
        );
        assert_eq!(
            merged["__tool_call_scope"]["call_b"]["tool_call_state"]["status"],
            json!("suspended")
        );
    }

    #[test]
    fn upsert_tool_call_state_rejects_empty_call_id() {
        let error = upsert_tool_call_state(&json!({}), " ", sample_state("x", ToolCallStatus::New))
            .expect_err("empty call_id must fail");
        let AgentLoopError::StateError(message) = error else {
            panic!("unexpected error type");
        };
        assert!(message.contains("call_id must not be empty"));
    }

    #[test]
    fn transition_tool_call_state_applies_seed_and_runtime_fields() {
        let transitioned = transition_tool_call_state(
            None,
            ToolCallStateSeed {
                call_id: "call_1",
                tool_name: "echo",
                arguments: &json!({"message":"hi"}),
                status: ToolCallStatus::Suspended,
                resume_token: Some("resume_token_1".to_string()),
            },
            ToolCallStateTransition {
                status: ToolCallStatus::Resuming,
                resume_token: Some("resume_token_1".to_string()),
                resume: Some(ToolCallResume {
                    decision_id: "decision_1".to_string(),
                    action: ResumeDecisionAction::Resume,
                    result: json!(true),
                    reason: None,
                    updated_at: 42,
                }),
                updated_at: 42,
            },
        )
        .expect("transition should be allowed");

        assert_eq!(transitioned.call_id, "call_1");
        assert_eq!(transitioned.tool_name, "echo");
        assert_eq!(transitioned.arguments, json!({"message":"hi"}));
        assert_eq!(transitioned.status, ToolCallStatus::Resuming);
        assert_eq!(transitioned.resume_token.as_deref(), Some("resume_token_1"));
        assert_eq!(
            transitioned
                .resume
                .as_ref()
                .map(|resume| &resume.decision_id),
            Some(&"decision_1".to_string())
        );
        assert_eq!(transitioned.updated_at, 42);
    }

    #[test]
    fn transition_tool_call_state_rejects_invalid_lifecycle_transition() {
        let current = ToolCallState {
            call_id: "call_1".to_string(),
            tool_name: "echo".to_string(),
            arguments: json!({"message":"done"}),
            status: ToolCallStatus::Succeeded,
            resume_token: None,
            resume: None,
            scratch: Value::Null,
            updated_at: 1,
        };

        let transitioned = transition_tool_call_state(
            Some(current),
            ToolCallStateSeed {
                call_id: "call_1",
                tool_name: "echo",
                arguments: &json!({"message":"done"}),
                status: ToolCallStatus::New,
                resume_token: None,
            },
            ToolCallStateTransition {
                status: ToolCallStatus::Running,
                resume_token: None,
                resume: None,
                updated_at: 2,
            },
        );
        assert!(transitioned.is_none(), "terminal state should not reopen");
    }
}
