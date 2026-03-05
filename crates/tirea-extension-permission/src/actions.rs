use tirea_contract::runtime::phase::{BeforeInferenceAction, BeforeToolExecuteAction};
use tirea_contract::runtime::tool_call::gate::SuspendTicket;

/// Block tool execution with a denial reason.
pub fn deny(reason: impl Into<String>) -> BeforeToolExecuteAction {
    BeforeToolExecuteAction::Block(reason.into())
}

/// Block tool execution for an explicitly denied tool.
pub fn deny_tool(tool_id: &str) -> BeforeToolExecuteAction {
    deny(format!("Tool '{}' is denied", tool_id))
}

/// Suspend tool execution pending user permission confirmation.
pub fn request_permission(ticket: SuspendTicket) -> BeforeToolExecuteAction {
    BeforeToolExecuteAction::Suspend(ticket)
}

/// Block tool execution due to policy (out-of-scope).
pub fn reject_out_of_scope(tool_id: &str) -> BeforeToolExecuteAction {
    deny(format!(
        "Tool '{}' is not allowed by current policy",
        tool_id
    ))
}

/// Block tool execution when permission check prerequisites fail (missing call id).
pub fn deny_missing_call_id() -> BeforeToolExecuteAction {
    deny("Permission check requires non-empty tool call id")
}

/// Apply tool policy: keep only allowed tools, remove excluded ones.
pub fn apply_tool_policy(
    allowed: Option<Vec<String>>,
    excluded: Option<Vec<String>>,
) -> Vec<BeforeInferenceAction> {
    let mut actions = vec![];
    if let Some(allowed) = allowed {
        actions.push(BeforeInferenceAction::IncludeOnlyTools(allowed));
    }
    if let Some(excluded) = excluded {
        for id in excluded {
            actions.push(BeforeInferenceAction::ExcludeTool(id));
        }
    }
    actions
}
