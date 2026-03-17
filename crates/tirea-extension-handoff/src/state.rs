use serde::{Deserialize, Serialize};
use tirea_state::State;

/// Persisted agent handoff state.
///
/// Thread-scoped: tracks the active agent variant and any pending handoff request.
/// `requested_agent` is set by `AgentHandoffTool` and consumed by
/// `HandoffPlugin` in the next `before_inference` phase.
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[serde(default)]
#[tirea(path = "agent_handoff", action = "HandoffAction", scope = "thread")]
pub struct HandoffState {
    /// The currently active agent variant (None = base configuration).
    #[serde(alias = "active_mode")]
    pub active_agent: Option<String>,
    /// A handoff requested by the tool, pending activation.
    #[serde(alias = "requested_mode")]
    pub requested_agent: Option<String>,
}

impl HandoffState {
    pub(super) fn reduce(&mut self, action: HandoffAction) {
        match action {
            HandoffAction::Request { agent } => {
                self.requested_agent = Some(agent);
            }
            HandoffAction::Activate { agent } => {
                self.active_agent = Some(agent);
                self.requested_agent = None;
            }
            HandoffAction::Clear => {
                self.active_agent = None;
                self.requested_agent = None;
            }
        }
    }
}

/// Action type for the [`HandoffState`] reducer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HandoffAction {
    /// Request a handoff to another agent variant.
    Request { agent: String },
    /// Activate the handoff (written by HandoffPlugin).
    Activate { agent: String },
    /// Clear all handoff state.
    Clear,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_is_all_none() {
        let state = HandoffState::default();
        assert!(state.active_agent.is_none());
        assert!(state.requested_agent.is_none());
    }

    #[test]
    fn request_sets_requested_agent() {
        let mut state = HandoffState::default();
        state.reduce(HandoffAction::Request {
            agent: "fast".into(),
        });
        assert!(state.active_agent.is_none());
        assert_eq!(state.requested_agent.as_deref(), Some("fast"));
    }

    #[test]
    fn activate_sets_active_and_clears_requested() {
        let mut state = HandoffState {
            active_agent: None,
            requested_agent: Some("fast".into()),
        };
        state.reduce(HandoffAction::Activate {
            agent: "fast".into(),
        });
        assert_eq!(state.active_agent.as_deref(), Some("fast"));
        assert!(state.requested_agent.is_none());
    }

    #[test]
    fn clear_resets_both() {
        let mut state = HandoffState {
            active_agent: Some("fast".into()),
            requested_agent: Some("deep".into()),
        };
        state.reduce(HandoffAction::Clear);
        assert!(state.active_agent.is_none());
        assert!(state.requested_agent.is_none());
    }

    #[test]
    fn roundtrip_serialization() {
        let state = HandoffState {
            active_agent: Some("fast".into()),
            requested_agent: None,
        };
        let json = serde_json::to_value(&state).unwrap();
        let back: HandoffState = serde_json::from_value(json).unwrap();
        assert_eq!(state.active_agent, back.active_agent);
        assert_eq!(state.requested_agent, back.requested_agent);
    }

    #[test]
    fn action_roundtrip() {
        let action = HandoffAction::Request {
            agent: "fast".into(),
        };
        let json = serde_json::to_value(&action).unwrap();
        let back: HandoffAction = serde_json::from_value(json).unwrap();
        assert_eq!(action, back);
    }
}
