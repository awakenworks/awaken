use super::ext::{FlowControl, InferenceContext, MessagingContext, ToolGate};
use crate::runtime::plugin::phase::action::Action;
use crate::runtime::plugin::phase::state_spec::AnyStateAction;
use crate::runtime::plugin::phase::step::StepContext;
use crate::runtime::plugin::phase::types::{Phase, RunAction, SuspendTicket};
use crate::runtime::run::TerminationReason;
use crate::runtime::tool_call::ToolResult;

// =============================================================================
// Inference-phase actions (BeforeInference only)
// =============================================================================

/// Append a line to the system prompt context.
pub struct AddSystemContext(pub String);

impl Action for AddSystemContext {
    fn label(&self) -> &'static str {
        "add_system_context"
    }

    fn validate(&self, phase: Phase) -> Result<(), String> {
        if phase == Phase::BeforeInference {
            Ok(())
        } else {
            Err(format!(
                "AddSystemContext is only allowed in BeforeInference, got {phase}"
            ))
        }
    }

    fn apply(self: Box<Self>, step: &mut StepContext<'_>) {
        step.extensions
            .get_or_default::<InferenceContext>()
            .system_context
            .push(self.0);
    }
}

/// Append a session context message (before user messages).
pub struct AddSessionContext(pub String);

impl Action for AddSessionContext {
    fn label(&self) -> &'static str {
        "add_session_context"
    }

    fn validate(&self, phase: Phase) -> Result<(), String> {
        if phase == Phase::BeforeInference {
            Ok(())
        } else {
            Err(format!(
                "AddSessionContext is only allowed in BeforeInference, got {phase}"
            ))
        }
    }

    fn apply(self: Box<Self>, step: &mut StepContext<'_>) {
        step.extensions
            .get_or_default::<InferenceContext>()
            .session_context
            .push(self.0);
    }
}

/// Exclude a tool by ID from the available set.
pub struct ExcludeTool(pub String);

impl Action for ExcludeTool {
    fn label(&self) -> &'static str {
        "exclude_tool"
    }

    fn validate(&self, phase: Phase) -> Result<(), String> {
        if phase == Phase::BeforeInference {
            Ok(())
        } else {
            Err(format!(
                "ExcludeTool is only allowed in BeforeInference, got {phase}"
            ))
        }
    }

    fn apply(self: Box<Self>, step: &mut StepContext<'_>) {
        let inf = step.extensions.get_or_default::<InferenceContext>();
        inf.tools.retain(|t| t.id != self.0);
    }
}

/// Keep only the specified tools in the available set.
pub struct IncludeOnlyTools(pub Vec<String>);

impl Action for IncludeOnlyTools {
    fn label(&self) -> &'static str {
        "include_only_tools"
    }

    fn validate(&self, phase: Phase) -> Result<(), String> {
        if phase == Phase::BeforeInference {
            Ok(())
        } else {
            Err(format!(
                "IncludeOnlyTools is only allowed in BeforeInference, got {phase}"
            ))
        }
    }

    fn apply(self: Box<Self>, step: &mut StepContext<'_>) {
        let inf = step.extensions.get_or_default::<InferenceContext>();
        inf.tools.retain(|t| self.0.iter().any(|id| id == &t.id));
    }
}

// =============================================================================
// Tool-gate actions (BeforeToolExecute only)
// =============================================================================

/// Block tool execution with a reason.
pub struct BlockTool(pub String);

impl Action for BlockTool {
    fn label(&self) -> &'static str {
        "block_tool"
    }

    fn validate(&self, phase: Phase) -> Result<(), String> {
        if phase == Phase::BeforeToolExecute {
            Ok(())
        } else {
            Err(format!(
                "BlockTool is only allowed in BeforeToolExecute, got {phase}"
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

/// Explicitly allow tool execution, clearing any prior block/suspend.
pub struct AllowTool;

impl Action for AllowTool {
    fn label(&self) -> &'static str {
        "allow_tool"
    }

    fn validate(&self, phase: Phase) -> Result<(), String> {
        if phase == Phase::BeforeToolExecute {
            Ok(())
        } else {
            Err(format!(
                "AllowTool is only allowed in BeforeToolExecute, got {phase}"
            ))
        }
    }

    fn apply(self: Box<Self>, step: &mut StepContext<'_>) {
        if let Some(gate) = step.extensions.get_mut::<ToolGate>() {
            gate.blocked = false;
            gate.block_reason = None;
            gate.pending = false;
            gate.suspend_ticket = None;
        }
    }
}

/// Suspend tool execution with a ticket for external resolution.
pub struct SuspendTool(pub SuspendTicket);

impl Action for SuspendTool {
    fn label(&self) -> &'static str {
        "suspend_tool"
    }

    fn validate(&self, phase: Phase) -> Result<(), String> {
        if phase == Phase::BeforeToolExecute {
            Ok(())
        } else {
            Err(format!(
                "SuspendTool is only allowed in BeforeToolExecute, got {phase}"
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

/// Override the tool result directly without executing the tool.
pub struct OverrideToolResult(pub ToolResult);

impl Action for OverrideToolResult {
    fn label(&self) -> &'static str {
        "override_tool_result"
    }

    fn validate(&self, phase: Phase) -> Result<(), String> {
        if phase == Phase::BeforeToolExecute {
            Ok(())
        } else {
            Err(format!(
                "OverrideToolResult is only allowed in BeforeToolExecute, got {phase}"
            ))
        }
    }

    fn apply(self: Box<Self>, step: &mut StepContext<'_>) {
        if let Some(gate) = step.extensions.get_mut::<ToolGate>() {
            gate.result = Some(self.0);
        }
    }
}

// =============================================================================
// Post-tool actions (AfterToolExecute only)
// =============================================================================

/// Append a system reminder after tool results.
pub struct AddSystemReminder(pub String);

impl Action for AddSystemReminder {
    fn label(&self) -> &'static str {
        "add_system_reminder"
    }

    fn validate(&self, phase: Phase) -> Result<(), String> {
        if phase == Phase::AfterToolExecute {
            Ok(())
        } else {
            Err(format!(
                "AddSystemReminder is only allowed in AfterToolExecute, got {phase}"
            ))
        }
    }

    fn apply(self: Box<Self>, step: &mut StepContext<'_>) {
        step.extensions
            .get_or_default::<MessagingContext>()
            .reminders
            .push(self.0);
    }
}

/// Append a user message to be injected after tool execution.
pub struct AddUserMessage(pub String);

impl Action for AddUserMessage {
    fn label(&self) -> &'static str {
        "add_user_message"
    }

    fn validate(&self, phase: Phase) -> Result<(), String> {
        if phase == Phase::AfterToolExecute {
            Ok(())
        } else {
            Err(format!(
                "AddUserMessage is only allowed in AfterToolExecute, got {phase}"
            ))
        }
    }

    fn apply(self: Box<Self>, step: &mut StepContext<'_>) {
        step.extensions
            .get_or_default::<MessagingContext>()
            .user_messages
            .push(self.0);
    }
}

// =============================================================================
// Lifecycle actions (BeforeInference + AfterInference)
// =============================================================================

/// Request run termination with a specific reason.
pub struct RequestTermination(pub TerminationReason);

impl Action for RequestTermination {
    fn label(&self) -> &'static str {
        "request_termination"
    }

    fn validate(&self, phase: Phase) -> Result<(), String> {
        if phase == Phase::BeforeInference || phase == Phase::AfterInference {
            Ok(())
        } else {
            Err(format!(
                "RequestTermination is only allowed in BeforeInference/AfterInference, got {phase}"
            ))
        }
    }

    fn apply(self: Box<Self>, step: &mut StepContext<'_>) {
        step.extensions
            .get_or_default::<FlowControl>()
            .run_action = Some(RunAction::Terminate(self.0));
    }
}

// =============================================================================
// Generic state patch action (any phase)
// =============================================================================

/// Emit a state patch through the typed StateSpec reducer.
///
/// This is the universal mechanism for plugins to mutate persistent state.
/// Wraps an `AnyStateAction` which carries the type-erased reducer logic.
pub struct EmitStatePatch(pub AnyStateAction);

impl Action for EmitStatePatch {
    fn label(&self) -> &'static str {
        "emit_state_patch"
    }

    fn apply(self: Box<Self>, step: &mut StepContext<'_>) {
        step.emit_state_action(self.0);
    }
}
