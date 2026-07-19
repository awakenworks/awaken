//! Neutral value types produced/consumed by [`SharedHost`]: pending tools,
//! run results, resume commands, host errors, and outcome reports.

use super::*;

/// A tool a run awaits: its id, model-visible name/input, and whether it is
/// client-executed (the caller runs it and returns a result) or a built-in tool
/// awaiting a permission decision.
#[derive(Debug, Clone)]
pub struct PendingTool {
    pub tool_use_id: String,
    pub name: String,
    pub input: serde_json::Value,
    pub client_executed: bool,
}

/// The neutral result of one step (a turn or a resume): the messages committed
/// during the step, the resulting state, and the pending tool when the run awaits.
pub struct RunResult {
    pub new_messages: Vec<Message>,
    pub state: RunState,
    pub pending: Option<PendingTool>,
    /// `true` when this turn folded its context (the compact plugin summarized
    /// older turns). Read from durable thread state at the terminal step, so a
    /// awaiting→resumed turn reports it exactly once.
    pub compacted: bool,
    /// `true` when the runtime transparently retried a transient inference failure
    /// during this turn (auto-recovery), read from the run's reschedule counter.
    pub rescheduled: bool,
    pub delegated_runs: Vec<awaken_protocol_managed::DelegatedRun>,
}

/// The neutral resume command: answer a built-in tool's permission gate, or
/// deliver a client-executed tool's result.
pub enum HostResume {
    /// Built-in tool awaiting approval (Managed `user.tool_confirmation`; AI SDK
    /// `approval-responded` / `output-denied`).
    ToolPermission { allow: bool, note: Option<String> },
    /// Client-executed tool result (Managed `user.custom_tool_result`; AI SDK
    /// `output-available` / `output-error` on a client tool part).
    ClientResult { content: String, is_error: bool },
}

impl HostResume {
    pub(crate) fn wants_client(&self) -> bool {
        matches!(self, HostResume::ClientResult { .. })
    }
}

/// A host failure classified by fault: `BadRequest` is the caller's (bad id,
/// wrong binding, no await), `Internal` is the runtime's. Each adapter maps this
/// to its own public error shape.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct HostError {
    pub message: String,
    pub kind: HostErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostErrorKind {
    Internal,
    BadRequest,
}

impl HostError {
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: HostErrorKind::Internal,
        }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: HostErrorKind::BadRequest,
        }
    }
}

/// One evaluation round of a goal (neutral): the revision messages committed this
/// round, the round index, the classification token, and the grader explanation.
pub struct HostOutcomeIteration {
    pub messages: Vec<Message>,
    pub outcome_id: String,
    pub iteration: u32,
    pub result: String,
    pub explanation: String,
}

/// The neutral outcome report: the ordered evaluation rounds.
pub struct HostOutcomeReport {
    pub iterations: Vec<HostOutcomeIteration>,
}
