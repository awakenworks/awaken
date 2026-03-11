//! Shared test fixtures for crates that depend on `tirea-contract`.
//!
//! Gated behind the `test-support` cargo feature so production builds are
//! unaffected.  Enable via `[dev-dependencies] tirea-contract = { ..., features = ["test-support"] }`.

use crate::runtime::activity::NoOpActivityManager;
use crate::runtime::phase::{
    ActionSet, AfterInferenceAction, AfterToolExecuteAction, BeforeInferenceAction,
    BeforeToolExecuteAction, LifecycleAction,
};
use crate::runtime::run::RunIdentity;
use crate::runtime::run::{RunAction, TerminationReason};
use crate::runtime::tool_call::suspension::Suspension;
use crate::runtime::tool_call::CallerContext;
use crate::runtime::tool_call::ToolDescriptor;
use crate::runtime::{
    PendingToolCall, StepContext, SuspendTicket, ToolCallContext, ToolCallResumeMode,
};
use crate::thread::Message;
use crate::RunPolicy;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tirea_state::{DocCell, Op, State};

/// Minimal State fixture for tests that exercise the `state_of` / `snapshot_of` APIs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(path = "__test_fixture")]
pub struct TestFixtureState {
    #[tirea(default = "None")]
    pub label: Option<String>,
}

pub struct TestFixture {
    pub doc: DocCell,
    pub ops: Mutex<Vec<Op>>,
    pub run_policy: RunPolicy,
    pub run_identity: RunIdentity,
    pub caller_context: CallerContext,
    pub pending_messages: Mutex<Vec<Arc<Message>>>,
    pub messages: Vec<Arc<Message>>,
}

impl TestFixture {
    pub fn new() -> Self {
        Self {
            doc: DocCell::new(serde_json::json!({})),
            ops: Mutex::new(Vec::new()),
            run_policy: RunPolicy::default(),
            run_identity: RunIdentity::default(),
            caller_context: CallerContext::default(),
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
            &self.run_policy,
            &self.pending_messages,
            NoOpActivityManager::arc(),
        )
        .with_run_identity(self.run_identity.clone())
        .with_caller_context(self.caller_context.clone())
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
            &self.run_policy,
            &self.pending_messages,
            NoOpActivityManager::arc(),
        )
        .with_run_identity(self.run_identity.clone())
        .with_caller_context(self.caller_context.clone())
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

// =============================================================================
// Typed phase action helpers for new tests
// =============================================================================

pub fn typed_system_context(text: impl Into<String>) -> BeforeInferenceAction {
    BeforeInferenceAction::AddSystemContext(text.into())
}

pub fn typed_block_tool(reason: impl Into<String>) -> BeforeToolExecuteAction {
    BeforeToolExecuteAction::Block(reason.into())
}

pub fn typed_suspend_tool(ticket: SuspendTicket) -> BeforeToolExecuteAction {
    BeforeToolExecuteAction::Suspend(ticket)
}

pub fn typed_system_reminder(text: impl Into<String>) -> AfterToolExecuteAction {
    AfterToolExecuteAction::AddSystemReminder(text.into())
}

pub fn typed_user_message(text: impl Into<String>) -> AfterToolExecuteAction {
    AfterToolExecuteAction::AddUserMessage(text.into())
}

pub fn typed_terminate_before(reason: TerminationReason) -> BeforeInferenceAction {
    BeforeInferenceAction::Terminate(reason)
}

pub fn typed_terminate_after(reason: TerminationReason) -> AfterInferenceAction {
    AfterInferenceAction::Terminate(reason)
}

// =============================================================================
// Typed phase apply helpers for tests that dispatch per-phase ActionSets
// =============================================================================

pub fn apply_lifecycle_for_test(step: &mut StepContext<'_>, actions: ActionSet<LifecycleAction>) {
    for action in actions {
        match action {
            LifecycleAction::State(sa) => step.emit_state_action(sa),
        }
    }
}

pub fn apply_before_inference_for_test(
    step: &mut StepContext<'_>,
    actions: ActionSet<BeforeInferenceAction>,
) {
    for action in actions {
        match action {
            BeforeInferenceAction::AddSystemContext(text) => {
                step.inference.system_context.push(text);
            }
            BeforeInferenceAction::AddSessionContext(text) => {
                step.inference.session_context.push(text);
            }
            BeforeInferenceAction::ExcludeTool(id) => {
                step.inference.tools.retain(|t| t.id != id);
            }
            BeforeInferenceAction::IncludeOnlyTools(ids) => {
                step.inference.tools.retain(|t| ids.contains(&t.id));
            }
            BeforeInferenceAction::AddRequestTransform(transform) => {
                step.inference.request_transforms.push(transform);
            }
            BeforeInferenceAction::Terminate(reason) => {
                step.flow.run_action = Some(RunAction::Terminate(reason));
            }
            BeforeInferenceAction::State(sa) => step.emit_state_action(sa),
        }
    }
}

pub fn apply_after_inference_for_test(
    step: &mut StepContext<'_>,
    actions: ActionSet<AfterInferenceAction>,
) {
    for action in actions {
        match action {
            AfterInferenceAction::Terminate(reason) => {
                step.flow.run_action = Some(RunAction::Terminate(reason));
            }
            AfterInferenceAction::State(sa) => step.emit_state_action(sa),
        }
    }
}

pub fn apply_before_tool_for_test(
    step: &mut StepContext<'_>,
    actions: ActionSet<BeforeToolExecuteAction>,
) {
    for action in actions {
        match action {
            BeforeToolExecuteAction::Block(reason) => {
                if let Some(gate) = step.gate.as_mut() {
                    gate.blocked = true;
                    gate.block_reason = Some(reason);
                    gate.pending = false;
                    gate.suspend_ticket = None;
                }
            }
            BeforeToolExecuteAction::Suspend(ticket) => {
                if let Some(gate) = step.gate.as_mut() {
                    gate.blocked = false;
                    gate.block_reason = None;
                    gate.pending = true;
                    gate.suspend_ticket = Some(ticket);
                }
            }
            BeforeToolExecuteAction::SetToolResult(result) => {
                if let Some(gate) = step.gate.as_mut() {
                    gate.result = Some(result);
                }
            }
            BeforeToolExecuteAction::State(sa) => step.emit_state_action(sa),
        }
    }
}

pub fn apply_after_tool_for_test(
    step: &mut StepContext<'_>,
    actions: ActionSet<AfterToolExecuteAction>,
) {
    for action in actions {
        match action {
            AfterToolExecuteAction::AddSystemReminder(text) => {
                step.messaging.reminders.push(text);
            }
            AfterToolExecuteAction::AddUserMessage(text) => {
                step.messaging.user_messages.push(text);
            }
            AfterToolExecuteAction::State(sa) => step.emit_state_action(sa),
        }
    }
}
