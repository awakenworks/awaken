//! Shared test fixtures for crates that depend on `tirea-contract`.
//!
//! Gated behind the `test-support` cargo feature so production builds are
//! unaffected.  Enable via `[dev-dependencies] tirea-contract = { ..., features = ["test-support"] }`.

use crate::runtime::activity::NoOpActivityManager;
use crate::runtime::plugin::phase::effect::{validate_effect, PhaseEffect, PhaseOutput};
use crate::runtime::tool_call::suspension::Suspension;
use crate::runtime::tool_call::ToolDescriptor;
use crate::runtime::{
    PendingToolCall, Phase, RunAction, StepContext, SuspendTicket, ToolCallContext,
    ToolCallResumeMode,
};
use crate::thread::Message;
use crate::RunConfig;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tirea_state::{DocCell, Op};

pub struct TestFixture {
    pub doc: DocCell,
    pub ops: Mutex<Vec<Op>>,
    pub run_config: RunConfig,
    pub pending_messages: Mutex<Vec<Arc<Message>>>,
    pub messages: Vec<Arc<Message>>,
}

impl TestFixture {
    pub fn new() -> Self {
        Self {
            doc: DocCell::new(serde_json::json!({})),
            ops: Mutex::new(Vec::new()),
            run_config: RunConfig::default(),
            pending_messages: Mutex::new(Vec::new()),
            messages: Vec::new(),
        }
    }

    pub fn new_with_state(state: Value) -> Self {
        Self {
            doc: DocCell::new(state),
            ..Self::new()
        }
    }

    pub fn ctx(&self) -> ToolCallContext<'_> {
        ToolCallContext::new(
            &self.doc,
            &self.ops,
            "test",
            "test",
            &self.run_config,
            &self.pending_messages,
            NoOpActivityManager::arc(),
        )
    }

    pub fn ctx_with(
        &self,
        call_id: impl Into<String>,
        source: impl Into<String>,
    ) -> ToolCallContext<'_> {
        ToolCallContext::new(
            &self.doc,
            &self.ops,
            call_id,
            source,
            &self.run_config,
            &self.pending_messages,
            NoOpActivityManager::arc(),
        )
    }

    pub fn step(&self, tools: Vec<ToolDescriptor>) -> StepContext<'_> {
        StepContext::new(self.ctx(), "test-thread", &self.messages, tools)
    }

    pub fn has_changes(&self) -> bool {
        !self.ops.lock().unwrap().is_empty()
    }

    pub fn updated_state(&self) -> Value {
        self.doc.snapshot()
    }
}

impl Default for TestFixture {
    fn default() -> Self {
        Self::new()
    }
}

/// Standard mock tool set with configurable third tool.
pub fn mock_tools() -> Vec<ToolDescriptor> {
    mock_tools_with("delete_file", "Delete File", "Delete a file")
}

/// Mock tool set with a custom third tool.
pub fn mock_tools_with(id: &str, display: &str, desc: &str) -> Vec<ToolDescriptor> {
    vec![
        ToolDescriptor::new("read_file", "Read File", "Read a file"),
        ToolDescriptor::new("write_file", "Write File", "Write a file"),
        ToolDescriptor::new(id, display, desc),
    ]
}

/// Build a `SuspendTicket` from a `Suspension` interaction for tests.
pub fn test_suspend_ticket(interaction: Suspension) -> SuspendTicket {
    let tool_name = interaction
        .action
        .strip_prefix("tool:")
        .unwrap_or("TestSuspend")
        .to_string();
    SuspendTicket::new(
        interaction.clone(),
        PendingToolCall::new(interaction.id, tool_name, interaction.parameters),
        ToolCallResumeMode::PassDecisionToTool,
    )
}

/// Apply a phase output in tests using the same reducer path as runtime.
///
/// This helper validates effects and applies them to `StepContext`.
pub fn apply_phase_output_for_test(
    phase: Phase,
    step: &mut StepContext<'_>,
    output: PhaseOutput,
) -> Result<(), String> {
    for effect in &output.effects {
        validate_effect(phase, effect)?;
    }
    for effect in output.effects {
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
            PhaseEffect::AppendUserMessage(text) => step.user_message(text),
            PhaseEffect::RequestTermination(reason) => {
                step.set_run_action(RunAction::Terminate(reason));
            }
        }
    }

    Ok(())
}
