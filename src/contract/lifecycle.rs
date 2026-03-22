//! Run lifecycle, termination reasons, and stop condition specifications.

use serde::{Deserialize, Serialize};

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
    /// A tool permission checker blocked the run.
    Blocked(String),
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

    /// Reconstruct a `TerminationReason` from the `done_reason` string stored in `RunLifecycleState`.
    pub fn from_done_reason(reason: &str) -> Self {
        match reason {
            "natural" => Self::NaturalEnd,
            "behavior_requested" => Self::BehaviorRequested,
            "cancelled" => Self::Cancelled,
            s if s.starts_with("blocked:") => {
                Self::Blocked(s.trim_start_matches("blocked:").to_string())
            }
            s if s.starts_with("stopped:") => {
                Self::Stopped(StoppedReason::new(s.trim_start_matches("stopped:")))
            }
            s if s.starts_with("error:") => Self::Error(s.trim_start_matches("error:").to_string()),
            other => Self::Error(other.to_string()),
        }
    }

    /// Map termination reason to durable run status and optional done_reason string.
    pub fn to_run_status(&self) -> (RunStatus, Option<String>) {
        match self {
            Self::Suspended => (RunStatus::Waiting, None),
            Self::NaturalEnd => (RunStatus::Done, Some("natural".to_string())),
            Self::BehaviorRequested => (RunStatus::Done, Some("behavior_requested".to_string())),
            Self::Cancelled => (RunStatus::Done, Some("cancelled".to_string())),
            Self::Blocked(reason) => (RunStatus::Done, Some(format!("blocked:{reason}"))),
            Self::Error(_) => (RunStatus::Done, Some("error".to_string())),
            Self::Stopped(stopped) => (RunStatus::Done, Some(format!("stopped:{}", stopped.code))),
        }
    }
}

/// Coarse run lifecycle status.
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
            RunStatus::Running => matches!(next, RunStatus::Waiting | RunStatus::Done),
            RunStatus::Waiting => matches!(next, RunStatus::Running | RunStatus::Done),
            RunStatus::Done => false,
        }
    }
}

/// Declarative stop-condition configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StopConditionSpec {
    MaxRounds { rounds: usize },
    Timeout { seconds: u64 },
    TokenBudget { max_total: usize },
    ConsecutiveErrors { max: usize },
    StopOnTool { tool_name: String },
    ContentMatch { pattern: String },
    LoopDetection { window: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_status_transitions_match_state_machine() {
        assert!(RunStatus::Running.can_transition_to(RunStatus::Waiting));
        assert!(RunStatus::Running.can_transition_to(RunStatus::Done));
        assert!(RunStatus::Waiting.can_transition_to(RunStatus::Running));
        assert!(RunStatus::Waiting.can_transition_to(RunStatus::Done));
        assert!(RunStatus::Running.can_transition_to(RunStatus::Running));
    }

    #[test]
    fn run_status_rejects_done_reopen_transitions() {
        assert!(!RunStatus::Done.can_transition_to(RunStatus::Running));
        assert!(!RunStatus::Done.can_transition_to(RunStatus::Waiting));
    }

    #[test]
    fn run_status_terminal_matches_done_only() {
        assert!(!RunStatus::Running.is_terminal());
        assert!(!RunStatus::Waiting.is_terminal());
        assert!(RunStatus::Done.is_terminal());
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
                TerminationReason::Blocked("unsafe tool".to_string()),
                RunStatus::Done,
                Some("blocked:unsafe tool"),
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
    fn termination_reason_serde_roundtrip() {
        let reasons = vec![
            TerminationReason::NaturalEnd,
            TerminationReason::BehaviorRequested,
            TerminationReason::stopped_with_detail("max_turns", "reached 10 rounds"),
            TerminationReason::Cancelled,
            TerminationReason::Blocked("unsafe".into()),
            TerminationReason::Suspended,
            TerminationReason::Error("oops".into()),
        ];
        for reason in reasons {
            let json = serde_json::to_string(&reason).unwrap();
            let parsed: TerminationReason = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, reason);
        }
    }

    #[test]
    fn stopped_reason_helpers_build_expected_values() {
        let simple = StoppedReason::new("budget");
        assert_eq!(simple.code, "budget");
        assert!(simple.detail.is_none());

        let detailed = StoppedReason::with_detail("budget", "limit reached");
        assert_eq!(detailed.code, "budget");
        assert_eq!(detailed.detail.as_deref(), Some("limit reached"));

        assert_eq!(
            TerminationReason::stopped("budget"),
            TerminationReason::Stopped(simple)
        );
        assert_eq!(
            TerminationReason::stopped_with_detail("budget", "limit reached"),
            TerminationReason::Stopped(detailed)
        );
    }

    #[test]
    fn stop_condition_spec_serde_roundtrip() {
        let specs = vec![
            StopConditionSpec::MaxRounds { rounds: 10 },
            StopConditionSpec::Timeout { seconds: 300 },
            StopConditionSpec::TokenBudget { max_total: 100_000 },
            StopConditionSpec::ConsecutiveErrors { max: 3 },
            StopConditionSpec::StopOnTool {
                tool_name: "done".into(),
            },
            StopConditionSpec::ContentMatch {
                pattern: r"\bDONE\b".into(),
            },
            StopConditionSpec::LoopDetection { window: 5 },
        ];
        for spec in specs {
            let json = serde_json::to_string(&spec).unwrap();
            let parsed: StopConditionSpec = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, spec);
        }
    }

    #[test]
    fn run_status_serde_roundtrip() {
        for status in [RunStatus::Running, RunStatus::Waiting, RunStatus::Done] {
            let json = serde_json::to_string(&status).unwrap();
            let parsed: RunStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, status);
        }
    }
}
