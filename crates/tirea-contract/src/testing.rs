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
use crate::storage::{MailboxEntry, MailboxEntryOrigin, MailboxEntryStatus};
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

/// Test-only builder for mailbox entries with deterministic defaults.
#[derive(Debug, Clone)]
pub struct MailboxEntryBuilder {
    entry: MailboxEntry,
}

impl MailboxEntryBuilder {
    /// Start a queued mailbox entry with minimal boilerplate.
    pub fn queued(entry_id: impl Into<String>, mailbox_id: impl Into<String>) -> Self {
        Self {
            entry: MailboxEntry {
                entry_id: entry_id.into(),
                mailbox_id: mailbox_id.into(),
                origin: MailboxEntryOrigin::External,
                sender_id: None,
                payload: Value::Null,
                priority: 0,
                dedupe_key: None,
                generation: 0,
                status: MailboxEntryStatus::Queued,
                available_at: 1,
                attempt_count: 0,
                last_error: None,
                claim_token: None,
                claimed_by: None,
                lease_until: None,
                created_at: 1,
                updated_at: 1,
            },
        }
    }

    /// Replace the payload.
    pub fn with_payload(mut self, payload: Value) -> Self {
        self.entry.payload = payload;
        self
    }

    /// Replace the sender identifier.
    pub fn with_sender_id(mut self, sender_id: impl Into<String>) -> Self {
        self.entry.sender_id = Some(sender_id.into());
        self
    }

    /// Replace the coarse ingress origin classification.
    pub fn with_origin(mut self, origin: MailboxEntryOrigin) -> Self {
        self.entry.origin = origin;
        self
    }

    /// Replace the priority.
    pub fn with_priority(mut self, priority: u8) -> Self {
        self.entry.priority = priority;
        self
    }

    /// Replace the dedupe key.
    pub fn with_dedupe_key(mut self, dedupe_key: impl Into<String>) -> Self {
        self.entry.dedupe_key = Some(dedupe_key.into());
        self
    }

    /// Replace the generation.
    pub fn with_generation(mut self, generation: u64) -> Self {
        self.entry.generation = generation;
        self
    }

    /// Replace the status and clear claim metadata when no longer claimed.
    pub fn with_status(mut self, status: MailboxEntryStatus) -> Self {
        self.entry.status = status;
        if status != MailboxEntryStatus::Claimed {
            self.entry.claim_token = None;
            self.entry.claimed_by = None;
            self.entry.lease_until = None;
        }
        self
    }

    /// Mark the entry as claimed.
    pub fn claimed(
        mut self,
        claim_token: impl Into<String>,
        claimed_by: impl Into<String>,
        lease_until: u64,
    ) -> Self {
        self.entry.status = MailboxEntryStatus::Claimed;
        self.entry.claim_token = Some(claim_token.into());
        self.entry.claimed_by = Some(claimed_by.into());
        self.entry.lease_until = Some(lease_until);
        self
    }

    /// Replace the available-at timestamp.
    pub fn with_available_at(mut self, available_at: u64) -> Self {
        self.entry.available_at = available_at;
        self
    }

    /// Replace the attempt count.
    pub fn with_attempt_count(mut self, attempt_count: u32) -> Self {
        self.entry.attempt_count = attempt_count;
        self
    }

    /// Replace the last error message.
    pub fn with_last_error(mut self, last_error: impl Into<String>) -> Self {
        self.entry.last_error = Some(last_error.into());
        self
    }

    /// Replace the created-at timestamp.
    pub fn with_created_at(mut self, created_at: u64) -> Self {
        self.entry.created_at = created_at;
        self
    }

    /// Replace the updated-at timestamp.
    pub fn with_updated_at(mut self, updated_at: u64) -> Self {
        self.entry.updated_at = updated_at;
        self
    }

    /// Replace both timestamps.
    pub fn with_timestamps(mut self, created_at: u64, updated_at: u64) -> Self {
        self.entry.created_at = created_at;
        self.entry.updated_at = updated_at;
        self
    }

    /// Finish building the mailbox entry.
    pub fn build(self) -> MailboxEntry {
        self.entry
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
            BeforeInferenceAction::OverrideModel(ovr) => {
                step.inference.model_override = Some(ovr);
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

#[cfg(test)]
mod tests {
    use super::MailboxEntryBuilder;
    use crate::storage::MailboxEntryStatus;
    use serde_json::json;

    #[test]
    fn mailbox_entry_builder_uses_deterministic_queued_defaults() {
        let entry = MailboxEntryBuilder::queued("entry-1", "mailbox-1").build();

        assert_eq!(entry.entry_id, "entry-1");
        assert_eq!(entry.mailbox_id, "mailbox-1");
        assert_eq!(entry.status, MailboxEntryStatus::Queued);
        assert_eq!(entry.available_at, 1);
        assert_eq!(entry.created_at, 1);
        assert_eq!(entry.updated_at, 1);
        assert!(entry.claim_token.is_none());
    }

    #[test]
    fn mailbox_entry_builder_supports_claimed_entries_and_field_overrides() {
        let entry = MailboxEntryBuilder::queued("entry-2", "mailbox-2")
            .with_payload(json!({"ok": true}))
            .with_priority(3)
            .with_generation(7)
            .with_attempt_count(2)
            .claimed("token-1", "worker-a", 99)
            .with_timestamps(10, 20)
            .build();

        assert_eq!(entry.payload, json!({"ok": true}));
        assert_eq!(entry.priority, 3);
        assert_eq!(entry.generation, 7);
        assert_eq!(entry.attempt_count, 2);
        assert_eq!(entry.status, MailboxEntryStatus::Claimed);
        assert_eq!(entry.claim_token.as_deref(), Some("token-1"));
        assert_eq!(entry.claimed_by.as_deref(), Some("worker-a"));
        assert_eq!(entry.lease_until, Some(99));
        assert_eq!(entry.created_at, 10);
        assert_eq!(entry.updated_at, 20);
    }

    #[test]
    fn mailbox_entry_builder_clears_claim_metadata_when_status_changes() {
        let entry = MailboxEntryBuilder::queued("entry-3", "mailbox-3")
            .claimed("token-1", "worker-a", 99)
            .with_status(MailboxEntryStatus::DeadLetter)
            .build();

        assert_eq!(entry.status, MailboxEntryStatus::DeadLetter);
        assert!(entry.claim_token.is_none());
        assert!(entry.claimed_by.is_none());
        assert!(entry.lease_until.is_none());
    }
}
