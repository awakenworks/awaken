use crate::runtime::tool_call::{PendingToolCall, Suspension, ToolCallResumeMode, ToolResult};
use crate::thread::ToolCall;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

    /// Create from a `ToolCall`.
    pub fn from_tool_call(call: &ToolCall) -> Self {
        Self::new(&call.id, &call.name, call.arguments.clone())
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

/// Suspension payload for `ToolCallAction::Suspend`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuspendTicket {
    /// External suspension payload.
    pub suspension: Suspension,
    /// Pending call projection emitted to event stream.
    pub pending: PendingToolCall,
    /// Resume mapping strategy.
    pub resume_mode: ToolCallResumeMode,
}

impl SuspendTicket {
    pub fn new(
        suspension: Suspension,
        pending: PendingToolCall,
        resume_mode: ToolCallResumeMode,
    ) -> Self {
        Self {
            suspension,
            pending,
            resume_mode,
        }
    }

    pub fn use_decision_as_tool_result(suspension: Suspension, pending: PendingToolCall) -> Self {
        Self::new(
            suspension,
            pending,
            ToolCallResumeMode::UseDecisionAsToolResult,
        )
    }

    pub fn with_resume_mode(mut self, resume_mode: ToolCallResumeMode) -> Self {
        self.resume_mode = resume_mode;
        self
    }

    pub fn with_pending(mut self, pending: PendingToolCall) -> Self {
        self.pending = pending;
        self
    }
}

/// Tool-call level control action emitted by plugins.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolCallAction {
    Proceed,
    Suspend(Box<SuspendTicket>),
    Block { reason: String },
}

impl ToolCallAction {
    pub fn suspend(ticket: SuspendTicket) -> Self {
        Self::Suspend(Box::new(ticket))
    }
}
