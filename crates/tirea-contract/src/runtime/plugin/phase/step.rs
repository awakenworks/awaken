use super::state_spec::CommutativeAction;
use super::{RunAction, StepOutcome, SuspendTicket, ToolCallAction};
use crate::runtime::llm::StreamResult;
use crate::runtime::tool_call::ToolCallContext;
use crate::runtime::tool_call::{ToolDescriptor, ToolResult};
use crate::thread::{Message, ToolCall};
use crate::RunConfig;
use serde_json::Value;
use std::sync::Arc;
use tirea_state::{State, TireaResult, TrackedPatch};

/// Context for the currently executing tool.
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// Tool call ID.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Tool arguments.
    pub args: Value,
    /// Tool execution result (set after execution).
    pub result: Option<ToolResult>,
    /// Whether execution is blocked.
    pub blocked: bool,
    /// Block reason.
    pub block_reason: Option<String>,
    /// Whether execution is pending user confirmation.
    pub pending: bool,
    /// Canonical suspend ticket carrying pause payload.
    pub suspend_ticket: Option<SuspendTicket>,
}

impl ToolContext {
    /// Create a new tool context from a tool call.
    pub fn new(call: &ToolCall) -> Self {
        Self {
            id: call.id.clone(),
            name: call.name.clone(),
            args: call.arguments.clone(),
            result: None,
            blocked: false,
            block_reason: None,
            pending: false,
            suspend_ticket: None,
        }
    }

    /// Check if the tool execution is blocked.
    pub fn is_blocked(&self) -> bool {
        self.blocked
    }

    /// Check if the tool execution is pending.
    pub fn is_pending(&self) -> bool {
        self.pending
    }

    /// Stable idempotency key for this tool invocation.
    ///
    /// This is the same value as `tool_call_id`.
    pub fn idempotency_key(&self) -> &str {
        &self.id
    }
}

/// Step context - mutable state passed through all phases.
///
/// This is the primary interface for plugins to interact with the agent loop.
/// It provides access to session state, message building, tool filtering,
/// and flow control.
pub struct StepContext<'a> {
    // === Execution Context ===
    /// Execution context providing state access, run config, identity.
    ctx: ToolCallContext<'a>,

    // === Identity (from persistent entity) ===
    /// Thread id (read-only).
    thread_id: &'a str,

    // === Thread Messages (read-only snapshot from persistent entity) ===
    /// Messages from the thread (for building LLM requests, finding pending calls).
    messages: &'a [Arc<Message>],

    // === Message Building ===
    /// System context to append to system prompt [Position 1].
    pub system_context: Vec<String>,
    /// Session context messages (before user messages) [Position 2].
    pub session_context: Vec<String>,
    /// System reminders (after tool results) [Position 7].
    pub system_reminders: Vec<String>,
    /// User messages to append after tool execution.
    pub user_messages: Vec<String>,

    // === Tool Control ===
    /// Available tool descriptors (can be filtered).
    pub tools: Vec<ToolDescriptor>,
    /// Current tool context (only valid during tool phases).
    pub tool: Option<ToolContext>,

    // === LLM Response ===
    /// LLM response (set after inference).
    pub response: Option<StreamResult>,

    // === Flow Control ===
    /// Unified run-level action emitted by plugins.
    pub run_action: Option<RunAction>,

    // === Pending State Changes ===
    /// Patches to apply to session state after this phase completes.
    pub pending_patches: Vec<TrackedPatch>,
    /// Commutative actions to merge/apply at a later stage.
    pub pending_commutative_actions: Vec<CommutativeAction>,
}

impl<'a> StepContext<'a> {
    /// Create a new step context.
    pub fn new(
        ctx: ToolCallContext<'a>,
        thread_id: &'a str,
        messages: &'a [Arc<Message>],
        tools: Vec<ToolDescriptor>,
    ) -> Self {
        Self {
            ctx,
            thread_id,
            messages,
            system_context: Vec::new(),
            session_context: Vec::new(),
            system_reminders: Vec::new(),
            user_messages: Vec::new(),
            tools,
            tool: None,
            response: None,
            run_action: None,
            pending_patches: Vec::new(),
            pending_commutative_actions: Vec::new(),
        }
    }

    // =========================================================================
    // Execution context access
    // =========================================================================

    /// Borrow the underlying execution context.
    pub fn ctx(&self) -> &ToolCallContext<'a> {
        &self.ctx
    }

    /// Thread id.
    pub fn thread_id(&self) -> &str {
        self.thread_id
    }

    /// Thread messages (read-only snapshot from persistent entity).
    pub fn messages(&self) -> &[Arc<Message>] {
        self.messages
    }

    /// Typed state reference at the type's canonical path.
    pub fn state_of<T: State>(&self) -> T::Ref<'_> {
        self.ctx.state_of::<T>()
    }

    /// Typed state reference at path.
    pub fn state<T: State>(&self, path: &str) -> T::Ref<'_> {
        self.ctx.state::<T>(path)
    }

    /// Borrow the run config.
    pub fn run_config(&self) -> &RunConfig {
        self.ctx.run_config()
    }

    /// Typed run config accessor.
    pub fn config_state<T: State>(&self) -> TireaResult<T::Ref<'_>> {
        self.ctx.config_state::<T>()
    }

    /// Snapshot the current document state.
    pub fn snapshot(&self) -> Value {
        self.ctx.snapshot()
    }

    /// Typed snapshot at the type's canonical path.
    pub fn snapshot_of<T: State>(&self) -> TireaResult<T> {
        self.ctx.snapshot_of::<T>()
    }

    /// Typed snapshot at an explicit path.
    pub fn snapshot_at<T: State>(&self, path: &str) -> TireaResult<T> {
        self.ctx.snapshot_at::<T>(path)
    }

    /// Reset step-specific state for a new step.
    pub fn reset(&mut self) {
        self.system_context.clear();
        self.session_context.clear();
        self.system_reminders.clear();
        self.user_messages.clear();
        self.tool = None;
        self.response = None;
        self.run_action = None;
        self.pending_patches.clear();
        self.pending_commutative_actions.clear();
    }

    // =========================================================================
    // Context Injection [Position 1, 2, 7]
    // =========================================================================

    /// Add system context (appended to system prompt) [Position 1].
    pub fn system(&mut self, content: impl Into<String>) {
        self.system_context.push(content.into());
    }

    /// Add session context message (before user messages) [Position 2].
    pub fn thread(&mut self, content: impl Into<String>) {
        self.session_context.push(content.into());
    }

    /// Add system reminder (after tool result) [Position 7].
    pub fn reminder(&mut self, content: impl Into<String>) {
        self.system_reminders.push(content.into());
    }

    /// Append a user message to be injected after tool execution.
    pub fn user_message(&mut self, content: impl Into<String>) {
        self.user_messages.push(content.into());
    }

    // =========================================================================
    // Tool Filtering
    // =========================================================================

    /// Exclude a tool by ID.
    pub fn exclude(&mut self, tool_id: &str) {
        self.tools.retain(|t| t.id != tool_id);
    }

    /// Include only specified tools.
    pub fn include_only(&mut self, tool_ids: &[&str]) {
        self.tools.retain(|t| tool_ids.contains(&t.id.as_str()));
    }

    // =========================================================================
    // Tool Control (only valid during tool phases)
    // =========================================================================

    /// Get the current tool name (e.g., `"read_file"`).
    pub fn tool_name(&self) -> Option<&str> {
        self.tool.as_ref().map(|t| t.name.as_str())
    }

    /// Get the current tool call ID (e.g., `"call_abc123"`).
    pub fn tool_call_id(&self) -> Option<&str> {
        self.tool.as_ref().map(|t| t.id.as_str())
    }

    /// Get the current tool idempotency key.
    ///
    /// This is an alias of `tool_call_id`.
    pub fn tool_idempotency_key(&self) -> Option<&str> {
        self.tool_call_id()
    }

    /// Get current tool arguments.
    pub fn tool_args(&self) -> Option<&Value> {
        self.tool.as_ref().map(|t| &t.args)
    }

    /// Get current tool result.
    pub fn tool_result(&self) -> Option<&ToolResult> {
        self.tool.as_ref().and_then(|t| t.result.as_ref())
    }

    /// Check if current tool is blocked.
    pub fn tool_blocked(&self) -> bool {
        self.tool.as_ref().map(|t| t.blocked).unwrap_or(false)
    }

    /// Check if current tool is pending.
    pub fn tool_pending(&self) -> bool {
        self.tool.as_ref().map(|t| t.pending).unwrap_or(false)
    }

    /// Mark the current tool as explicitly allowed.
    ///
    /// This clears any prior block/suspend state.
    pub fn allow(&mut self) {
        if let Some(ref mut tool) = self.tool {
            tool.blocked = false;
            tool.block_reason = None;
            tool.pending = false;
            tool.suspend_ticket = None;
        }
    }

    /// Mark the current tool as blocked with a reason.
    ///
    /// This clears any prior suspend state.
    pub fn block(&mut self, reason: impl Into<String>) {
        if let Some(ref mut tool) = self.tool {
            tool.blocked = true;
            tool.block_reason = Some(reason.into());
            tool.pending = false;
            tool.suspend_ticket = None;
        }
    }

    /// Mark the current tool as suspended with a ticket.
    ///
    /// This clears any prior block state.
    pub fn suspend(&mut self, ticket: SuspendTicket) {
        if let Some(ref mut tool) = self.tool {
            tool.blocked = false;
            tool.block_reason = None;
            tool.pending = true;
            tool.suspend_ticket = Some(ticket);
        }
    }

    /// Set tool result.
    pub fn set_tool_result(&mut self, result: ToolResult) {
        if let Some(ref mut tool) = self.tool {
            tool.result = Some(result);
        }
    }

    /// Set run-level action.
    pub fn set_run_action(&mut self, action: RunAction) {
        self.run_action = Some(action);
    }

    /// Emit a state patch side effect.
    pub fn emit_patch(&mut self, patch: TrackedPatch) {
        self.pending_patches.push(patch);
    }

    /// Emit a commutative state action side effect.
    pub fn emit_commutative_action(&mut self, action: CommutativeAction) {
        self.pending_commutative_actions.push(action);
    }

    /// Effective run-level action for current step.
    pub fn run_action(&self) -> RunAction {
        self.run_action.clone().unwrap_or(RunAction::Continue)
    }

    /// Current tool action derived from tool gate state.
    pub fn tool_action(&self) -> ToolCallAction {
        if let Some(tool) = self.tool.as_ref() {
            if tool.blocked {
                return ToolCallAction::Block {
                    reason: tool.block_reason.clone().unwrap_or_default(),
                };
            }
            if tool.pending {
                if let Some(ticket) = tool.suspend_ticket.as_ref() {
                    return ToolCallAction::suspend(ticket.clone());
                }
                return ToolCallAction::Block {
                    reason: "invalid pending tool state: missing suspend ticket".to_string(),
                };
            }
        }
        ToolCallAction::Proceed
    }

    // =========================================================================
    // Step Outcome
    // =========================================================================

    /// Get the step outcome based on current state.
    pub fn result(&self) -> StepOutcome {
        // Check if any tool is pending
        if let Some(ref tool) = self.tool {
            if tool.pending {
                if let Some(ticket) = tool.suspend_ticket.as_ref() {
                    return StepOutcome::Pending(ticket.clone());
                }
                return StepOutcome::Continue;
            }
        }

        // Check if LLM response has more tool calls or is complete
        if let Some(ref response) = self.response {
            if response.tool_calls.is_empty() && !response.text.is_empty() {
                return StepOutcome::Complete;
            }
        }

        StepOutcome::Continue
    }
}
