use super::state::ToolPermissionBehavior;
use tirea_contract::runtime::action::Action;
use tirea_contract::runtime::inference::InferenceContext;
use tirea_contract::runtime::phase::step::StepContext;
use tirea_contract::runtime::phase::Phase;
use tirea_contract::runtime::tool_call::ToolGate;

/// Block tool execution with a denial reason.
pub struct DenyTool(pub String);

impl Action for DenyTool {
    fn label(&self) -> &'static str {
        "block_tool"
    }

    fn validate(&self, phase: Phase) -> Result<(), String> {
        if phase == Phase::BeforeToolExecute {
            Ok(())
        } else {
            Err(format!(
                "DenyTool is only allowed in BeforeToolExecute, got {phase}"
            ))
        }
    }

    fn apply(self: Box<Self>, step: &mut StepContext<'_>) {
        if let Some(gate) = step.extensions.get_mut::<ToolGate>() {
            gate.blocked = true;
            gate.block_reason = Some(self.0);
            gate.pending = false;
            gate.suspend_ticket = None;
        }
    }
}

/// Suspend tool execution pending user permission confirmation.
pub struct RequestPermission(pub tirea_contract::runtime::phase::SuspendTicket);

impl Action for RequestPermission {
    fn label(&self) -> &'static str {
        "suspend_tool"
    }

    fn validate(&self, phase: Phase) -> Result<(), String> {
        if phase == Phase::BeforeToolExecute {
            Ok(())
        } else {
            Err(format!(
                "RequestPermission is only allowed in BeforeToolExecute, got {phase}"
            ))
        }
    }

    fn apply(self: Box<Self>, step: &mut StepContext<'_>) {
        if let Some(gate) = step.extensions.get_mut::<ToolGate>() {
            gate.blocked = false;
            gate.block_reason = None;
            gate.pending = true;
            gate.suspend_ticket = Some(self.0);
        }
    }
}

/// Apply tool policy: keep only allowed tools, remove excluded ones.
pub struct ApplyToolPolicy {
    pub allowed: Option<Vec<String>>,
    pub excluded: Option<Vec<String>>,
}

impl Action for ApplyToolPolicy {
    fn label(&self) -> &'static str {
        "apply_tool_policy"
    }

    fn validate(&self, phase: Phase) -> Result<(), String> {
        if phase == Phase::BeforeInference {
            Ok(())
        } else {
            Err(format!(
                "ApplyToolPolicy is only allowed in BeforeInference, got {phase}"
            ))
        }
    }

    fn apply(self: Box<Self>, step: &mut StepContext<'_>) {
        let inf = step.extensions.get_or_default::<InferenceContext>();
        if let Some(allowed) = &self.allowed {
            inf.tools.retain(|t| allowed.iter().any(|id| id == &t.id));
        }
        if let Some(excluded) = &self.excluded {
            for id in excluded {
                inf.tools.retain(|t| t.id != *id);
            }
        }
    }
}

/// Block tool execution due to policy violation.
pub struct RejectPolicyViolation(pub String);

impl Action for RejectPolicyViolation {
    fn label(&self) -> &'static str {
        "block_tool"
    }

    fn validate(&self, phase: Phase) -> Result<(), String> {
        if phase == Phase::BeforeToolExecute {
            Ok(())
        } else {
            Err(format!(
                "RejectPolicyViolation is only allowed in BeforeToolExecute, got {phase}"
            ))
        }
    }

    fn apply(self: Box<Self>, step: &mut StepContext<'_>) {
        if let Some(gate) = step.extensions.get_mut::<ToolGate>() {
            gate.blocked = true;
            gate.block_reason = Some(self.0);
            gate.pending = false;
            gate.suspend_ticket = None;
        }
    }
}

/// Helper to create a `DenyTool` action for an explicit `Deny` behavior.
pub(super) fn deny_action(tool_id: &str) -> Box<dyn Action> {
    Box::new(DenyTool(format!("Tool '{}' is denied", tool_id)))
}

/// Helper to create a `DenyTool` action when permission check prerequisites fail.
pub(super) fn deny_missing_call_id() -> Box<dyn Action> {
    Box::new(DenyTool(
        "Permission check requires non-empty tool call id".to_string(),
    ))
}

/// Helper to create a `RejectPolicyViolation` action for out-of-scope tools.
pub(super) fn reject_out_of_scope(tool_id: &str) -> Box<dyn Action> {
    Box::new(RejectPolicyViolation(format!(
        "Tool '{}' is not allowed by current policy",
        tool_id
    )))
}

/// Helper to check whether a behavior is [`ToolPermissionBehavior::Deny`].
pub(super) fn is_deny(behavior: ToolPermissionBehavior) -> bool {
    matches!(behavior, ToolPermissionBehavior::Deny)
}
