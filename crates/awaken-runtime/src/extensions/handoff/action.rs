use serde::{Deserialize, Serialize};

/// Action type for the `HandoffState` reducer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HandoffAction {
    /// Request a handoff to another agent variant.
    Request { agent: String },
    /// Activate the handoff (consumed by the plugin).
    Activate { agent: String },
    /// Clear all handoff state (return to base agent).
    Clear,
}

/// Create a handoff request mutation.
pub fn request_handoff(agent: impl Into<String>) -> HandoffAction {
    HandoffAction::Request {
        agent: agent.into(),
    }
}

/// Create a handoff activation mutation.
pub fn activate_handoff(agent: impl Into<String>) -> HandoffAction {
    HandoffAction::Activate {
        agent: agent.into(),
    }
}

/// Create a clear-handoff mutation.
pub fn clear_handoff() -> HandoffAction {
    HandoffAction::Clear
}
