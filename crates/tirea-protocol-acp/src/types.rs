use serde::{Deserialize, Serialize};

/// ACP tool call status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    /// Tool call is in progress.
    InProgress,
    /// Tool call completed successfully.
    Completed,
    /// Tool call was denied by the user.
    Denied,
    /// Tool call failed with an error.
    Errored,
}

/// ACP stop reason for session completion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Agent finished naturally (end turn).
    EndTurn,
    /// Agent stopped due to a length/budget limit.
    MaxTokens,
    /// Run was cancelled externally.
    Cancelled,
    /// Run ended due to an error.
    Error,
    /// Run is suspended waiting for external input.
    Suspended,
}

/// Permission options presented to the user in a `session/request_permission` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOption {
    /// Allow the tool call for this invocation only.
    AllowOnce,
    /// Allow the tool (or pattern) for the remainder of the session.
    AllowAlways,
    /// Deny the tool call for this invocation only.
    RejectOnce,
    /// Deny the tool (or pattern) for the remainder of the session.
    RejectAlways,
}
