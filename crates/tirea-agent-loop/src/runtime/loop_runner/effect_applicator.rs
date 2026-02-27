use super::AgentLoopError;
use crate::contracts::runtime::plugin::phase::effect::{validate_effect, PhaseEffect, PhaseOutput};
use crate::contracts::runtime::plugin::phase::{
    reduce_state_actions, Phase, RunAction, StepContext,
};
use tirea_state::DocCell;

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

    let tracked_actions = reduce_state_actions(output.state_actions, &doc.snapshot(), "agent")
        .map_err(|e| AgentLoopError::StateError(e.to_string()))?;
    for tracked in tracked_actions {
        step.emit_patch(tracked);
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
    use crate::contracts::runtime::plugin::phase::state_spec::{AnyStateAction, StateSpec};
    use crate::contracts::runtime::plugin::phase::ToolContext;
    use crate::contracts::runtime::run::TerminationReason;
    use crate::contracts::runtime::tool_call::Suspension;
    use crate::contracts::testing::{mock_tools_with, test_suspend_ticket, TestFixture};
    use crate::contracts::thread::ToolCall;
    use serde::{Deserialize, Serialize};
    use serde_json::json;
    use serde_json::Value;
    use tirea_state::{
        apply_patches, path, DocCell, Op, Patch, PatchSink, Path as TPath, State, TireaResult,
        TrackedPatch,
    };

    #[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
    struct CounterState {
        value: i64,
    }

    struct CounterRef;

    impl State for CounterState {
        type Ref<'a> = CounterRef;
        const PATH: &'static str = "counter";

        fn state_ref<'a>(_: &'a DocCell, _: TPath, _: PatchSink<'a>) -> Self::Ref<'a> {
            CounterRef
        }

        fn from_value(value: &Value) -> TireaResult<Self> {
            if value.is_null() {
                return Ok(Self::default());
            }
            serde_json::from_value(value.clone()).map_err(tirea_state::TireaError::Serialization)
        }

        fn to_value(&self) -> TireaResult<Value> {
            serde_json::to_value(self).map_err(tirea_state::TireaError::Serialization)
        }
    }

    enum CounterAction {
        Increment(i64),
    }

    impl StateSpec for CounterState {
        type Action = CounterAction;

        fn reduce(&mut self, action: Self::Action) {
            match action {
                CounterAction::Increment(delta) => self.value += delta,
            }
        }
    }

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

        let ticket =
            test_suspend_ticket(Suspension::new("confirm", "confirm").with_message("Execute?"));
        let output = PhaseOutput::new().suspend_tool(ticket);
        apply_phase_output(Phase::BeforeToolExecute, &mut step, output, &doc).unwrap();

        assert!(step.tool_pending());
    }

    #[test]
    fn apply_request_termination() {
        let fix = TestFixture::new();
        let mut step = fix.step(vec![]);
        let doc = DocCell::new(json!({}));

        let output = PhaseOutput::new().terminate_behavior_requested();
        apply_phase_output(Phase::BeforeInference, &mut step, output, &doc).unwrap();

        assert!(matches!(
            step.run_action(),
            RunAction::Terminate(TerminationReason::BehaviorRequested)
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

    #[test]
    fn apply_state_actions_use_rolling_snapshot() {
        let fix = TestFixture::new();
        let mut step = fix.step(vec![]);
        let initial = json!({"counter": {"value": 1}});
        let doc = DocCell::new(initial.clone());

        let output = PhaseOutput::new()
            .with_state_action(AnyStateAction::new::<CounterState>(
                CounterAction::Increment(1),
            ))
            .with_state_action(AnyStateAction::new::<CounterState>(
                CounterAction::Increment(1),
            ));

        apply_phase_output(Phase::BeforeInference, &mut step, output, &doc).unwrap();

        let patches: Vec<&Patch> = step
            .pending_patches
            .iter()
            .map(|patch| patch.patch())
            .collect();
        assert_eq!(patches.len(), 2);

        let final_state = apply_patches(&initial, patches).unwrap();
        assert_eq!(final_state["counter"]["value"], 3);
    }

    #[test]
    fn apply_raw_patch_preserves_source_metadata() {
        let fix = TestFixture::new();
        let mut step = fix.step(vec![]);
        let doc = DocCell::new(json!({}));

        let tracked = TrackedPatch::new(Patch::new().with_op(Op::set(path!("flag"), json!(true))))
            .with_source("plugin:test");
        let output = PhaseOutput::new().with_state_action(AnyStateAction::Patch(tracked));

        apply_phase_output(Phase::BeforeInference, &mut step, output, &doc).unwrap();

        match step.pending_patches.first() {
            Some(patch) => {
                assert_eq!(patch.source.as_deref(), Some("plugin:test"));
            }
            other => panic!("expected pending patch, got: {other:?}"),
        }
    }
}
