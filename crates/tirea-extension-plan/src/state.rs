use serde::{Deserialize, Serialize};
use tirea_state::State;

/// Reference to an approved plan stored as a thread message.
///
/// Plans are persisted as messages in `ThreadStore`. Any thread holding
/// a `PlanRef` can load the full plan content via the store.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanRef {
    /// Thread that owns the plan message.
    pub thread_id: String,
    /// Idempotency key of the tool call that produced the plan.
    pub plan_id: String,
    /// One-line summary for lightweight context injection.
    pub summary: String,
}

/// Persisted plan mode state.
///
/// Thread-scoped: active throughout the conversation, cleared on new thread.
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[serde(default)]
#[tirea(path = "plan_mode", action = "PlanModeAction", scope = "thread")]
pub struct PlanModeState {
    /// Whether plan mode is currently active.
    pub active: bool,
    /// Reference to the most recently approved plan.
    pub approved_plan: Option<PlanRef>,
}

impl PlanModeState {
    pub(super) fn reduce(&mut self, action: PlanModeAction) {
        match action {
            PlanModeAction::Enter => {
                self.active = true;
            }
            PlanModeAction::Exit { plan_ref } => {
                self.active = false;
                self.approved_plan = Some(plan_ref);
            }
            PlanModeAction::Deactivate => {
                self.active = false;
            }
        }
    }
}

/// Action type for the [`PlanModeState`] reducer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PlanModeAction {
    /// Activate plan mode.
    Enter,
    /// Deactivate plan mode and store the approved plan reference.
    Exit { plan_ref: PlanRef },
    /// Deactivate plan mode without storing a plan (e.g. empty plan).
    Deactivate,
}
