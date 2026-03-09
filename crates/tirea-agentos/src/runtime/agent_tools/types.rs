//! Sub-agent types for agent run orchestration.
//!
//! These types model the persisted state of sub-agent runs
//! (created by `agent_run` / `agent_stop` / `agent_output` tools).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tirea_state::State;

/// Status of a sub-agent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubAgentStatus {
    Running,
    Completed,
    Failed,
    Stopped,
}

impl SubAgentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }
}

/// Lightweight sub-agent metadata — no embedded Thread, no cached output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgent {
    /// Thread ID for the sub-agent's independent thread in ThreadStore.
    pub thread_id: String,
    /// Parent caller run id (from caller runtime `run_id`), if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<String>,
    /// Target agent id.
    pub agent_id: String,
    /// Current run status.
    pub status: SubAgentStatus,
    /// Error message (if the run failed or was force-stopped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Persisted sub-agent state at `state["sub_agents"]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(path = "sub_agents", action = "SubAgentAction", scope = "thread")]
pub struct SubAgentState {
    /// Sub-agent runs keyed by `run_id`.
    #[tirea(default = "HashMap::new()")]
    pub runs: HashMap<String, SubAgent>,
}

/// Internal lifecycle action for `SubAgentState` reducer.
#[derive(Serialize, Deserialize)]
pub enum SubAgentAction {
    /// Set status of a sub-agent run (used by recovery plugin).
    SetStatus {
        run_id: String,
        status: SubAgentStatus,
        error: Option<String>,
    },
}

impl SubAgentState {
    fn reduce(&mut self, action: SubAgentAction) {
        match action {
            SubAgentAction::SetStatus {
                run_id,
                status,
                error,
            } => {
                if let Some(sub) = self.runs.get_mut(&run_id) {
                    sub.status = status;
                    sub.error = error;
                }
            }
        }
    }
}
