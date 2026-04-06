use crate::state::{MergeStrategy, StateKey};
use awaken_contract::contract::suspension::{ToolCallResume, ToolCallResumeMode, ToolCallStatus};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Per-tool-call lifecycle state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolCallState {
    pub call_id: String,
    pub tool_name: String,
    pub arguments: Value,
    pub status: ToolCallStatus,
    pub updated_at: u64,
    /// Resume mode from the `SuspendTicket` (set when status becomes Suspended).
    #[serde(default)]
    pub resume_mode: ToolCallResumeMode,
    /// External-facing suspension id used by protocols that distinguish
    /// approval/interrupt ids from the underlying tool call id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspension_id: Option<String>,
    /// Suspension reason/action from the active `SuspendTicket`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspension_reason: Option<String>,
    /// Most recent external resume input applied to this suspended tool call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_input: Option<ToolCallResume>,
}

/// Keyed collection of tool call states for the current step.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolCallStateMap {
    pub calls: HashMap<String, ToolCallState>,
}

/// Update for the tool call states key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionalField<T> {
    Preserve,
    Clear,
    Set(T),
}

impl<T> Default for OptionalField<T> {
    fn default() -> Self {
        Self::Preserve
    }
}

pub enum ToolCallStatesUpdate {
    /// Upsert a tool call's lifecycle state (validates transition).
    Upsert {
        call_id: String,
        tool_name: String,
        arguments: Value,
        status: ToolCallStatus,
        updated_at: u64,
        /// Resume mode from the `SuspendTicket` (meaningful when status is Suspended).
        resume_mode: ToolCallResumeMode,
        /// External-facing suspension id, when the tool is suspended.
        suspension_id: OptionalField<String>,
        /// Suspension reason/action, when the tool is suspended.
        suspension_reason: OptionalField<String>,
        /// External resume payload applied while resuming/cancelling.
        resume_input: OptionalField<ToolCallResume>,
    },
    /// Clear all tool call states (at step boundary).
    Clear,
}

/// State key for tool call lifecycle tracking within a step.
pub struct ToolCallStates;

impl StateKey for ToolCallStates {
    const KEY: &'static str = "__runtime.tool_call_states";
    const MERGE: MergeStrategy = MergeStrategy::Commutative;

    type Value = ToolCallStateMap;
    type Update = ToolCallStatesUpdate;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        match update {
            ToolCallStatesUpdate::Upsert {
                call_id,
                tool_name,
                arguments,
                status,
                updated_at,
                resume_mode,
                suspension_id,
                suspension_reason,
                resume_input,
            } => {
                let existing = value.calls.get(&call_id);
                let current_status = existing.map(|s| s.status).unwrap_or(ToolCallStatus::New);

                if !current_status.can_transition_to(status) {
                    tracing::error!(
                        from = ?current_status,
                        to = ?status,
                        call_id = %call_id,
                        "invalid tool call transition — skipping update"
                    );
                    return;
                }

                // Preserve stored resume_mode unless this update explicitly sets Suspended
                let effective_resume_mode = if status == ToolCallStatus::Suspended {
                    resume_mode
                } else {
                    existing.map(|s| s.resume_mode).unwrap_or(resume_mode)
                };
                let effective_suspension_id = match suspension_id {
                    OptionalField::Preserve => existing.and_then(|s| s.suspension_id.clone()),
                    OptionalField::Clear => None,
                    OptionalField::Set(id) => {
                        let trimmed = id.trim();
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed.to_string())
                        }
                    }
                };
                let effective_suspension_reason = match suspension_reason {
                    OptionalField::Preserve => existing.and_then(|s| s.suspension_reason.clone()),
                    OptionalField::Clear => None,
                    OptionalField::Set(reason) => {
                        let trimmed = reason.trim();
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed.to_string())
                        }
                    }
                };
                let effective_resume_input = match resume_input {
                    OptionalField::Preserve => existing.and_then(|s| s.resume_input.clone()),
                    OptionalField::Clear => None,
                    OptionalField::Set(value) => Some(value),
                };

                value.calls.insert(
                    call_id.clone(),
                    ToolCallState {
                        call_id,
                        tool_name,
                        arguments,
                        status,
                        updated_at,
                        resume_mode: effective_resume_mode,
                        suspension_id: effective_suspension_id,
                        suspension_reason: effective_suspension_reason,
                        resume_input: effective_resume_input,
                    },
                );
            }
            ToolCallStatesUpdate::Clear => {
                value.calls.clear();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upsert(
        states: &mut ToolCallStateMap,
        call_id: &str,
        tool: &str,
        status: ToolCallStatus,
        ts: u64,
    ) {
        ToolCallStates::apply(
            states,
            ToolCallStatesUpdate::Upsert {
                call_id: call_id.into(),
                tool_name: tool.into(),
                arguments: serde_json::json!({}),
                status,
                updated_at: ts,
                resume_mode: ToolCallResumeMode::default(),
                suspension_id: OptionalField::Preserve,
                suspension_reason: OptionalField::Clear,
                resume_input: OptionalField::Clear,
            },
        );
    }

    #[test]
    fn tool_call_new_to_running() {
        let mut states = ToolCallStateMap::default();
        upsert(&mut states, "c1", "echo", ToolCallStatus::Running, 100);
        assert_eq!(states.calls["c1"].status, ToolCallStatus::Running);
    }

    #[test]
    fn tool_call_running_to_succeeded() {
        let mut states = ToolCallStateMap::default();
        upsert(&mut states, "c1", "echo", ToolCallStatus::Running, 100);
        upsert(&mut states, "c1", "echo", ToolCallStatus::Succeeded, 200);
        assert_eq!(states.calls["c1"].status, ToolCallStatus::Succeeded);
    }

    #[test]
    fn tool_call_running_to_failed() {
        let mut states = ToolCallStateMap::default();
        upsert(&mut states, "c1", "echo", ToolCallStatus::Running, 100);
        upsert(&mut states, "c1", "echo", ToolCallStatus::Failed, 200);
        assert_eq!(states.calls["c1"].status, ToolCallStatus::Failed);
    }

    #[test]
    fn tool_call_running_to_suspended_to_resuming() {
        let mut states = ToolCallStateMap::default();
        upsert(&mut states, "c1", "echo", ToolCallStatus::Running, 100);
        upsert(&mut states, "c1", "echo", ToolCallStatus::Suspended, 200);
        upsert(&mut states, "c1", "echo", ToolCallStatus::Resuming, 300);
        assert_eq!(states.calls["c1"].status, ToolCallStatus::Resuming);
    }

    #[test]
    fn tool_call_suspended_to_cancelled() {
        let mut states = ToolCallStateMap::default();
        upsert(&mut states, "c1", "echo", ToolCallStatus::Running, 100);
        upsert(&mut states, "c1", "echo", ToolCallStatus::Suspended, 200);
        upsert(&mut states, "c1", "echo", ToolCallStatus::Cancelled, 300);
        assert_eq!(states.calls["c1"].status, ToolCallStatus::Cancelled);
        assert!(states.calls["c1"].status.is_terminal());
    }

    #[test]
    fn tool_call_rejects_succeeded_to_running() {
        let mut states = ToolCallStateMap::default();
        upsert(&mut states, "c1", "echo", ToolCallStatus::Running, 100);
        upsert(&mut states, "c1", "echo", ToolCallStatus::Succeeded, 200);
        upsert(&mut states, "c1", "echo", ToolCallStatus::Running, 300);
        assert_eq!(states.calls["c1"].status, ToolCallStatus::Succeeded);
        assert_eq!(states.calls["c1"].updated_at, 200);
    }

    #[test]
    fn tool_call_rejects_failed_to_running() {
        let mut states = ToolCallStateMap::default();
        upsert(&mut states, "c1", "echo", ToolCallStatus::Running, 100);
        upsert(&mut states, "c1", "echo", ToolCallStatus::Failed, 200);
        upsert(&mut states, "c1", "echo", ToolCallStatus::Running, 300);
        assert_eq!(states.calls["c1"].status, ToolCallStatus::Failed);
        assert_eq!(states.calls["c1"].updated_at, 200);
    }

    #[test]
    fn tool_call_multiple_calls_independent() {
        let mut states = ToolCallStateMap::default();
        upsert(&mut states, "c1", "echo", ToolCallStatus::Running, 100);
        upsert(&mut states, "c2", "calc", ToolCallStatus::Running, 100);
        upsert(&mut states, "c1", "echo", ToolCallStatus::Succeeded, 200);
        upsert(&mut states, "c2", "calc", ToolCallStatus::Failed, 200);

        assert_eq!(states.calls["c1"].status, ToolCallStatus::Succeeded);
        assert_eq!(states.calls["c2"].status, ToolCallStatus::Failed);
    }

    #[test]
    fn tool_call_clear_removes_all() {
        let mut states = ToolCallStateMap::default();
        upsert(&mut states, "c1", "echo", ToolCallStatus::Running, 100);
        upsert(&mut states, "c2", "calc", ToolCallStatus::Running, 100);
        ToolCallStates::apply(&mut states, ToolCallStatesUpdate::Clear);
        assert!(states.calls.is_empty());
    }

    #[test]
    fn tool_call_state_serde_roundtrip() {
        let mut states = ToolCallStateMap::default();
        upsert(&mut states, "c1", "echo", ToolCallStatus::Running, 100);
        upsert(&mut states, "c1", "echo", ToolCallStatus::Succeeded, 200);
        let json = serde_json::to_string(&states).unwrap();
        let parsed: ToolCallStateMap = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, states);
    }

    #[test]
    fn tool_call_full_lifecycle_suspend_resume_succeed() {
        let mut states = ToolCallStateMap::default();
        upsert(&mut states, "c1", "dangerous", ToolCallStatus::Running, 100);
        upsert(
            &mut states,
            "c1",
            "dangerous",
            ToolCallStatus::Suspended,
            200,
        );
        upsert(
            &mut states,
            "c1",
            "dangerous",
            ToolCallStatus::Resuming,
            300,
        );
        upsert(&mut states, "c1", "dangerous", ToolCallStatus::Running, 400);
        upsert(
            &mut states,
            "c1",
            "dangerous",
            ToolCallStatus::Succeeded,
            500,
        );
        assert_eq!(states.calls["c1"].status, ToolCallStatus::Succeeded);
        assert_eq!(states.calls["c1"].updated_at, 500);
    }

    // -----------------------------------------------------------------------
    // Migrated from uncarve: additional tool call lifecycle tests
    // -----------------------------------------------------------------------

    #[test]
    fn tool_call_new_can_transition_to_any() {
        let mut states = ToolCallStateMap::default();
        upsert(&mut states, "c1", "echo", ToolCallStatus::Succeeded, 100);
        assert_eq!(states.calls["c1"].status, ToolCallStatus::Succeeded);
    }

    #[test]
    fn tool_call_new_to_running_is_typical_path() {
        let mut states = ToolCallStateMap::default();
        upsert(&mut states, "c1", "echo", ToolCallStatus::Running, 100);
        assert_eq!(states.calls["c1"].status, ToolCallStatus::Running);
    }

    #[test]
    fn tool_call_suspended_to_succeeded_not_allowed() {
        let mut states = ToolCallStateMap::default();
        upsert(&mut states, "c1", "echo", ToolCallStatus::Running, 100);
        upsert(&mut states, "c1", "echo", ToolCallStatus::Suspended, 200);
        upsert(&mut states, "c1", "echo", ToolCallStatus::Succeeded, 300);
        assert_eq!(states.calls["c1"].status, ToolCallStatus::Suspended);
        assert_eq!(states.calls["c1"].updated_at, 200);
    }

    #[test]
    fn tool_call_map_default_is_empty() {
        let states = ToolCallStateMap::default();
        assert!(states.calls.is_empty());
    }

    #[test]
    fn tool_call_preserves_tool_name_and_arguments() {
        let mut states = ToolCallStateMap::default();
        ToolCallStates::apply(
            &mut states,
            ToolCallStatesUpdate::Upsert {
                call_id: "c1".into(),
                tool_name: "search".into(),
                arguments: serde_json::json!({"query": "test"}),
                status: ToolCallStatus::Running,
                updated_at: 100,
                resume_mode: ToolCallResumeMode::default(),
                suspension_id: OptionalField::Preserve,
                suspension_reason: OptionalField::Clear,
                resume_input: OptionalField::Clear,
            },
        );
        let call = &states.calls["c1"];
        assert_eq!(call.tool_name, "search");
        assert_eq!(call.arguments["query"], "test");
    }

    #[test]
    fn tool_call_suspension_context_roundtrip() {
        let mut states = ToolCallStateMap::default();
        ToolCallStates::apply(
            &mut states,
            ToolCallStatesUpdate::Upsert {
                call_id: "c1".into(),
                tool_name: "dangerous".into(),
                arguments: serde_json::json!({"cmd": "rm"}),
                status: ToolCallStatus::Suspended,
                updated_at: 100,
                resume_mode: ToolCallResumeMode::ReplayToolCall,
                suspension_id: OptionalField::Set("perm_c1".into()),
                suspension_reason: OptionalField::Set("tool:PermissionConfirm".into()),
                resume_input: OptionalField::Clear,
            },
        );
        ToolCallStates::apply(
            &mut states,
            ToolCallStatesUpdate::Upsert {
                call_id: "c1".into(),
                tool_name: "dangerous".into(),
                arguments: serde_json::json!({"cmd": "rm"}),
                status: ToolCallStatus::Cancelled,
                updated_at: 200,
                resume_mode: ToolCallResumeMode::ReplayToolCall,
                suspension_id: OptionalField::Preserve,
                suspension_reason: OptionalField::Preserve,
                resume_input: OptionalField::Set(ToolCallResume {
                    decision_id: "d1".into(),
                    action: awaken_contract::contract::suspension::ResumeDecisionAction::Cancel,
                    result: serde_json::json!({"approved": false}),
                    reason: Some("user denied".into()),
                    updated_at: 200,
                }),
            },
        );
        let call = &states.calls["c1"];
        assert_eq!(call.suspension_id.as_deref(), Some("perm_c1"));
        assert_eq!(
            call.suspension_reason.as_deref(),
            Some("tool:PermissionConfirm")
        );
        assert_eq!(
            call.resume_input.as_ref().map(|resume| &resume.result),
            Some(&serde_json::json!({"approved": false}))
        );
    }

    #[test]
    fn tool_call_clear_then_reuse() {
        let mut states = ToolCallStateMap::default();
        upsert(&mut states, "c1", "echo", ToolCallStatus::Running, 100);
        upsert(&mut states, "c1", "echo", ToolCallStatus::Succeeded, 200);

        ToolCallStates::apply(&mut states, ToolCallStatesUpdate::Clear);
        assert!(states.calls.is_empty());

        upsert(&mut states, "c1", "echo", ToolCallStatus::Running, 300);
        assert_eq!(states.calls["c1"].status, ToolCallStatus::Running);
    }

    #[test]
    fn tool_call_cancelled_is_terminal() {
        let mut states = ToolCallStateMap::default();
        upsert(&mut states, "c1", "echo", ToolCallStatus::Running, 100);
        upsert(&mut states, "c1", "echo", ToolCallStatus::Suspended, 200);
        upsert(&mut states, "c1", "echo", ToolCallStatus::Cancelled, 300);
        assert!(states.calls["c1"].status.is_terminal());
    }

    #[test]
    fn tool_call_succeeded_is_terminal() {
        let mut states = ToolCallStateMap::default();
        upsert(&mut states, "c1", "echo", ToolCallStatus::Running, 100);
        upsert(&mut states, "c1", "echo", ToolCallStatus::Succeeded, 200);
        assert!(states.calls["c1"].status.is_terminal());
    }

    #[test]
    fn tool_call_failed_is_terminal() {
        let mut states = ToolCallStateMap::default();
        upsert(&mut states, "c1", "echo", ToolCallStatus::Running, 100);
        upsert(&mut states, "c1", "echo", ToolCallStatus::Failed, 200);
        assert!(states.calls["c1"].status.is_terminal());
    }

    #[test]
    fn tool_call_running_is_not_terminal() {
        let mut states = ToolCallStateMap::default();
        upsert(&mut states, "c1", "echo", ToolCallStatus::Running, 100);
        assert!(!states.calls["c1"].status.is_terminal());
    }

    #[test]
    fn tool_call_many_calls_independent_lifecycle() {
        let mut states = ToolCallStateMap::default();

        upsert(&mut states, "c1", "echo", ToolCallStatus::Running, 100);
        upsert(&mut states, "c1", "echo", ToolCallStatus::Succeeded, 200);

        upsert(&mut states, "c2", "calc", ToolCallStatus::Running, 100);
        upsert(&mut states, "c2", "calc", ToolCallStatus::Failed, 200);

        upsert(&mut states, "c3", "search", ToolCallStatus::Running, 100);
        upsert(&mut states, "c3", "search", ToolCallStatus::Suspended, 200);
        upsert(&mut states, "c3", "search", ToolCallStatus::Resuming, 300);
        upsert(&mut states, "c3", "search", ToolCallStatus::Running, 400);
        upsert(&mut states, "c3", "search", ToolCallStatus::Succeeded, 500);

        assert_eq!(states.calls.len(), 3);
        assert_eq!(states.calls["c1"].status, ToolCallStatus::Succeeded);
        assert_eq!(states.calls["c2"].status, ToolCallStatus::Failed);
        assert_eq!(states.calls["c3"].status, ToolCallStatus::Succeeded);
    }
}
