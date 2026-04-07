use crate::state::StateKey;
use awaken_contract::contract::lifecycle::RunStatus;
use serde::{Deserialize, Serialize};

/// Run lifecycle state stored in the state engine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLifecycleState {
    /// Current run id.
    pub run_id: String,
    /// Coarse lifecycle status.
    pub status: RunStatus,
    /// Reason string for the current status (set when Done or Waiting, None when Running).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "done_reason",
        alias = "pause_reason"
    )]
    pub status_reason: Option<String>,
    /// Last update timestamp (unix millis).
    pub updated_at: u64,
    /// Total steps completed.
    pub step_count: u32,
}

/// Update for the run lifecycle state key.
pub enum RunLifecycleUpdate {
    Start {
        run_id: String,
        updated_at: u64,
    },
    StepCompleted {
        updated_at: u64,
    },
    SetWaiting {
        updated_at: u64,
        pause_reason: String,
    },
    SetRunning {
        updated_at: u64,
    },
    Done {
        done_reason: String,
        updated_at: u64,
    },
}

impl RunLifecycleUpdate {
    /// The target `RunStatus` this update will produce.
    pub fn target_status(&self) -> RunStatus {
        match self {
            Self::Start { .. } | Self::StepCompleted { .. } | Self::SetRunning { .. } => {
                RunStatus::Running
            }
            Self::SetWaiting { .. } => RunStatus::Waiting,
            Self::Done { .. } => RunStatus::Done,
        }
    }
}

/// State key for run lifecycle tracking.
pub struct RunLifecycle;

impl StateKey for RunLifecycle {
    const KEY: &'static str = "__runtime.run_lifecycle";

    type Value = RunLifecycleState;
    type Update = RunLifecycleUpdate;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        let target_status = update.target_status();
        if !value.status.can_transition_to(target_status) {
            tracing::error!(
                from = ?value.status,
                to = ?target_status,
                "invalid lifecycle transition — skipping update"
            );
            return;
        }
        match update {
            RunLifecycleUpdate::Start { run_id, updated_at } => {
                value.run_id = run_id;
                value.status = RunStatus::Running;
                value.status_reason = None;
                value.updated_at = updated_at;
                value.step_count = 0;
            }
            RunLifecycleUpdate::StepCompleted { updated_at } => {
                value.step_count += 1;
                value.updated_at = updated_at;
            }
            RunLifecycleUpdate::SetWaiting {
                updated_at,
                pause_reason,
            } => {
                value.status = RunStatus::Waiting;
                value.status_reason = Some(pause_reason);
                value.updated_at = updated_at;
            }
            RunLifecycleUpdate::SetRunning { updated_at } => {
                value.status = RunStatus::Running;
                value.status_reason = None;
                value.updated_at = updated_at;
            }
            RunLifecycleUpdate::Done {
                done_reason,
                updated_at,
            } => {
                value.status = RunStatus::Done;
                value.status_reason = Some(done_reason);
                value.updated_at = updated_at;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    #[test]
    fn run_lifecycle_start_sets_running() {
        let mut state = RunLifecycleState::default();
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Start {
                run_id: "r1".into(),
                updated_at: 100,
            },
        );
        assert_eq!(state.run_id, "r1");
        assert_eq!(state.status, RunStatus::Running);
        assert_eq!(state.step_count, 0);
    }

    #[test]
    fn run_lifecycle_step_completed_increments() {
        let mut state = RunLifecycleState::default();
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Start {
                run_id: "r1".into(),
                updated_at: 100,
            },
        );
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::StepCompleted { updated_at: 200 },
        );
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::StepCompleted { updated_at: 300 },
        );
        assert_eq!(state.step_count, 2);
        assert_eq!(state.updated_at, 300);
    }

    #[test]
    fn run_lifecycle_done_sets_terminal() {
        let mut state = RunLifecycleState::default();
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Start {
                run_id: "r1".into(),
                updated_at: 100,
            },
        );
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Done {
                done_reason: "natural".into(),
                updated_at: 200,
            },
        );
        assert_eq!(state.status, RunStatus::Done);
        assert_eq!(state.status_reason.as_deref(), Some("natural"));
        assert!(state.status.is_terminal());
    }

    #[test]
    fn run_lifecycle_waiting_transition() {
        let mut state = RunLifecycleState::default();
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Start {
                run_id: "r1".into(),
                updated_at: 100,
            },
        );
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::SetWaiting {
                updated_at: 150,
                pause_reason: "suspended".into(),
            },
        );
        assert_eq!(state.status, RunStatus::Waiting);
        assert_eq!(state.status_reason.as_deref(), Some("suspended"));
    }

    #[test]
    fn run_lifecycle_status_reason_set_and_cleared() {
        let mut state = RunLifecycleState::default();
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Start {
                run_id: "r1".into(),
                updated_at: 100,
            },
        );
        assert!(state.status_reason.is_none());

        // SetWaiting stores status_reason
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::SetWaiting {
                updated_at: 150,
                pause_reason: "awaiting_tasks".into(),
            },
        );
        assert_eq!(state.status_reason.as_deref(), Some("awaiting_tasks"));

        // SetRunning clears status_reason
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::SetRunning { updated_at: 200 },
        );
        assert!(state.status_reason.is_none());

        // SetWaiting again with different reason
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::SetWaiting {
                updated_at: 250,
                pause_reason: "user_input_required".into(),
            },
        );
        assert_eq!(state.status_reason.as_deref(), Some("user_input_required"));

        // Done sets status_reason
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Done {
                done_reason: "finished".into(),
                updated_at: 300,
            },
        );
        assert_eq!(state.status_reason.as_deref(), Some("finished"));
    }

    #[test]
    fn run_lifecycle_status_reason_cleared_on_start() {
        let mut state = RunLifecycleState {
            run_id: "r1".into(),
            status: RunStatus::Waiting,
            status_reason: Some("old_reason".into()),
            updated_at: 100,
            step_count: 1,
        };
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Start {
                run_id: "r2".into(),
                updated_at: 200,
            },
        );
        assert!(state.status_reason.is_none());
    }

    #[test]
    fn run_lifecycle_full_sequence() {
        let mut state = RunLifecycleState::default();
        let t = now_ms();

        // Start
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Start {
                run_id: "run-42".into(),
                updated_at: t,
            },
        );
        assert_eq!(state.status, RunStatus::Running);

        // Steps
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::StepCompleted { updated_at: t + 1 },
        );
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::StepCompleted { updated_at: t + 2 },
        );
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::StepCompleted { updated_at: t + 3 },
        );
        assert_eq!(state.step_count, 3);

        // Done
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Done {
                done_reason: "stopped:max_turns".into(),
                updated_at: t + 4,
            },
        );
        assert_eq!(state.status, RunStatus::Done);
        assert_eq!(state.step_count, 3);
    }

    #[test]
    fn run_lifecycle_rejects_done_to_running() {
        let mut state = RunLifecycleState {
            run_id: "r1".into(),
            status: RunStatus::Done,
            status_reason: Some("natural".into()),
            updated_at: 100,
            step_count: 1,
        };
        // Done -> Running should be rejected (state unchanged)
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Start {
                run_id: "r2".into(),
                updated_at: 200,
            },
        );
        assert_eq!(state.status, RunStatus::Done);
        assert_eq!(state.run_id, "r1");
        assert_eq!(state.updated_at, 100);
    }

    #[test]
    fn run_lifecycle_rejects_done_to_waiting() {
        let mut state = RunLifecycleState {
            run_id: "r1".into(),
            status: RunStatus::Done,
            status_reason: Some("natural".into()),
            updated_at: 100,
            step_count: 1,
        };
        // Done -> Waiting should be rejected (state unchanged)
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::SetWaiting {
                updated_at: 200,
                pause_reason: "suspended".into(),
            },
        );
        assert_eq!(state.status, RunStatus::Done);
        assert_eq!(state.updated_at, 100);
    }

    #[test]
    fn run_lifecycle_rejects_done_to_step_completed() {
        let mut state = RunLifecycleState {
            run_id: "r1".into(),
            status: RunStatus::Done,
            status_reason: Some("natural".into()),
            updated_at: 100,
            step_count: 1,
        };
        // Done -> Running (via StepCompleted) should be rejected (state unchanged)
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::StepCompleted { updated_at: 200 },
        );
        assert_eq!(state.status, RunStatus::Done);
        assert_eq!(state.step_count, 1);
        assert_eq!(state.updated_at, 100);
    }

    #[test]
    fn run_lifecycle_allows_waiting_to_running_via_start() {
        let mut state = RunLifecycleState {
            run_id: "r1".into(),
            status: RunStatus::Waiting,
            status_reason: Some("suspended".into()),
            updated_at: 100,
            step_count: 1,
        };
        // Waiting -> Running is valid
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Start {
                run_id: "r2".into(),
                updated_at: 200,
            },
        );
        assert_eq!(state.status, RunStatus::Running);
    }

    #[test]
    fn run_lifecycle_state_serde_roundtrip() {
        let state = RunLifecycleState {
            run_id: "r1".into(),
            status: RunStatus::Done,
            status_reason: Some("natural".into()),
            updated_at: 12345,
            step_count: 3,
        };
        let json = serde_json::to_string(&state).unwrap();
        let parsed: RunLifecycleState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, state);
    }

    // -----------------------------------------------------------------------
    // Migrated from uncarve: additional lifecycle tests
    // -----------------------------------------------------------------------

    #[test]
    fn run_lifecycle_default_state() {
        let state = RunLifecycleState::default();
        assert!(state.run_id.is_empty());
        assert_eq!(state.status, RunStatus::default());
        assert!(state.status_reason.is_none());
        assert_eq!(state.step_count, 0);
        assert_eq!(state.updated_at, 0);
    }

    #[test]
    fn run_lifecycle_multiple_steps_then_done() {
        let mut state = RunLifecycleState::default();
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Start {
                run_id: "r1".into(),
                updated_at: 100,
            },
        );

        for step in 1..=10u64 {
            RunLifecycle::apply(
                &mut state,
                RunLifecycleUpdate::StepCompleted {
                    updated_at: 100 + step,
                },
            );
        }
        assert_eq!(state.step_count, 10);
        assert_eq!(state.status, RunStatus::Running);

        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Done {
                done_reason: "max_rounds".into(),
                updated_at: 200,
            },
        );
        assert_eq!(state.status, RunStatus::Done);
        assert_eq!(state.step_count, 10);
    }

    #[test]
    fn run_lifecycle_waiting_to_done() {
        let mut state = RunLifecycleState::default();
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Start {
                run_id: "r1".into(),
                updated_at: 100,
            },
        );
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::SetWaiting {
                updated_at: 150,
                pause_reason: "suspended".into(),
            },
        );
        assert_eq!(state.status, RunStatus::Waiting);

        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Done {
                done_reason: "cancelled".into(),
                updated_at: 200,
            },
        );
        assert_eq!(state.status, RunStatus::Done);
    }

    #[test]
    fn run_lifecycle_waiting_to_running_to_waiting() {
        let mut state = RunLifecycleState::default();
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Start {
                run_id: "r1".into(),
                updated_at: 100,
            },
        );
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::SetWaiting {
                updated_at: 150,
                pause_reason: "suspended".into(),
            },
        );
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::SetRunning { updated_at: 200 },
        );
        assert_eq!(state.status, RunStatus::Running);
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::SetWaiting {
                updated_at: 250,
                pause_reason: "suspended".into(),
            },
        );
        assert_eq!(state.status, RunStatus::Waiting);
    }

    #[test]
    fn run_lifecycle_update_target_status() {
        assert_eq!(
            RunLifecycleUpdate::Start {
                run_id: "r".into(),
                updated_at: 0,
            }
            .target_status(),
            RunStatus::Running
        );
        assert_eq!(
            RunLifecycleUpdate::StepCompleted { updated_at: 0 }.target_status(),
            RunStatus::Running
        );
        assert_eq!(
            RunLifecycleUpdate::SetWaiting {
                updated_at: 0,
                pause_reason: "test".into()
            }
            .target_status(),
            RunStatus::Waiting
        );
        assert_eq!(
            RunLifecycleUpdate::SetRunning { updated_at: 0 }.target_status(),
            RunStatus::Running
        );
        assert_eq!(
            RunLifecycleUpdate::Done {
                done_reason: "done".into(),
                updated_at: 0,
            }
            .target_status(),
            RunStatus::Done
        );
    }

    #[test]
    fn run_lifecycle_start_resets_step_count() {
        let mut state = RunLifecycleState::default();
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Start {
                run_id: "r1".into(),
                updated_at: 100,
            },
        );
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::StepCompleted { updated_at: 200 },
        );
        assert_eq!(state.step_count, 1);

        // Transition to waiting, then start a new run
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::SetWaiting {
                updated_at: 250,
                pause_reason: "suspended".into(),
            },
        );
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Start {
                run_id: "r2".into(),
                updated_at: 300,
            },
        );
        assert_eq!(state.step_count, 0, "step count should reset on new start");
        assert_eq!(state.run_id, "r2");
    }

    #[test]
    fn run_lifecycle_done_preserves_step_count() {
        let mut state = RunLifecycleState::default();
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Start {
                run_id: "r1".into(),
                updated_at: 100,
            },
        );
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::StepCompleted { updated_at: 200 },
        );
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::StepCompleted { updated_at: 300 },
        );
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Done {
                done_reason: "finished".into(),
                updated_at: 400,
            },
        );
        assert_eq!(state.step_count, 2, "done should not reset step count");
    }

    #[test]
    fn run_lifecycle_state_equality() {
        let s1 = RunLifecycleState {
            run_id: "r1".into(),
            status: RunStatus::Running,
            status_reason: None,
            updated_at: 100,
            step_count: 3,
        };
        let s2 = s1.clone();
        assert_eq!(s1, s2);

        let s3 = RunLifecycleState {
            step_count: 4,
            ..s1.clone()
        };
        assert_ne!(s1, s3);
    }

    #[test]
    fn run_lifecycle_status_is_terminal() {
        let mut state = RunLifecycleState::default();
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Start {
                run_id: "r1".into(),
                updated_at: 100,
            },
        );
        assert!(!state.status.is_terminal());

        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Done {
                done_reason: "done".into(),
                updated_at: 200,
            },
        );
        assert!(state.status.is_terminal());
    }

    // -----------------------------------------------------------------------
    // Continuation semantics: SetRunning preserves step_count
    // -----------------------------------------------------------------------

    #[test]
    fn continuation_set_running_preserves_step_count() {
        let mut state = RunLifecycleState::default();
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Start {
                run_id: "r1".into(),
                updated_at: 100,
            },
        );
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::StepCompleted { updated_at: 200 },
        );
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::StepCompleted { updated_at: 300 },
        );
        assert_eq!(state.step_count, 2);

        // Simulate awaiting_tasks → continuation
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::SetWaiting {
                updated_at: 400,
                pause_reason: "awaiting_tasks".into(),
            },
        );
        // Continuation: SetRunning instead of Start → step_count preserved
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::SetRunning { updated_at: 500 },
        );
        assert_eq!(state.status, RunStatus::Running);
        assert_eq!(state.step_count, 2, "continuation must preserve step_count");
        assert!(state.status_reason.is_none());
    }

    #[test]
    fn new_start_resets_step_count_after_waiting() {
        let mut state = RunLifecycleState::default();
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Start {
                run_id: "r1".into(),
                updated_at: 100,
            },
        );
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::StepCompleted { updated_at: 200 },
        );
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::SetWaiting {
                updated_at: 300,
                pause_reason: "awaiting_tasks".into(),
            },
        );
        // New start (not continuation) → step_count resets
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Start {
                run_id: "r2".into(),
                updated_at: 400,
            },
        );
        assert_eq!(state.step_count, 0, "new Start must reset step_count");
        assert_eq!(state.run_id, "r2");
    }

    #[test]
    fn status_reason_serde_backward_compat_missing() {
        // Old serialized state without any reason field
        let json = r#"{"run_id":"r1","status":"waiting","updated_at":100,"step_count":0}"#;
        let parsed: RunLifecycleState = serde_json::from_str(json).unwrap();
        assert!(
            parsed.status_reason.is_none(),
            "missing status_reason should deserialize as None"
        );
    }

    #[test]
    fn status_reason_serde_backward_compat_done_reason_alias() {
        // Old serialized state with done_reason field
        let json = r#"{"run_id":"r1","status":"done","done_reason":"natural","updated_at":100,"step_count":1}"#;
        let parsed: RunLifecycleState = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.status_reason.as_deref(), Some("natural"));
    }

    #[test]
    fn status_reason_serde_backward_compat_pause_reason_alias() {
        // Old serialized state with pause_reason field
        let json = r#"{"run_id":"r1","status":"waiting","pause_reason":"awaiting_tasks","updated_at":100,"step_count":2}"#;
        let parsed: RunLifecycleState = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.status_reason.as_deref(), Some("awaiting_tasks"));
    }

    #[test]
    fn status_reason_included_in_serde() {
        let state = RunLifecycleState {
            run_id: "r1".into(),
            status: RunStatus::Waiting,
            status_reason: Some("awaiting_tasks".into()),
            updated_at: 100,
            step_count: 2,
        };
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("awaiting_tasks"));
        let parsed: RunLifecycleState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.status_reason.as_deref(), Some("awaiting_tasks"));
    }
}
