use super::*;
use crate::contracts::ToolCallContext;

mod run_tool;

pub use run_tool::AgentRunTool;

pub(super) fn agent_tool_result(
    tool_name: &str,
    run_id: &str,
    agent_id: &str,
    status: &str,
    error: Option<&str>,
) -> ToolResult {
    ToolResult::success(
        tool_name,
        json!({
            "run_id": run_id,
            "agent_id": agent_id,
            "status": status,
            "error": error,
        }),
    )
}

pub(super) fn tool_error(tool_name: &str, code: &str, message: impl Into<String>) -> ToolResult {
    ToolResult::error_with_code(tool_name, code, message)
}

#[derive(Debug)]
pub(super) struct ToolArgError {
    code: &'static str,
    message: String,
}

impl ToolArgError {
    pub(super) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub(super) fn into_tool_result(self, tool_name: &str) -> ToolResult {
        tool_error(tool_name, self.code, self.message)
    }
}

pub(super) fn scope_string(scope: Option<&tirea_contract::RunConfig>, key: &str) -> Option<String> {
    scope
        .and_then(|scope: &tirea_contract::RunConfig| scope.value(key))
        .and_then(|value: &serde_json::Value| value.as_str())
        .map(|value| value.to_string())
}

pub(super) fn scope_run_id(scope: Option<&tirea_contract::RunConfig>) -> Option<String> {
    scope_string(scope, SCOPE_RUN_ID_KEY)
}


pub(super) fn parse_caller_messages(
    scope: Option<&tirea_contract::RunConfig>,
) -> Option<Vec<Message>> {
    let value = scope.and_then(|scope| scope.value(SCOPE_CALLER_MESSAGES_KEY))?;
    serde_json::from_value::<Vec<Message>>(value.clone()).ok()
}

pub(super) fn filtered_fork_messages(messages: Vec<Message>) -> Vec<Message> {
    let visible: Vec<Message> = messages
        .into_iter()
        .filter(|m| m.visibility == crate::contracts::thread::Visibility::All)
        .collect();

    let assistant_call_ids: std::collections::HashSet<String> = visible
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .filter_map(|m| m.tool_calls.as_ref())
        .flatten()
        .map(|tc| tc.id.clone())
        .collect();

    let tool_result_ids: std::collections::HashSet<String> = visible
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| m.tool_call_id.clone())
        .collect();

    let paired_ids: std::collections::HashSet<String> = assistant_call_ids
        .intersection(&tool_result_ids)
        .cloned()
        .collect();

    visible
        .into_iter()
        .filter_map(|mut m| {
            if m.role == Role::System {
                return None;
            }

            match m.role {
                Role::Assistant => {
                    if let Some(tool_calls) = m.tool_calls.take() {
                        let filtered_calls: Vec<ToolCall> = tool_calls
                            .into_iter()
                            .filter(|tc| paired_ids.contains(&tc.id))
                            .collect();
                        m.tool_calls = if filtered_calls.is_empty() {
                            None
                        } else {
                            Some(filtered_calls)
                        };
                    }
                    Some(m)
                }
                Role::Tool => {
                    let call_id = m.tool_call_id.as_deref()?;
                    if paired_ids.contains(call_id) {
                        Some(m)
                    } else {
                        None
                    }
                }
                Role::User => Some(m),
                Role::System => None,
            }
        })
        .collect()
}

pub(super) struct LaunchNewRunRequest {
    pub(super) run_id: String,
    pub(super) owner_thread_id: String,
    pub(super) agent_id: String,
    pub(super) parent_run_id: Option<String>,
    pub(super) child_thread_id: String,
    pub(super) messages: Vec<Message>,
    pub(super) initial_state: Option<Value>,
    pub(super) background: bool,
}

pub(super) fn is_target_agent_visible(
    registry: &dyn AgentRegistry,
    target: &str,
    caller: Option<&str>,
    scope: Option<&tirea_contract::RunConfig>,
) -> bool {
    if caller.is_some_and(|c| c == target) {
        return false;
    }
    if !is_scope_allowed(
        scope,
        target,
        SCOPE_ALLOWED_AGENTS_KEY,
        SCOPE_EXCLUDED_AGENTS_KEY,
    ) {
        return false;
    }
    registry.get(target).is_some()
}

pub(super) fn sub_agent_thread_id(run_id: &str) -> String {
    format!("sub-agent-{run_id}")
}
