use super::state::{HandoffAction, HandoffState};
use tirea_contract::runtime::state::AnyStateAction;

/// Create a state action that requests a handoff to another agent variant.
pub fn request_handoff_action(agent: &str) -> AnyStateAction {
    AnyStateAction::new::<HandoffState>(HandoffAction::Request {
        agent: agent.to_string(),
    })
}

/// Create a state action that activates a handoff.
pub fn activate_handoff_action(agent: &str) -> AnyStateAction {
    AnyStateAction::new::<HandoffState>(HandoffAction::Activate {
        agent: agent.to_string(),
    })
}
