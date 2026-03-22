//! Permission plugin: registers state keys and a tool permission checker.

use async_trait::async_trait;

use awaken_contract::StateError;
use awaken_runtime::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use awaken_runtime::runtime::{PhaseContext, ToolPermission, ToolPermissionChecker};
use awaken_runtime::state::{KeyScope, StateKeyOptions};

use crate::rules::{ToolPermissionBehavior, evaluate_tool_permission};
use crate::state::{PermissionOverridesKey, PermissionPolicyKey, permission_rules_from_state};

/// Stable plugin name for the permission extension.
pub const PERMISSION_PLUGIN_NAME: &str = "ext-permission";

/// Permission extension plugin.
///
/// Registers:
/// - [`PermissionPolicyKey`]: thread-scoped persisted permission rules
/// - [`PermissionOverridesKey`]: run-scoped temporary overrides
/// - A [`ToolPermissionChecker`] that evaluates rules against tool calls
pub struct PermissionPlugin;

impl Plugin for PermissionPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: PERMISSION_PLUGIN_NAME,
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_key::<PermissionPolicyKey>(StateKeyOptions {
            persistent: true,
            retain_on_uninstall: false,
            scope: KeyScope::Thread,
        })?;

        registrar.register_key::<PermissionOverridesKey>(StateKeyOptions {
            persistent: false,
            retain_on_uninstall: false,
            scope: KeyScope::Run,
        })?;

        registrar.register_tool_permission(PERMISSION_PLUGIN_NAME, PermissionChecker)?;

        Ok(())
    }
}

/// Tool permission checker that evaluates the permission ruleset.
struct PermissionChecker;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_descriptor() {
        let plugin = PermissionPlugin;
        assert_eq!(plugin.descriptor().name, PERMISSION_PLUGIN_NAME);
    }
}
