use crate::actions::{deny_missing_call_id, deny_tool, request_permission};
use crate::form::{permission_confirmation_ticket, PERMISSION_CONFIRM_TOOL_NAME};
use crate::model::{PermissionEvaluation, ToolPermissionBehavior};
use crate::state::{permission_state_action, PermissionAction};
use serde_json::Value;
use tirea_contract::io::ResumeDecisionAction;
use tirea_contract::runtime::phase::BeforeToolExecuteAction;
use tirea_contract::runtime::state::AnyStateAction;
use tirea_contract::runtime::SuspendedCall;
use tirea_contract::ToolCallDecision;

/// Runtime input required to enforce permission decisions for one tool call.
pub struct PermissionMechanismInput<'a> {
    pub tool_id: &'a str,
    pub tool_args: serde_json::Value,
    pub call_id: Option<&'a str>,
    pub resume_action: Option<ResumeDecisionAction>,
}

/// Mechanism output after combining strategy verdict with runtime state.
pub enum PermissionMechanismDecision {
    Proceed,
    Action(Box<BeforeToolExecuteAction>),
}

fn decision_requests_memory(result: &Value) -> bool {
    result
        .as_object()
        .and_then(|obj| obj.get("remember"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn is_permission_confirmation(suspended_call: &SuspendedCall) -> bool {
    suspended_call
        .ticket
        .pending
        .name
        .eq_ignore_ascii_case(PERMISSION_CONFIRM_TOOL_NAME)
        || suspended_call
            .ticket
            .suspension
            .action
            .eq_ignore_ascii_case("tool:PermissionConfirm")
}

/// Translate a remembered permission decision into a persistent rule mutation.
#[must_use]
pub fn remembered_permission_state_action(
    suspended_call: &SuspendedCall,
    decision: &ToolCallDecision,
) -> Option<AnyStateAction> {
    if suspended_call.tool_name.trim().is_empty()
        || !is_permission_confirmation(suspended_call)
        || !decision_requests_memory(&decision.resume.result)
    {
        return None;
    }

    let behavior = match decision.resume.action {
        ResumeDecisionAction::Resume => ToolPermissionBehavior::Allow,
        ResumeDecisionAction::Cancel => ToolPermissionBehavior::Deny,
    };

    Some(permission_state_action(PermissionAction::SetTool {
        tool_id: suspended_call.tool_name.clone(),
        behavior,
    }))
}

/// Apply runtime permission mechanism to a strategy verdict.
#[must_use]
pub fn enforce_permission(
    input: PermissionMechanismInput<'_>,
    evaluation: &PermissionEvaluation,
) -> PermissionMechanismDecision {
    if input
        .resume_action
        .is_some_and(|action| matches!(action, ResumeDecisionAction::Resume))
    {
        return PermissionMechanismDecision::Proceed;
    }

    match evaluation.behavior {
        ToolPermissionBehavior::Allow => PermissionMechanismDecision::Proceed,
        ToolPermissionBehavior::Deny => {
            PermissionMechanismDecision::Action(Box::new(deny_tool(input.tool_id)))
        }
        ToolPermissionBehavior::Ask => {
            let Some(call_id) = input.call_id.filter(|call_id| !call_id.is_empty()) else {
                return PermissionMechanismDecision::Action(Box::new(deny_missing_call_id()));
            };
            PermissionMechanismDecision::Action(Box::new(request_permission(
                permission_confirmation_ticket(call_id, input.tool_id, input.tool_args),
            )))
        }
    }
}
