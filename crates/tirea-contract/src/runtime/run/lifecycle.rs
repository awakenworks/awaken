use crate::runtime::plugin::phase::state_spec::StateSpec;
use crate::runtime::state_paths::RUN_LIFECYCLE_STATE_PATH;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tirea_state::State;

/// Generic stopped payload emitted when a plugin decides to terminate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoppedReason {
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl StoppedReason {
    #[must_use]
    pub fn new(code: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            detail: None,
        }
    }

    #[must_use]
    pub fn with_detail(code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            detail: Some(detail.into()),
        }
    }
}

/// Why a run terminated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum TerminationReason {
    /// LLM returned a response with no tool calls.
    NaturalEnd,
    /// A behavior requested inference skip.
    #[serde(alias = "plugin_requested")]
    BehaviorRequested,
    /// A configured stop condition fired.
    Stopped(StoppedReason),
    /// External run cancellation signal was received.
    Cancelled,
    /// Run paused waiting for external suspended tool-call resolution.
    Suspended,
    /// Run ended due to an error path.
    Error(String),
}

impl TerminationReason {
    #[must_use]
    pub fn stopped(code: impl Into<String>) -> Self {
        Self::Stopped(StoppedReason::new(code))
    }

    #[must_use]
    pub fn stopped_with_detail(code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Stopped(StoppedReason::with_detail(code, detail))
    }

    /// Map termination reason to durable run status and optional done_reason string.
    pub fn to_run_status(&self) -> (RunStatus, Option<String>) {
        match self {
            Self::Suspended => (RunStatus::Waiting, None),
            Self::NaturalEnd => (RunStatus::Done, Some("natural".to_string())),
            Self::BehaviorRequested => (RunStatus::Done, Some("behavior_requested".to_string())),
            Self::Cancelled => (RunStatus::Done, Some("cancelled".to_string())),
            Self::Error(_) => (RunStatus::Done, Some("error".to_string())),
            Self::Stopped(stopped) => (RunStatus::Done, Some(format!("stopped:{}", stopped.code))),
        }
    }
}

/// Coarse run lifecycle status persisted in thread state.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Run is actively executing.
    #[default]
    Running,
    /// Run is waiting for external decisions.
    Waiting,
    /// Run has reached a terminal state.
    Done,
}

impl RunStatus {
    /// Canonical run-lifecycle state machine used by runtime tests.
    pub const ASCII_STATE_MACHINE: &str = r#"start
  |
  v
running -------> done
  |
  v
waiting -------> done
  |
  +-----------> running"#;

    /// Whether this lifecycle status is terminal.
    pub fn is_terminal(self) -> bool {
        matches!(self, RunStatus::Done)
    }

    /// Validate lifecycle transition from `self` to `next`.
    pub fn can_transition_to(self, next: Self) -> bool {
        if self == next {
            return true;
        }

        match self {
            RunStatus::Running => {
                matches!(next, RunStatus::Waiting | RunStatus::Done)
            }
            RunStatus::Waiting => {
                matches!(next, RunStatus::Running | RunStatus::Done)
            }
            RunStatus::Done => false,
        }
    }
}

/// Minimal durable run lifecycle envelope stored at `state["__run"]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, State, PartialEq, Eq)]
#[tirea(path = "__run")]
pub struct RunLifecycleState {
    /// Current run id associated with this lifecycle record.
    #[serde(default)]
    pub id: String,
    /// Coarse lifecycle status.
    #[serde(default)]
    pub status: RunStatus,
    /// Optional terminal reason when `status=done`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done_reason: Option<String>,
    /// Last update timestamp (unix millis).
    #[serde(default)]
    pub updated_at: u64,
}

/// Action type for [`RunLifecycleState`] reducer.
pub enum RunLifecycleAction {
    /// Set the entire run lifecycle envelope in one reducer step.
    Set {
        id: String,
        status: RunStatus,
        done_reason: Option<String>,
        updated_at: u64,
    },
}

impl StateSpec for RunLifecycleState {
    type Action = RunLifecycleAction;

    fn reduce(&mut self, action: Self::Action) {
        match action {
            RunLifecycleAction::Set {
                id,
                status,
                done_reason,
                updated_at,
            } => {
                self.id = id;
                self.status = status;
                self.done_reason = done_reason;
                self.updated_at = updated_at;
            }
        }
    }
}

/// Parse persisted run lifecycle from a rebuilt state snapshot.
pub fn run_lifecycle_from_state(state: &Value) -> Option<RunLifecycleState> {
    state
        .get(RUN_LIFECYCLE_STATE_PATH)
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::plugin::phase::state_spec::{reduce_state_actions, AnyStateAction};
    use tirea_state::apply_patch;

    #[test]
    fn run_lifecycle_roundtrip_from_state() {
        let state = serde_json::json!({
            "__run": {
                "id": "run_1",
                "status": "running",
                "updated_at": 42
            }
        });

        let lifecycle = run_lifecycle_from_state(&state).expect("run lifecycle");
        assert_eq!(lifecycle.id, "run_1");
        assert_eq!(lifecycle.status, RunStatus::Running);
        assert_eq!(lifecycle.done_reason, None);
        assert_eq!(lifecycle.updated_at, 42);
    }

    #[test]
    fn run_lifecycle_status_transitions_match_state_machine() {
        assert!(RunStatus::Running.can_transition_to(RunStatus::Waiting));
        assert!(RunStatus::Running.can_transition_to(RunStatus::Done));
        assert!(RunStatus::Waiting.can_transition_to(RunStatus::Running));
        assert!(RunStatus::Waiting.can_transition_to(RunStatus::Done));
        assert!(RunStatus::Running.can_transition_to(RunStatus::Running));
    }

    #[test]
    fn run_lifecycle_status_rejects_done_reopen_transitions() {
        assert!(!RunStatus::Done.can_transition_to(RunStatus::Running));
        assert!(!RunStatus::Done.can_transition_to(RunStatus::Waiting));
    }

    #[test]
    fn termination_reason_to_run_status_mapping() {
        let cases = vec![
            (TerminationReason::Suspended, RunStatus::Waiting, None),
            (
                TerminationReason::NaturalEnd,
                RunStatus::Done,
                Some("natural"),
            ),
            (
                TerminationReason::BehaviorRequested,
                RunStatus::Done,
                Some("behavior_requested"),
            ),
            (
                TerminationReason::Cancelled,
                RunStatus::Done,
                Some("cancelled"),
            ),
            (
                TerminationReason::Error("test error".to_string()),
                RunStatus::Done,
                Some("error"),
            ),
            (
                TerminationReason::stopped("max_turns"),
                RunStatus::Done,
                Some("stopped:max_turns"),
            ),
        ];
        for (reason, expected_status, expected_done) in cases {
            let (status, done) = reason.to_run_status();
            assert_eq!(status, expected_status, "status mismatch for {reason:?}");
            assert_eq!(
                done.as_deref(),
                expected_done,
                "done_reason mismatch for {reason:?}"
            );
        }
    }

    #[test]
    fn run_lifecycle_ascii_state_machine_contains_all_states() {
        let diagram = RunStatus::ASCII_STATE_MACHINE;
        assert!(diagram.contains("running"));
        assert!(diagram.contains("waiting"));
        assert!(diagram.contains("done"));
        assert!(diagram.contains("start"));
    }

    #[test]
    fn run_lifecycle_state_action_reduces_into_run_envelope_patch() {
        let base = serde_json::json!({});
        let actions = vec![AnyStateAction::new::<RunLifecycleState>(
            RunLifecycleAction::Set {
                id: "run_42".to_string(),
                status: RunStatus::Waiting,
                done_reason: None,
                updated_at: 99,
            },
        )];

        let patches = reduce_state_actions(actions, &base, "agent_loop").expect("reduce");
        assert_eq!(patches.len(), 1);

        let merged = apply_patch(&base, patches[0].patch()).expect("apply");
        assert_eq!(merged["__run"]["id"], serde_json::json!("run_42"));
        assert_eq!(merged["__run"]["status"], serde_json::json!("waiting"));
        assert!(merged["__run"]["done_reason"].is_null());
        assert_eq!(merged["__run"]["updated_at"], serde_json::json!(99u64));
    }
}
