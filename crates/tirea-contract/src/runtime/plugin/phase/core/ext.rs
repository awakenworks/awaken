use crate::runtime::llm::StreamResult;
use crate::runtime::plugin::phase::types::{RunAction, SuspendTicket};
use crate::runtime::tool_call::{ToolDescriptor, ToolResult};
use serde_json::Value;

/// Inference-phase extension: system/session context and tool descriptors.
///
/// Populated by `AddSystemContext`, `AddSessionContext`, `ExcludeTool`,
/// `IncludeOnlyTools` actions during `BeforeInference`.
#[derive(Debug, Default, Clone)]
pub struct InferenceContext {
    /// System context lines appended to the system prompt.
    pub system_context: Vec<String>,
    /// Session context messages injected before user messages.
    pub session_context: Vec<String>,
    /// Available tool descriptors (can be filtered by actions).
    pub tools: Vec<ToolDescriptor>,
}

/// Tool-gate extension: per-tool-call execution control.
///
/// Populated by `BlockTool`, `AllowTool`, `SuspendTool`,
/// `OverrideToolResult` actions during `BeforeToolExecute`.
#[derive(Debug, Clone)]
pub struct ToolGate {
    /// Tool call ID.
    pub id: String,
    /// Tool name.
    pub name: String,
    /// Tool arguments.
    pub args: Value,
    /// Tool execution result (set after execution or by override).
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

impl ToolGate {
    /// Create a new tool gate from identifiers and arguments.
    pub fn new(id: impl Into<String>, name: impl Into<String>, args: Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            args,
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
    pub fn idempotency_key(&self) -> &str {
        &self.id
    }
}

/// Post-tool messaging extension: reminders and user messages.
///
/// Populated by `AddSystemReminder`, `AddUserMessage` actions
/// during `AfterToolExecute`.
#[derive(Debug, Default, Clone)]
pub struct MessagingContext {
    /// System reminders injected after tool results.
    pub reminders: Vec<String>,
    /// User messages to append after tool execution.
    pub user_messages: Vec<String>,
}

/// Flow control extension: run-level action.
///
/// Populated by `RequestTermination` action.
#[derive(Debug, Default, Clone)]
pub struct FlowControl {
    /// Run-level action emitted by plugins.
    pub run_action: Option<RunAction>,
}

/// LLM response extension: set after inference completes.
#[derive(Debug, Clone)]
pub struct LLMResponse {
    /// LLM inference result.
    pub result: StreamResult,
}

impl LLMResponse {
    pub fn new(result: StreamResult) -> Self {
        Self { result }
    }
}
