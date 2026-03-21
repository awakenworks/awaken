//! Agent loop state keys — run lifecycle, tool call lifecycle, and inference override tracking.

use crate::contract::inference::InferenceOverride;
use crate::contract::lifecycle::RunStatus;
use crate::contract::suspension::ToolCallStatus;
use crate::state::{MergeStrategy, StateKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Run lifecycle state stored in the state engine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLifecycleState {
    /// Current run id.
    pub run_id: String,
    /// Coarse lifecycle status.
    pub status: RunStatus,
    /// Optional terminal reason when `status=done`.
    pub done_reason: Option<String>,
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
            Self::Start { .. } | Self::StepCompleted { .. } => RunStatus::Running,
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
        assert!(
            value.status.can_transition_to(target_status),
            "invalid lifecycle transition from {:?} to {:?}",
            value.status,
            target_status,
        );
        match update {
            RunLifecycleUpdate::Start { run_id, updated_at } => {
                value.run_id = run_id;
                value.status = RunStatus::Running;
                value.done_reason = None;
                value.updated_at = updated_at;
                value.step_count = 0;
            }
            RunLifecycleUpdate::StepCompleted { updated_at } => {
                value.step_count += 1;
                value.updated_at = updated_at;
            }
            RunLifecycleUpdate::SetWaiting { updated_at } => {
                value.status = RunStatus::Waiting;
                value.updated_at = updated_at;
            }
            RunLifecycleUpdate::Done {
                done_reason,
                updated_at,
            } => {
                value.status = RunStatus::Done;
                value.done_reason = Some(done_reason);
                value.updated_at = updated_at;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tool call lifecycle state
// ---------------------------------------------------------------------------

/// Per-tool-call lifecycle state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallState {
    pub call_id: String,
    pub tool_name: String,
    pub arguments: Value,
    pub status: ToolCallStatus,
    pub updated_at: u64,
}

/// Keyed collection of tool call states for the current step.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallStateMap {
    pub calls: HashMap<String, ToolCallState>,
}

/// Update for the tool call states key.
pub enum ToolCallStatesUpdate {
    /// Upsert a tool call's lifecycle state (validates transition).
    Upsert {
        call_id: String,
        tool_name: String,
        arguments: Value,
        status: ToolCallStatus,
        updated_at: u64,
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
            } => {
                let existing = value.calls.get(&call_id);
                let current_status = existing.map(|s| s.status).unwrap_or(ToolCallStatus::New);

                assert!(
                    current_status.can_transition_to(status),
                    "invalid tool call transition from {current_status:?} to {status:?} for call {call_id}",
                );

                value.calls.insert(
                    call_id.clone(),
                    ToolCallState {
                        call_id,
                        tool_name,
                        arguments,
                        status,
                        updated_at,
                    },
                );
            }
            ToolCallStatesUpdate::Clear => {
                value.calls.clear();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Loop-consumed action specs
// ---------------------------------------------------------------------------

/// Action spec for injecting a context message into the prompt.
///
/// Scheduled by `BeforeInference` hooks via `cmd.schedule_action::<AddContextMessage>(...)`.
/// The loop runner consumes all matching actions, groups by target, and inserts into
/// the message list at the appropriate positions before building the `InferenceRequest`.
pub struct AddContextMessage;

impl crate::model::ScheduledActionSpec for AddContextMessage {
    const KEY: &'static str = "runtime.add_context_message";
    const PHASE: crate::model::Phase = crate::model::Phase::BeforeInference;
    type Payload = crate::contract::context_message::ContextMessage;
}

/// Action spec for per-inference parameter overrides.
///
/// Scheduled by `BeforeInference` hooks via `cmd.schedule_action::<SetInferenceOverride>(...)`.
/// The loop runner consumes all matching actions before building the `InferenceRequest`,
/// merging payloads with last-wins semantics per field. No handler registration needed.
pub struct SetInferenceOverride;

impl crate::model::ScheduledActionSpec for SetInferenceOverride {
    const KEY: &'static str = "runtime.set_inference_override";
    const PHASE: crate::model::Phase = crate::model::Phase::BeforeInference;
    type Payload = InferenceOverride;
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
        assert_eq!(state.done_reason.as_deref(), Some("natural"));
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
            RunLifecycleUpdate::SetWaiting { updated_at: 150 },
        );
        assert_eq!(state.status, RunStatus::Waiting);
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
    #[should_panic(expected = "invalid lifecycle transition")]
    fn run_lifecycle_rejects_done_to_running() {
        let mut state = RunLifecycleState {
            run_id: "r1".into(),
            status: RunStatus::Done,
            done_reason: Some("natural".into()),
            updated_at: 100,
            step_count: 1,
        };
        // Done -> Running is an invalid transition
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::Start {
                run_id: "r2".into(),
                updated_at: 200,
            },
        );
    }

    #[test]
    #[should_panic(expected = "invalid lifecycle transition")]
    fn run_lifecycle_rejects_done_to_waiting() {
        let mut state = RunLifecycleState {
            run_id: "r1".into(),
            status: RunStatus::Done,
            done_reason: Some("natural".into()),
            updated_at: 100,
            step_count: 1,
        };
        // Done -> Waiting is an invalid transition
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::SetWaiting { updated_at: 200 },
        );
    }

    #[test]
    #[should_panic(expected = "invalid lifecycle transition")]
    fn run_lifecycle_rejects_done_to_step_completed() {
        let mut state = RunLifecycleState {
            run_id: "r1".into(),
            status: RunStatus::Done,
            done_reason: Some("natural".into()),
            updated_at: 100,
            step_count: 1,
        };
        // Done -> Running (via StepCompleted) is invalid
        RunLifecycle::apply(
            &mut state,
            RunLifecycleUpdate::StepCompleted { updated_at: 200 },
        );
    }

    #[test]
    fn run_lifecycle_allows_waiting_to_running_via_start() {
        let mut state = RunLifecycleState {
            run_id: "r1".into(),
            status: RunStatus::Waiting,
            done_reason: None,
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
            done_reason: Some("natural".into()),
            updated_at: 12345,
            step_count: 3,
        };
        let json = serde_json::to_string(&state).unwrap();
        let parsed: RunLifecycleState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, state);
    }

    // -------------------------------------------------------------------
    // Tool call lifecycle tests
    // -------------------------------------------------------------------

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
    #[should_panic(expected = "invalid tool call transition")]
    fn tool_call_rejects_succeeded_to_running() {
        let mut states = ToolCallStateMap::default();
        upsert(&mut states, "c1", "echo", ToolCallStatus::Running, 100);
        upsert(&mut states, "c1", "echo", ToolCallStatus::Succeeded, 200);
        // Terminal -> Running is invalid
        upsert(&mut states, "c1", "echo", ToolCallStatus::Running, 300);
    }

    #[test]
    #[should_panic(expected = "invalid tool call transition")]
    fn tool_call_rejects_failed_to_running() {
        let mut states = ToolCallStateMap::default();
        upsert(&mut states, "c1", "echo", ToolCallStatus::Running, 100);
        upsert(&mut states, "c1", "echo", ToolCallStatus::Failed, 200);
        upsert(&mut states, "c1", "echo", ToolCallStatus::Running, 300);
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
        // New -> Running
        upsert(&mut states, "c1", "dangerous", ToolCallStatus::Running, 100);
        // Running -> Suspended
        upsert(
            &mut states,
            "c1",
            "dangerous",
            ToolCallStatus::Suspended,
            200,
        );
        // Suspended -> Resuming
        upsert(
            &mut states,
            "c1",
            "dangerous",
            ToolCallStatus::Resuming,
            300,
        );
        // Resuming -> Running
        upsert(&mut states, "c1", "dangerous", ToolCallStatus::Running, 400);
        // Running -> Succeeded
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
}
