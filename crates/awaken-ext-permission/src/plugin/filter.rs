use async_trait::async_trait;

use awaken_contract::StateError;
use awaken_runtime::agent::state::ExcludeTool;
use awaken_runtime::state::StateCommand;
use awaken_runtime::{PhaseContext, PhaseHook};

use crate::state::{PermissionOverridesKey, PermissionPolicyKey, permission_rules_from_state};

/// BeforeInference hook that removes unconditionally denied tools from the
/// tool list before the LLM sees them.
///
/// Only exact-match, argument-independent Deny rules are applied here.
/// Conditional rules remain handled by [`super::checker::PermissionToolGateHook`]
/// at `ToolGate`.
pub(super) struct PermissionToolFilterHook;

#[async_trait]
impl PhaseHook for PermissionToolFilterHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let policy = ctx.state::<PermissionPolicyKey>();
        let overrides = ctx.state::<PermissionOverridesKey>();
        let ruleset = permission_rules_from_state(policy, overrides);

        let denied = ruleset.unconditionally_denied_tools();
        if denied.is_empty() {
            return Ok(StateCommand::new());
        }

        let mut cmd = StateCommand::new();
        for tool_id in denied {
            cmd.schedule_action::<ExcludeTool>(tool_id.to_owned())?;
        }
        Ok(cmd)
    }
}
