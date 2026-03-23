//! Tool call suspension, resume, and gate types.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// External suspension payload.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Suspension {
    /// Unique suspension ID.
    #[serde(default)]
    pub id: String,
    /// Action identifier (freeform string, meaning defined by caller).
    #[serde(default)]
    pub action: String,
    /// Human-readable message/description.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// Action-specific parameters.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub parameters: Value,
    /// Optional JSON Schema for expected response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_schema: Option<Value>,
}

/// How the agent loop should replay resolved suspend decisions.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallResumeMode {
    /// Resume by replaying the original backend tool call.
    #[default]
    ReplayToolCall,
    /// Resume by turning external decision payload into tool result directly.
    UseDecisionAsToolResult,
    /// Resume by passing external payload back into tool-call arguments.
    PassDecisionToTool,
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

/// Suspension payload for tool call suspension.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SuspendTicket {
    /// External suspension payload.
    #[serde(default)]
    pub suspension: Suspension,
    /// Pending call projection emitted to event stream.
    #[serde(default)]
    pub pending: PendingToolCall,
    /// Resume mapping strategy.
    #[serde(default)]
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

    #[must_use]
    pub fn with_resume_mode(mut self, resume_mode: ToolCallResumeMode) -> Self {
        self.resume_mode = resume_mode;
        self
    }

    #[must_use]
    pub fn with_pending(mut self, pending: PendingToolCall) -> Self {
        self.pending = pending;
        self
    }
}

/// Action to apply for a suspended tool call decision.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResumeDecisionAction {
    Resume,
    Cancel,
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

/// Tool call lifecycle status.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    #[default]
    New,
    Running,
    Suspended,
    Resuming,
    Succeeded,
    Failed,
    Cancelled,
}

impl ToolCallStatus {
    /// Whether this status is terminal.
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

/// Tool call outcome derived from result status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallOutcome {
    Suspended,
    #[default]
    Succeeded,
    Failed,
}

impl ToolCallOutcome {
    /// Derive outcome from a `ToolResult`.
    pub fn from_tool_result(result: &super::tool::ToolResult) -> Self {
        match result.status {
            super::tool::ToolStatus::Pending => Self::Suspended,
            super::tool::ToolStatus::Error => Self::Failed,
            super::tool::ToolStatus::Success => Self::Succeeded,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn suspend_ticket_serde_roundtrip() {
        let ticket = SuspendTicket::new(
            Suspension {
                id: "s1".into(),
                action: "confirm".into(),
                message: "Approve?".into(),
                parameters: json!({"tool": "delete_file"}),
                response_schema: None,
            },
            PendingToolCall::new("c1", "delete_file", json!({"path": "/tmp/x"})),
            ToolCallResumeMode::UseDecisionAsToolResult,
        );
        let json = serde_json::to_string(&ticket).unwrap();
        let parsed: SuspendTicket = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, ticket);
    }

    #[test]
    fn suspend_ticket_helpers_apply_expected_fields() {
        let suspension = Suspension {
            id: "s1".into(),
            action: "approve".into(),
            message: "Approve tool call".into(),
            parameters: json!({"tool": "delete_file"}),
            response_schema: Some(json!({"type": "object"})),
        };
        let pending = PendingToolCall::new("c1", "delete_file", json!({"path": "/tmp/x"}));

        let ticket =
            SuspendTicket::use_decision_as_tool_result(suspension.clone(), pending.clone())
                .with_resume_mode(ToolCallResumeMode::PassDecisionToTool)
                .with_pending(PendingToolCall::new(
                    "c2",
                    "move_file",
                    json!({"path": "/tmp/y"}),
                ));

        assert_eq!(ticket.suspension, suspension);
        assert_eq!(ticket.resume_mode, ToolCallResumeMode::PassDecisionToTool);
        assert_eq!(ticket.pending.id, "c2");
        assert_eq!(ticket.pending.name, "move_file");
    }

    #[test]
    fn pending_tool_call_roundtrip() {
        let pending = PendingToolCall::new("c1", "search", json!({"q": "rust"}));
        let json = serde_json::to_string(&pending).unwrap();
        let parsed: PendingToolCall = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed, pending);
    }

    #[test]
    fn tool_call_status_transitions() {
        assert!(ToolCallStatus::New.can_transition_to(ToolCallStatus::Running));
        assert!(ToolCallStatus::Running.can_transition_to(ToolCallStatus::Suspended));
        assert!(ToolCallStatus::Suspended.can_transition_to(ToolCallStatus::Resuming));
        assert!(ToolCallStatus::Resuming.can_transition_to(ToolCallStatus::Succeeded));
        assert!(!ToolCallStatus::Succeeded.can_transition_to(ToolCallStatus::Running));
        assert!(!ToolCallStatus::Failed.can_transition_to(ToolCallStatus::Running));
        assert!(!ToolCallStatus::Cancelled.can_transition_to(ToolCallStatus::Running));
    }

    #[test]
    fn tool_call_status_terminal() {
        assert!(!ToolCallStatus::New.is_terminal());
        assert!(!ToolCallStatus::Running.is_terminal());
        assert!(!ToolCallStatus::Suspended.is_terminal());
        assert!(ToolCallStatus::Succeeded.is_terminal());
        assert!(ToolCallStatus::Failed.is_terminal());
        assert!(ToolCallStatus::Cancelled.is_terminal());
    }

    #[test]
    fn tool_call_resume_serde_roundtrip() {
        let resume = ToolCallResume {
            decision_id: "d1".into(),
            action: ResumeDecisionAction::Resume,
            result: json!({"approved": true}),
            reason: Some("user approved".into()),
            updated_at: 1234567890,
        };
        let json = serde_json::to_string(&resume).unwrap();
        let parsed: ToolCallResume = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, resume);
    }

    #[test]
    fn tool_call_resume_omits_empty_optional_fields() {
        let resume = ToolCallResume {
            decision_id: "d1".into(),
            action: ResumeDecisionAction::Cancel,
            result: Value::Null,
            reason: None,
            updated_at: 0,
        };
        let json = serde_json::to_string(&resume).unwrap();

        assert!(!json.contains("reason"));
        assert!(!json.contains("result"));
    }

    #[test]
    fn tool_call_outcome_from_result() {
        use super::super::tool::ToolResult;

        let success = ToolResult::success("t", json!(null));
        assert_eq!(
            ToolCallOutcome::from_tool_result(&success),
            ToolCallOutcome::Succeeded
        );

        let error = ToolResult::error("t", "fail");
        assert_eq!(
            ToolCallOutcome::from_tool_result(&error),
            ToolCallOutcome::Failed
        );

        let pending = ToolResult::suspended("t", "waiting");
        assert_eq!(
            ToolCallOutcome::from_tool_result(&pending),
            ToolCallOutcome::Suspended
        );
    }

    #[test]
    fn resume_mode_serde_roundtrip() {
        for mode in [
            ToolCallResumeMode::ReplayToolCall,
            ToolCallResumeMode::UseDecisionAsToolResult,
            ToolCallResumeMode::PassDecisionToTool,
        ] {
            let json = serde_json::to_string(&mode).unwrap();
            let parsed: ToolCallResumeMode = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, mode);
        }
    }

    #[test]
    fn resume_decision_action_serde_roundtrip() {
        for action in [ResumeDecisionAction::Resume, ResumeDecisionAction::Cancel] {
            let json = serde_json::to_string(&action).unwrap();
            let parsed: ResumeDecisionAction = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, action);
        }
    }

    #[test]
    fn suspension_omits_empty_fields() {
        let s = Suspension::default();
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("message"));
        assert!(!json.contains("parameters"));
        assert!(!json.contains("response_schema"));
    }

    // ── Additional ToolCallStatus transition tests ──

    #[test]
    fn tool_call_status_self_transitions_always_allowed() {
        for status in [
            ToolCallStatus::New,
            ToolCallStatus::Running,
            ToolCallStatus::Suspended,
            ToolCallStatus::Resuming,
            ToolCallStatus::Succeeded,
            ToolCallStatus::Failed,
            ToolCallStatus::Cancelled,
        ] {
            assert!(
                status.can_transition_to(status),
                "{status:?} -> {status:?} should be allowed"
            );
        }
    }

    #[test]
    fn tool_call_status_new_can_transition_to_any() {
        for target in [
            ToolCallStatus::Running,
            ToolCallStatus::Suspended,
            ToolCallStatus::Resuming,
            ToolCallStatus::Succeeded,
            ToolCallStatus::Failed,
            ToolCallStatus::Cancelled,
        ] {
            assert!(
                ToolCallStatus::New.can_transition_to(target),
                "New -> {target:?} should be allowed"
            );
        }
    }

    #[test]
    fn tool_call_status_running_rejects_resuming() {
        assert!(!ToolCallStatus::Running.can_transition_to(ToolCallStatus::Resuming));
    }

    #[test]
    fn tool_call_status_suspended_rejects_running_directly() {
        assert!(!ToolCallStatus::Suspended.can_transition_to(ToolCallStatus::Running));
        assert!(!ToolCallStatus::Suspended.can_transition_to(ToolCallStatus::Succeeded));
        assert!(!ToolCallStatus::Suspended.can_transition_to(ToolCallStatus::Failed));
    }

    #[test]
    fn tool_call_status_resuming_allows_wide_transitions() {
        assert!(ToolCallStatus::Resuming.can_transition_to(ToolCallStatus::Running));
        assert!(ToolCallStatus::Resuming.can_transition_to(ToolCallStatus::Suspended));
        assert!(ToolCallStatus::Resuming.can_transition_to(ToolCallStatus::Succeeded));
        assert!(ToolCallStatus::Resuming.can_transition_to(ToolCallStatus::Failed));
        assert!(ToolCallStatus::Resuming.can_transition_to(ToolCallStatus::Cancelled));
    }

    #[test]
    fn tool_call_status_terminal_cannot_transition_to_non_self() {
        for terminal in [
            ToolCallStatus::Succeeded,
            ToolCallStatus::Failed,
            ToolCallStatus::Cancelled,
        ] {
            for target in [
                ToolCallStatus::New,
                ToolCallStatus::Running,
                ToolCallStatus::Suspended,
                ToolCallStatus::Resuming,
            ] {
                assert!(
                    !terminal.can_transition_to(target),
                    "{terminal:?} -> {target:?} should be rejected"
                );
            }
        }
    }

    #[test]
    fn tool_call_status_serde_roundtrip() {
        for status in [
            ToolCallStatus::New,
            ToolCallStatus::Running,
            ToolCallStatus::Suspended,
            ToolCallStatus::Resuming,
            ToolCallStatus::Succeeded,
            ToolCallStatus::Failed,
            ToolCallStatus::Cancelled,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let parsed: ToolCallStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, status);
        }
    }

    #[test]
    fn tool_call_outcome_serde_roundtrip() {
        for outcome in [
            ToolCallOutcome::Succeeded,
            ToolCallOutcome::Suspended,
            ToolCallOutcome::Failed,
        ] {
            let json = serde_json::to_string(&outcome).unwrap();
            let parsed: ToolCallOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, outcome);
        }
    }

    #[test]
    fn suspension_with_all_fields_serde_roundtrip() {
        let s = Suspension {
            id: "s1".into(),
            action: "confirm".into(),
            message: "Allow this?".into(),
            parameters: json!({"tool": "delete"}),
            response_schema: Some(json!({"type": "object"})),
        };
        let json = serde_json::to_string(&s).unwrap();
        let parsed: Suspension = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, s);
    }

    #[test]
    fn tool_call_resume_mode_default_is_replay() {
        assert_eq!(
            ToolCallResumeMode::default(),
            ToolCallResumeMode::ReplayToolCall
        );
    }

    #[test]
    fn tool_call_status_default_is_new() {
        assert_eq!(ToolCallStatus::default(), ToolCallStatus::New);
    }
}
