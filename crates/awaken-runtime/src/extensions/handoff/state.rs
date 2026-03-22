use serde::{Deserialize, Serialize};

use crate::state::StateKey;

use super::action::HandoffAction;

/// Persisted handoff state — tracks the active agent variant and any pending request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HandoffState {
    /// The currently active agent variant (`None` = base configuration).
    pub active_agent: Option<String>,
    /// A handoff requested by the tool, pending activation.
    pub requested_agent: Option<String>,
}

impl HandoffState {
    pub(crate) fn reduce(&mut self, action: HandoffAction) {
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

/// State key for the handoff state.
pub struct ActiveAgentKey;

impl StateKey for ActiveAgentKey {
    const KEY: &'static str = "agent_handoff";
    type Value = HandoffState;
    type Update = HandoffAction;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        value.reduce(update);
    }
}
