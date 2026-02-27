use crate::runtime::plugin::phase::state_spec::StateSpec;
use crate::runtime::plugin::phase::SuspendTicket;
use crate::runtime::state_paths::{SUSPENDED_TOOL_CALLS_STATE_PATH, TOOL_CALL_STATES_STATE_PATH};
use crate::thread::ToolCall;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use tirea_state::State;

/// Action to apply for a suspended tool call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResumeDecisionAction {
    Resume,
    Cancel,
}

/// A tool call that has been suspended, awaiting external resolution.
///
/// The core loop stores stable call identity, pending interaction payload,
/// and explicit resume behavior for deterministic replay.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallResumeMode {
    /// Resume by replaying the original backend tool call.
    ReplayToolCall,
    /// Resume by turning external decision payload into tool result directly.
    UseDecisionAsToolResult,
    /// Resume by passing external payload back into tool-call arguments.
    PassDecisionToTool,
}

impl Default for ToolCallResumeMode {
    fn default() -> Self {
        Self::ReplayToolCall
    }
}

/// External pending tool-call projection emitted to event streams.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PendingToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

impl PendingToolCall {
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SuspendedCall {
    /// Original backend call identity.
    pub call_id: String,
    /// Original backend tool name.
    pub tool_name: String,
    /// Original backend tool arguments.
    pub arguments: Value,
    /// Suspension ticket carrying interaction payload, pending projection, and resume strategy.
    #[serde(flatten)]
    pub ticket: SuspendTicket,
}

impl SuspendedCall {
    /// Create a suspended call from a tool call and a suspend ticket.
    pub fn new(call: &ToolCall, ticket: SuspendTicket) -> Self {
        Self {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            arguments: call.arguments.clone(),
            ticket,
        }
    }
}

/// Durable suspended tool-call map persisted at `state["__suspended_tool_calls"]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(path = "__suspended_tool_calls")]
pub struct SuspendedToolCallsState {
    /// Per-call suspended tool calls awaiting external resolution.
    #[serde(default)]
    #[tirea(default = "HashMap::new()")]
    pub calls: HashMap<String, SuspendedCall>,
}

/// Action type for `SuspendedToolCallsState` reducer.
pub enum SuspendedToolCallsAction {
    /// Insert or replace a suspended call entry.
    InsertCall { call: SuspendedCall },
}

impl StateSpec for SuspendedToolCallsState {
    type Action = SuspendedToolCallsAction;

    fn reduce(&mut self, action: SuspendedToolCallsAction) {
        match action {
            SuspendedToolCallsAction::InsertCall { call } => {
                self.calls.insert(call.call_id.clone(), call);
            }
        }
    }
}

/// Action type for `ToolCallStatesMap` reducer.
pub enum ToolCallStatesAction {
    /// Insert or replace a tool call state entry.
    InsertState { state: ToolCallState },
}

/// Tool call lifecycle status for suspend/resume capable execution.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    /// Newly observed call that has not started execution yet.
    #[default]
    New,
    /// Call is currently executing.
    Running,
    /// Call is suspended waiting for a resume decision.
    Suspended,
    /// Call is resuming with external decision input.
    Resuming,
    /// Call finished successfully.
    Succeeded,
    /// Call finished with failure.
    Failed,
    /// Call was cancelled.
    Cancelled,
}

impl ToolCallStatus {
    /// Canonical tool-call lifecycle state machine used by runtime tests.
    pub const ASCII_STATE_MACHINE: &str = r#"new ------------> running
 |                  |
 |                  v
 +------------> suspended -----> resuming
                    |               |
                    +---------------+

running/resuming ---> succeeded
running/resuming ---> failed
running/suspended/resuming ---> cancelled"#;

    /// Whether this status is terminal (no further lifecycle transition expected).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            ToolCallStatus::Succeeded | ToolCallStatus::Failed | ToolCallStatus::Cancelled
        )
    }

    /// Validate lifecycle transition from `self` to `next`.
    pub fn can_transition_to(self, next: Self) -> bool {
        if self == next {
            return true;
        }

        match self {
            ToolCallStatus::New => true,
            ToolCallStatus::Running => matches!(
                next,
                ToolCallStatus::Suspended
                    | ToolCallStatus::Succeeded
                    | ToolCallStatus::Failed
                    | ToolCallStatus::Cancelled
            ),
            ToolCallStatus::Suspended => {
                matches!(next, ToolCallStatus::Resuming | ToolCallStatus::Cancelled)
            }
            ToolCallStatus::Resuming => matches!(
                next,
                ToolCallStatus::Running
                    | ToolCallStatus::Suspended
                    | ToolCallStatus::Succeeded
                    | ToolCallStatus::Failed
                    | ToolCallStatus::Cancelled
            ),
            ToolCallStatus::Succeeded | ToolCallStatus::Failed | ToolCallStatus::Cancelled => false,
        }
    }
}

/// Resume input payload attached to a suspended tool call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallResume {
    /// Idempotency key for the decision submission.
    #[serde(default)]
    pub decision_id: String,
    /// Resume or cancel action.
    pub action: ResumeDecisionAction,
    /// Raw response payload from suspension/frontend.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub result: Value,
    /// Optional human-readable reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Decision update timestamp (unix millis).
    #[serde(default)]
    pub updated_at: u64,
}

/// Durable per-tool-call runtime state.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, State)]
pub struct ToolCallState {
    /// Stable tool call id.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub call_id: String,
    /// Tool name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tool_name: String,
    /// Tool arguments snapshot.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub arguments: Value,
    /// Lifecycle status.
    #[serde(default)]
    pub status: ToolCallStatus,
    /// Token used by external actor to resume this call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_token: Option<String>,
    /// Resume payload written by external decision handling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<ToolCallResume>,
    /// Plugin/tool scratch data for this call.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub scratch: Value,
    /// Last update timestamp (unix millis).
    #[serde(default)]
    pub updated_at: u64,
}

/// Durable per-call runtime map persisted at `state["__tool_call_states"]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(path = "__tool_call_states")]
pub struct ToolCallStatesMap {
    /// Runtime state keyed by `tool_call_id`.
    #[serde(default)]
    #[tirea(default = "HashMap::new()")]
    pub calls: HashMap<String, ToolCallState>,
}

impl StateSpec for ToolCallStatesMap {
    type Action = ToolCallStatesAction;

    fn reduce(&mut self, action: ToolCallStatesAction) {
        match action {
            ToolCallStatesAction::InsertState { state } => {
                self.calls.insert(state.call_id.clone(), state);
            }
        }
    }
}

/// Parse suspended tool calls from a rebuilt state snapshot.
pub fn suspended_calls_from_state(state: &Value) -> HashMap<String, SuspendedCall> {
    state
        .get(SUSPENDED_TOOL_CALLS_STATE_PATH)
        .and_then(|value| value.get("calls"))
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

/// Parse persisted tool call runtime states from a rebuilt state snapshot.
pub fn tool_call_states_from_state(state: &Value) -> HashMap<String, ToolCallState> {
    state
        .get(TOOL_CALL_STATES_STATE_PATH)
        .and_then(|value| value.get("calls"))
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suspended_tool_calls_state_defaults_to_empty() {
        let suspended = SuspendedToolCallsState::default();
        assert!(suspended.calls.is_empty());
    }

    #[test]
    fn tool_call_status_transitions_match_lifecycle() {
        assert!(ToolCallStatus::New.can_transition_to(ToolCallStatus::Running));
        assert!(ToolCallStatus::Running.can_transition_to(ToolCallStatus::Suspended));
        assert!(ToolCallStatus::Suspended.can_transition_to(ToolCallStatus::Resuming));
        assert!(ToolCallStatus::Resuming.can_transition_to(ToolCallStatus::Running));
        assert!(ToolCallStatus::Resuming.can_transition_to(ToolCallStatus::Failed));
        assert!(ToolCallStatus::Running.can_transition_to(ToolCallStatus::Succeeded));
        assert!(ToolCallStatus::Running.can_transition_to(ToolCallStatus::Failed));
        assert!(ToolCallStatus::Suspended.can_transition_to(ToolCallStatus::Cancelled));
    }

    #[test]
    fn tool_call_status_rejects_terminal_reopen_transitions() {
        assert!(!ToolCallStatus::Succeeded.can_transition_to(ToolCallStatus::Running));
        assert!(!ToolCallStatus::Failed.can_transition_to(ToolCallStatus::Resuming));
        assert!(!ToolCallStatus::Cancelled.can_transition_to(ToolCallStatus::Suspended));
    }

    #[test]
    fn suspended_call_serde_flatten_roundtrip() {
        use crate::runtime::tool_call::Suspension;

        let call = SuspendedCall {
            call_id: "call_1".into(),
            tool_name: "my_tool".into(),
            arguments: serde_json::json!({"key": "val"}),
            ticket: SuspendTicket::new(
                Suspension::new("susp_1", "confirm"),
                PendingToolCall::new("pending_1", "my_tool", serde_json::json!({"key": "val"})),
                ToolCallResumeMode::UseDecisionAsToolResult,
            ),
        };

        let json = serde_json::to_value(&call).unwrap();

        // Flattened fields should appear at top level, not nested under "ticket"
        assert!(json.get("ticket").is_none(), "ticket should be flattened");
        assert!(json.get("suspension").is_some(), "suspension should be at top level");
        assert!(json.get("pending").is_some(), "pending should be at top level");
        assert!(json.get("resume_mode").is_some(), "resume_mode should be at top level");
        assert_eq!(json["call_id"], "call_1");
        assert_eq!(json["suspension"]["id"], "susp_1");
        assert_eq!(json["pending"]["id"], "pending_1");

        // Roundtrip: deserialize back
        let deserialized: SuspendedCall = serde_json::from_value(json).unwrap();
        assert_eq!(deserialized, call);
    }

    #[test]
    fn tool_call_ascii_state_machine_contains_all_states() {
        let diagram = ToolCallStatus::ASCII_STATE_MACHINE;
        assert!(diagram.contains("new"));
        assert!(diagram.contains("running"));
        assert!(diagram.contains("suspended"));
        assert!(diagram.contains("resuming"));
        assert!(diagram.contains("succeeded"));
        assert!(diagram.contains("failed"));
        assert!(diagram.contains("cancelled"));
    }
}
