use crate::runtime::phase::SuspendTicket;
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SuspendedCall {
    /// Original backend call identity.
    #[serde(default)]
    pub call_id: String,
    /// Original backend tool name.
    #[serde(default)]
    pub tool_name: String,
    /// Original backend tool arguments.
    #[serde(default)]
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

    /// Convert into a type-erased state action targeting this call's scope.
    ///
    /// Equivalent to `AnyStateAction::new_for_call::<SuspendedCallState>(Set(self), call_id)`
    /// but hides the internal `SuspendedCallState` / `SuspendedCallAction` types.
    pub fn into_state_action(self) -> crate::runtime::state::AnyStateAction {
        let call_id = self.call_id.clone();
        crate::runtime::state::AnyStateAction::new_for_call::<SuspendedCallState>(
            SuspendedCallAction::Set(self),
            call_id,
        )
    }
}

/// Per-tool-call suspended state stored at `__tool_call_scope.<call_id>.suspended_call`.
///
/// When a tool call is suspended, this state holds the suspension ticket, pending
/// interaction payload, and resume strategy. It is automatically deleted when the
/// tool call reaches a terminal outcome (Succeeded/Failed/Cancelled).
#[derive(Debug, Clone, Serialize, Deserialize, State)]
#[tirea(
    path = "suspended_call",
    action = "SuspendedCallAction",
    scope = "tool_call"
)]
pub struct SuspendedCallState {
    /// The suspended call data (flattened for serialization).
    #[serde(flatten)]
    pub call: SuspendedCall,
}

impl Default for SuspendedCallState {
    fn default() -> Self {
        Self {
            call: SuspendedCall::default(),
        }
    }
}

/// Action type for `SuspendedCallState` reducer.
#[derive(Serialize, Deserialize)]
pub enum SuspendedCallAction {
    /// Set the suspended call state.
    Set(SuspendedCall),
}

impl SuspendedCallState {
    fn reduce(&mut self, action: SuspendedCallAction) {
        match action {
            SuspendedCallAction::Set(call) => {
                self.call = call;
            }
        }
    }
}

/// Action type for `ToolCallState` reducer.
#[derive(Serialize, Deserialize)]
pub enum ToolCallStateAction {
    /// Set the full tool call state (used by recovery and normal updates).
    Set(ToolCallState),
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
///
/// Stored under `__tool_call_scope.<call_id>.tool_call_state` (ToolCall-scoped).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, State)]
#[tirea(
    path = "tool_call_state",
    action = "ToolCallStateAction",
    scope = "tool_call"
)]
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

impl ToolCallState {
    /// Convert into a type-erased state action targeting this call's scope.
    ///
    /// Equivalent to `AnyStateAction::new_for_call::<ToolCallState>(Set(self), call_id)`
    /// but hides the internal `ToolCallStateAction` type.
    pub fn into_state_action(self) -> crate::runtime::state::AnyStateAction {
        let call_id = self.call_id.clone();
        crate::runtime::state::AnyStateAction::new_for_call::<ToolCallState>(
            ToolCallStateAction::Set(self),
            call_id,
        )
    }
}

impl ToolCallState {
    fn reduce(&mut self, action: ToolCallStateAction) {
        match action {
            ToolCallStateAction::Set(s) => *self = s,
        }
    }
}

/// Parse suspended tool calls from a rebuilt state snapshot.
pub fn suspended_calls_from_state(state: &Value) -> HashMap<String, SuspendedCall> {
    let Some(Value::Object(scopes)) = state.get("__tool_call_scope") else {
        return HashMap::new();
    };
    scopes
        .iter()
        .filter_map(|(call_id, scope_val)| {
            scope_val
                .get("suspended_call")
                .and_then(|v| SuspendedCallState::from_value(v).ok())
                .map(|s| (call_id.clone(), s.call))
        })
        .collect()
}

/// Parse persisted tool call runtime states from a rebuilt state snapshot.
///
/// Iterates `__tool_call_scope.*["tool_call_state"]` to enumerate all call states.
pub fn tool_call_states_from_state(state: &Value) -> HashMap<String, ToolCallState> {
    let Some(Value::Object(scopes)) = state.get("__tool_call_scope") else {
        return HashMap::new();
    };
    scopes
        .iter()
        .filter_map(|(call_id, scope_val)| {
            scope_val
                .get("tool_call_state")
                .and_then(|v| ToolCallState::from_value(v).ok())
                .map(|s| (call_id.clone(), s))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suspended_call_state_default() {
        let suspended = SuspendedCallState::default();
        assert_eq!(suspended.call.call_id, "");
        assert_eq!(suspended.call.tool_name, "");
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
        assert!(
            json.get("suspension").is_some(),
            "suspension should be at top level"
        );
        assert!(
            json.get("pending").is_some(),
            "pending should be at top level"
        );
        assert!(
            json.get("resume_mode").is_some(),
            "resume_mode should be at top level"
        );
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
