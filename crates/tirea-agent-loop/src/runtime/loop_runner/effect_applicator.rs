use super::AgentLoopError;
use crate::contracts::runtime::plugin::phase::effect::{validate_effect, PhaseEffect, PhaseOutput};
use crate::contracts::runtime::plugin::phase::{Phase, RunAction, StateEffect, StepContext};
use tirea_state::{DocCell, TrackedPatch};

/// Apply a [`PhaseOutput`] to the mutable [`StepContext`].
///
/// Each [`PhaseEffect`] is validated against the current phase before being
/// applied. State actions produce patches that are emitted on the step context.
pub fn apply_phase_output(
    phase: Phase,
    step: &mut StepContext<'_>,
    output: PhaseOutput,
    doc: &DocCell,
) -> Result<(), AgentLoopError> {
    // Validate all effects before applying any.
    for effect in &output.effects {
        validate_effect(phase, effect).map_err(AgentLoopError::StateError)?;
    }

    for effect in output.effects {
        apply_effect(step, effect);
    }

    // Apply state actions → produce patches.
    for action in output.state_actions {
        let snapshot = doc.snapshot();
        let patch = action
            .apply(&snapshot)
            .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
        if !patch.is_empty() {
            let tracked = TrackedPatch::new(patch).with_source("agent");
            step.emit_state_effect(StateEffect::Patch(tracked));
        }
    }

    Ok(())
}

fn apply_effect(step: &mut StepContext<'_>, effect: PhaseEffect) {
    match effect {
        PhaseEffect::SystemContext(s) => step.system(s),
        PhaseEffect::SessionContext(s) => step.thread(s),
        PhaseEffect::SystemReminder(s) => step.reminder(s),
        PhaseEffect::ExcludeTool(id) => step.exclude(&id),
        PhaseEffect::IncludeOnlyTools(ids) => {
            let refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
            step.include_only(&refs);
        }
        PhaseEffect::BlockTool(reason) => step.block(reason),
        PhaseEffect::AllowTool => step.allow(),
        PhaseEffect::SuspendTool(ticket) => step.suspend(ticket),
        PhaseEffect::OverrideToolResult(result) => step.set_tool_result(result),
        PhaseEffect::RequestTermination(reason) => {
            step.set_run_action(RunAction::Terminate(reason));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::runtime::plugin::phase::effect::PhaseEffect;
    use crate::contracts::runtime::plugin::phase::ToolContext;
    use crate::contracts::runtime::run::TerminationReason;
    use crate::contracts::runtime::tool_call::Suspension;
    use crate::contracts::testing::{mock_tools_with, test_suspend_ticket, TestFixture};
    use crate::contracts::thread::ToolCall;
    use serde_json::json;
    use tirea_state::DocCell;

    #[test]
    fn apply_system_context() {
        let fix = TestFixture::new();
        let mut step = fix.step(vec![]);
        let doc = DocCell::new(json!({}));

        let output = PhaseOutput::new().system_context("hello");
        apply_phase_output(Phase::BeforeInference, &mut step, output, &doc).unwrap();

        assert_eq!(step.system_context, vec!["hello"]);
    }

    #[test]
    fn apply_session_context() {
        let fix = TestFixture::new();
        let mut step = fix.step(vec![]);
        let doc = DocCell::new(json!({}));

        let output = PhaseOutput::new().session_context("session");
        apply_phase_output(Phase::BeforeInference, &mut step, output, &doc).unwrap();

        assert_eq!(step.session_context, vec!["session"]);
    }

    #[test]
    fn apply_system_reminder() {
        let fix = TestFixture::new();
        let mut step = fix.step(vec![]);
        let doc = DocCell::new(json!({}));

        let output = PhaseOutput::new().system_reminder("reminder");
        apply_phase_output(Phase::AfterToolExecute, &mut step, output, &doc).unwrap();

        assert_eq!(step.system_reminders, vec!["reminder"]);
    }

    #[test]
    fn apply_exclude_tool() {
        let fix = TestFixture::new();
        let tools = mock_tools_with("dangerous", "Danger", "A dangerous tool");
        let mut step = fix.step(tools);
        let doc = DocCell::new(json!({}));

        assert!(step.tools.iter().any(|t| t.id == "dangerous"));

        let output = PhaseOutput::new().exclude_tool("dangerous");
        apply_phase_output(Phase::BeforeInference, &mut step, output, &doc).unwrap();

        assert!(!step.tools.iter().any(|t| t.id == "dangerous"));
    }

    #[test]
    fn apply_block_tool() {
        let fix = TestFixture::new();
        let mut step = fix.step(vec![]);
        let call = ToolCall::new("call_1", "test_tool", json!({}));
        step.tool = Some(ToolContext::new(&call));
        let doc = DocCell::new(json!({}));

        let output = PhaseOutput::new().block_tool("denied");
        apply_phase_output(Phase::BeforeToolExecute, &mut step, output, &doc).unwrap();

        assert!(step.tool_blocked());
    }

    #[test]
    fn apply_allow_tool() {
        let fix = TestFixture::new();
        let mut step = fix.step(vec![]);
        let call = ToolCall::new("call_1", "test_tool", json!({}));
        step.tool = Some(ToolContext::new(&call));
        step.block("previously blocked");
        let doc = DocCell::new(json!({}));

        let output = PhaseOutput::new().allow_tool();
        apply_phase_output(Phase::BeforeToolExecute, &mut step, output, &doc).unwrap();

        assert!(!step.tool_blocked());
    }

    #[test]
    fn apply_suspend_tool() {
        let fix = TestFixture::new();
        let mut step = fix.step(vec![]);
        let call = ToolCall::new("call_1", "test_tool", json!({}));
        step.tool = Some(ToolContext::new(&call));
        let doc = DocCell::new(json!({}));

        let ticket = test_suspend_ticket(
            Suspension::new("confirm", "confirm").with_message("Execute?"),
        );
        let output = PhaseOutput::new().suspend_tool(ticket);
        apply_phase_output(Phase::BeforeToolExecute, &mut step, output, &doc).unwrap();

        assert!(step.tool_pending());
    }

    #[test]
    fn apply_request_termination() {
        let fix = TestFixture::new();
        let mut step = fix.step(vec![]);
        let doc = DocCell::new(json!({}));

        let output = PhaseOutput::new().terminate_plugin_requested();
        apply_phase_output(Phase::BeforeInference, &mut step, output, &doc).unwrap();

        assert!(matches!(
            step.run_action(),
            RunAction::Terminate(TerminationReason::PluginRequested)
        ));
    }

    #[test]
    fn rejects_invalid_phase_effect() {
        let fix = TestFixture::new();
        let mut step = fix.step(vec![]);
        let doc = DocCell::new(json!({}));

        // SystemContext is only valid in BeforeInference
        let output = PhaseOutput::new().system_context("wrong phase");
        let result = apply_phase_output(Phase::StepStart, &mut step, output, &doc);

        assert!(result.is_err());
    }

    #[test]
    fn apply_multiple_effects() {
        let fix = TestFixture::new();
        let mut step = fix.step(vec![]);
        let doc = DocCell::new(json!({}));

        let output = PhaseOutput::new()
            .system_context("ctx1")
            .system_context("ctx2")
            .session_context("session1");
        apply_phase_output(Phase::BeforeInference, &mut step, output, &doc).unwrap();

        assert_eq!(step.system_context, vec!["ctx1", "ctx2"]);
        assert_eq!(step.session_context, vec!["session1"]);
    }

    #[test]
    fn apply_empty_output_is_noop() {
        let fix = TestFixture::new();
        let mut step = fix.step(vec![]);
        let doc = DocCell::new(json!({}));

        let output = PhaseOutput::default();
        apply_phase_output(Phase::BeforeInference, &mut step, output, &doc).unwrap();

        assert!(step.system_context.is_empty());
        assert!(step.session_context.is_empty());
    }
}
