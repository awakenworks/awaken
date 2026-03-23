use async_trait::async_trait;

use awaken_contract::StateError;
use awaken_runtime::phase::{PhaseContext, ToolPermission, ToolPermissionChecker};

use crate::rules::{ToolPermissionBehavior, evaluate_tool_permission};
use crate::state::{PermissionOverridesKey, PermissionPolicyKey, permission_rules_from_state};

/// Tool permission checker that evaluates the permission ruleset.
pub(super) struct PermissionChecker;

#[async_trait]
impl ToolPermissionChecker for PermissionChecker {
    async fn check(&self, ctx: &PhaseContext) -> Result<ToolPermission, StateError> {
        let tool_name = match &ctx.tool_name {
            Some(name) => name.as_str(),
            None => return Ok(ToolPermission::Abstain),
        };
        let tool_args = ctx.tool_args.clone().unwrap_or_default();

        let policy = ctx.state::<PermissionPolicyKey>();
        let overrides = ctx.state::<PermissionOverridesKey>();

        let ruleset = permission_rules_from_state(policy, overrides);
        let evaluation = evaluate_tool_permission(&ruleset, tool_name, &tool_args);

        match evaluation.behavior {
            ToolPermissionBehavior::Allow => Ok(ToolPermission::Allow),
            ToolPermissionBehavior::Deny => Ok(ToolPermission::Deny {
                reason: format!("Tool '{}' is denied by permission rules", tool_name),
                message: None,
            }),
            ToolPermissionBehavior::Ask => Ok(ToolPermission::Abstain),
        }
    }
}
