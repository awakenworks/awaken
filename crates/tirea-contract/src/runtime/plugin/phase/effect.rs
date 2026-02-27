use super::{Phase, SuspendTicket};
use crate::runtime::run::TerminationReason;
use crate::runtime::tool_call::ToolResult;

use super::state_spec::AnyStateAction;

/// Declarative effect emitted by an Agent phase hook.
///
/// Each variant maps to a specific mutation that the loop engine will apply
/// to the step context after the hook returns. Phase-validity is checked by
/// [`validate_effect`].
#[derive(Debug, Clone)]
pub enum PhaseEffect {
    // Context injection — BeforeInference only
    SystemContext(String),
    SessionContext(String),

    // Reminders — AfterToolExecute only
    SystemReminder(String),

    // Tool filtering — BeforeInference only
    ExcludeTool(String),
    IncludeOnlyTools(Vec<String>),

    // Tool gating — BeforeToolExecute only
    BlockTool(String),
    AllowTool,
    SuspendTool(SuspendTicket),
    OverrideToolResult(ToolResult),

    // Lifecycle — BeforeInference + AfterInference
    RequestTermination(TerminationReason),
}

/// Return type for [`Agent`](crate::runtime::plugin::agent::Agent) phase hooks.
///
/// Carries declarative effects and typed state actions that the loop engine
/// applies after each hook returns.
#[derive(Debug, Default)]
pub struct PhaseOutput {
    pub effects: Vec<PhaseEffect>,
    pub state_actions: Vec<AnyStateAction>,
}

impl PhaseOutput {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_effect(mut self, effect: PhaseEffect) -> Self {
        self.effects.push(effect);
        self
    }

    #[must_use]
    pub fn with_state_action(mut self, action: AnyStateAction) -> Self {
        self.state_actions.push(action);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.effects.is_empty() && self.state_actions.is_empty()
    }

    // -- convenience builders for effects --

    #[must_use]
    pub fn system_context(self, text: impl Into<String>) -> Self {
        self.with_effect(PhaseEffect::SystemContext(text.into()))
    }

    #[must_use]
    pub fn session_context(self, text: impl Into<String>) -> Self {
        self.with_effect(PhaseEffect::SessionContext(text.into()))
    }

    #[must_use]
    pub fn system_reminder(self, text: impl Into<String>) -> Self {
        self.with_effect(PhaseEffect::SystemReminder(text.into()))
    }

    #[must_use]
    pub fn exclude_tool(self, tool_id: impl Into<String>) -> Self {
        self.with_effect(PhaseEffect::ExcludeTool(tool_id.into()))
    }

    #[must_use]
    pub fn include_only_tools(self, tool_ids: Vec<String>) -> Self {
        self.with_effect(PhaseEffect::IncludeOnlyTools(tool_ids))
    }

    #[must_use]
    pub fn block_tool(self, reason: impl Into<String>) -> Self {
        self.with_effect(PhaseEffect::BlockTool(reason.into()))
    }

    #[must_use]
    pub fn allow_tool(self) -> Self {
        self.with_effect(PhaseEffect::AllowTool)
    }

    #[must_use]
    pub fn suspend_tool(self, ticket: SuspendTicket) -> Self {
        self.with_effect(PhaseEffect::SuspendTool(ticket))
    }

    #[must_use]
    pub fn override_tool_result(self, result: ToolResult) -> Self {
        self.with_effect(PhaseEffect::OverrideToolResult(result))
    }

    #[must_use]
    pub fn request_termination(self, reason: TerminationReason) -> Self {
        self.with_effect(PhaseEffect::RequestTermination(reason))
    }

    #[must_use]
    pub fn terminate_behavior_requested(self) -> Self {
        self.request_termination(TerminationReason::BehaviorRequested)
    }
}

/// Check whether a [`PhaseEffect`] is allowed in the given [`Phase`].
///
/// Returns `Ok(())` if the effect is valid, or `Err(reason)` describing the
/// violation. This mirrors the existing [`PhasePolicy`](super::PhasePolicy)
/// but operates on the declarative enum instead of before/after snapshot diffs.
pub fn validate_effect(phase: Phase, effect: &PhaseEffect) -> Result<(), String> {
    match effect {
        PhaseEffect::SystemContext(_) | PhaseEffect::SessionContext(_) => {
            if phase == Phase::BeforeInference {
                Ok(())
            } else {
                Err(format!(
                    "context injection effects are only allowed in BeforeInference, got {phase}"
                ))
            }
        }
        PhaseEffect::SystemReminder(_) => {
            if phase == Phase::AfterToolExecute {
                Ok(())
            } else {
                Err(format!(
                    "system reminder effects are only allowed in AfterToolExecute, got {phase}"
                ))
            }
        }
        PhaseEffect::ExcludeTool(_) | PhaseEffect::IncludeOnlyTools(_) => {
            if phase == Phase::BeforeInference {
                Ok(())
            } else {
                Err(format!(
                    "tool filter effects are only allowed in BeforeInference, got {phase}"
                ))
            }
        }
        PhaseEffect::BlockTool(_)
        | PhaseEffect::AllowTool
        | PhaseEffect::SuspendTool(_)
        | PhaseEffect::OverrideToolResult(_) => {
            if phase == Phase::BeforeToolExecute {
                Ok(())
            } else {
                Err(format!(
                    "tool gate effects are only allowed in BeforeToolExecute, got {phase}"
                ))
            }
        }
        PhaseEffect::RequestTermination(_) => {
            if phase == Phase::BeforeInference || phase == Phase::AfterInference {
                Ok(())
            } else {
                Err(format!(
                    "termination effects are only allowed in BeforeInference/AfterInference, got {phase}"
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_output_default_is_empty() {
        let output = PhaseOutput::default();
        assert!(output.is_empty());
        assert!(output.effects.is_empty());
        assert!(output.state_actions.is_empty());
    }

    #[test]
    fn phase_output_builder_accumulates() {
        let output = PhaseOutput::new()
            .system_context("ctx1")
            .session_context("ctx2")
            .exclude_tool("tool_a");

        assert_eq!(output.effects.len(), 3);
        assert!(!output.is_empty());
    }

    #[test]
    fn validate_effect_context_injection() {
        let effects = [
            PhaseEffect::SystemContext("x".into()),
            PhaseEffect::SessionContext("y".into()),
        ];
        for effect in &effects {
            assert!(validate_effect(Phase::BeforeInference, effect).is_ok());
            assert!(validate_effect(Phase::AfterInference, effect).is_err());
            assert!(validate_effect(Phase::RunStart, effect).is_err());
            assert!(validate_effect(Phase::AfterToolExecute, effect).is_err());
        }
    }

    #[test]
    fn validate_effect_reminder() {
        let effect = PhaseEffect::SystemReminder("r".into());
        assert!(validate_effect(Phase::AfterToolExecute, &effect).is_ok());
        assert!(validate_effect(Phase::BeforeInference, &effect).is_err());
        assert!(validate_effect(Phase::StepEnd, &effect).is_err());
    }

    #[test]
    fn validate_effect_tool_filter() {
        let effects = [
            PhaseEffect::ExcludeTool("t".into()),
            PhaseEffect::IncludeOnlyTools(vec!["t".into()]),
        ];
        for effect in &effects {
            assert!(validate_effect(Phase::BeforeInference, effect).is_ok());
            assert!(validate_effect(Phase::AfterInference, effect).is_err());
            assert!(validate_effect(Phase::BeforeToolExecute, effect).is_err());
        }
    }

    #[test]
    fn validate_effect_tool_gate() {
        let effects = [
            PhaseEffect::BlockTool("reason".into()),
            PhaseEffect::AllowTool,
        ];
        for effect in &effects {
            assert!(validate_effect(Phase::BeforeToolExecute, effect).is_ok());
            assert!(validate_effect(Phase::BeforeInference, effect).is_err());
            assert!(validate_effect(Phase::AfterToolExecute, effect).is_err());
        }
    }

    #[test]
    fn validate_effect_termination() {
        let effect = PhaseEffect::RequestTermination(TerminationReason::BehaviorRequested);
        assert!(validate_effect(Phase::BeforeInference, &effect).is_ok());
        assert!(validate_effect(Phase::AfterInference, &effect).is_ok());
        assert!(validate_effect(Phase::StepStart, &effect).is_err());
        assert!(validate_effect(Phase::RunEnd, &effect).is_err());
    }

    #[test]
    fn validate_effect_read_only_phases_reject_all() {
        let read_only_phases = [
            Phase::RunStart,
            Phase::StepStart,
            Phase::StepEnd,
            Phase::RunEnd,
        ];
        let effects = [
            PhaseEffect::SystemContext("x".into()),
            PhaseEffect::SessionContext("y".into()),
            PhaseEffect::SystemReminder("r".into()),
            PhaseEffect::ExcludeTool("t".into()),
            PhaseEffect::BlockTool("b".into()),
            PhaseEffect::AllowTool,
            PhaseEffect::RequestTermination(TerminationReason::NaturalEnd),
        ];
        for phase in &read_only_phases {
            for effect in &effects {
                assert!(
                    validate_effect(*phase, effect).is_err(),
                    "expected {phase} to reject {effect:?}"
                );
            }
        }
    }
}
