use awaken_contract::StateError;
use awaken_contract::contract::active_agent::ActiveAgentIdKey;

use crate::hooks::{PhaseContext, PhaseHook};

use super::action::HandoffAction;
use super::state::ActiveAgentKey;

pub(crate) struct HandoffSyncHook;

#[async_trait::async_trait]
impl PhaseHook for HandoffSyncHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<crate::state::StateCommand, StateError> {
        let handoff = ctx.state::<ActiveAgentKey>().cloned().unwrap_or_default();
        let current_active = ctx.state::<ActiveAgentIdKey>().cloned().unwrap_or(None);
        let mut cmd = crate::state::StateCommand::new();

        if let Some(requested) = handoff.requested_agent {
            if handoff.active_agent.as_deref() != Some(requested.as_str()) {
                cmd.update::<ActiveAgentKey>(HandoffAction::Activate {
                    agent: requested.clone(),
                });
            }
            if current_active.as_deref() != Some(requested.as_str()) {
                cmd.update::<ActiveAgentIdKey>(Some(requested));
            }
            return Ok(cmd);
        }

        if current_active != handoff.active_agent {
            cmd.update::<ActiveAgentIdKey>(handoff.active_agent);
        }
        Ok(cmd)
    }
}
