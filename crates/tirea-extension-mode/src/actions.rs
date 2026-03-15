use super::state::{AgentModeAction, AgentModeState};
use tirea_contract::runtime::state::AnyStateAction;

/// Create a state action that requests a mode switch.
pub fn request_mode_switch_action(mode: &str) -> AnyStateAction {
    AnyStateAction::new::<AgentModeState>(AgentModeAction::RequestSwitch {
        mode: mode.to_string(),
    })
}

/// Create a state action that activates a mode after re-resolution.
pub fn activate_mode_action(mode: &str) -> AnyStateAction {
    AnyStateAction::new::<AgentModeState>(AgentModeAction::Activate {
        mode: mode.to_string(),
    })
}
