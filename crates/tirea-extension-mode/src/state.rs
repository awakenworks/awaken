use serde::{Deserialize, Serialize};
use tirea_state::State;

/// Persisted agent mode state.
///
/// Thread-scoped: tracks the active mode and any pending switch request.
/// `requested_mode` is set by [`SwitchModeTool`] and consumed by the
/// `AgentOs::run()` auto-restart loop after re-resolution.
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[serde(default)]
#[tirea(path = "agent_mode", action = "AgentModeAction", scope = "thread")]
pub struct AgentModeState {
    /// The currently active mode (None = base configuration).
    pub active_mode: Option<String>,
    /// A mode switch requested by the tool, pending activation.
    pub requested_mode: Option<String>,
}

impl AgentModeState {
    pub(super) fn reduce(&mut self, action: AgentModeAction) {
        match action {
            AgentModeAction::RequestSwitch { mode } => {
                self.requested_mode = Some(mode);
            }
            AgentModeAction::Activate { mode } => {
                self.active_mode = Some(mode);
                self.requested_mode = None;
            }
            AgentModeAction::Clear => {
                self.active_mode = None;
                self.requested_mode = None;
            }
        }
    }
}

/// Action type for the [`AgentModeState`] reducer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentModeAction {
    /// Request a mode switch (written by SwitchModeTool).
    RequestSwitch { mode: String },
    /// Activate the mode after re-resolution (written by AgentOs::run loop).
    Activate { mode: String },
    /// Clear all mode state.
    Clear,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_is_all_none() {
        let state = AgentModeState::default();
        assert!(state.active_mode.is_none());
        assert!(state.requested_mode.is_none());
    }

    #[test]
    fn request_switch_sets_requested_mode() {
        let mut state = AgentModeState::default();
        state.reduce(AgentModeAction::RequestSwitch {
            mode: "fast".into(),
        });
        assert!(state.active_mode.is_none());
        assert_eq!(state.requested_mode.as_deref(), Some("fast"));
    }

    #[test]
    fn activate_sets_active_and_clears_requested() {
        let mut state = AgentModeState {
            active_mode: None,
            requested_mode: Some("fast".into()),
        };
        state.reduce(AgentModeAction::Activate {
            mode: "fast".into(),
        });
        assert_eq!(state.active_mode.as_deref(), Some("fast"));
        assert!(state.requested_mode.is_none());
    }

    #[test]
    fn clear_resets_both() {
        let mut state = AgentModeState {
            active_mode: Some("fast".into()),
            requested_mode: Some("deep".into()),
        };
        state.reduce(AgentModeAction::Clear);
        assert!(state.active_mode.is_none());
        assert!(state.requested_mode.is_none());
    }

    #[test]
    fn roundtrip_serialization() {
        let state = AgentModeState {
            active_mode: Some("fast".into()),
            requested_mode: None,
        };
        let json = serde_json::to_value(&state).unwrap();
        let back: AgentModeState = serde_json::from_value(json).unwrap();
        assert_eq!(state.active_mode, back.active_mode);
        assert_eq!(state.requested_mode, back.requested_mode);
    }

    #[test]
    fn action_roundtrip() {
        let action = AgentModeAction::RequestSwitch {
            mode: "fast".into(),
        };
        let json = serde_json::to_value(&action).unwrap();
        let back: AgentModeAction = serde_json::from_value(json).unwrap();
        assert_eq!(action, back);
    }
}
