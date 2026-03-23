use awaken_contract::StateError;
use awaken_runtime::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use awaken_runtime::state::{KeyScope, StateKeyOptions};

use crate::state::{PermissionOverridesKey, PermissionPolicyKey};

use super::checker::PermissionChecker;

/// Stable plugin name for the permission extension.
pub const PERMISSION_PLUGIN_NAME: &str = "ext-permission";

/// Permission extension plugin.
///
/// Registers:
/// - [`PermissionPolicyKey`]: thread-scoped persisted permission rules
/// - [`PermissionOverridesKey`]: run-scoped temporary overrides
/// - A [`awaken_runtime::runtime::ToolPermissionChecker`] that evaluates rules against tool calls
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
