//! Built-in tool permission plugins.

use async_trait::async_trait;

use crate::phase::{PhaseContext, ToolPermission, ToolPermissionChecker};
use crate::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use awaken_contract::StateError;

/// Default plugin that allows all tool calls.
///
/// Install this to get a permissive baseline. Remove or replace it
/// to require explicit approval for tool calls.
pub struct AllowAllToolsPlugin;

impl Plugin for AllowAllToolsPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: "tool-permission:allow-all",
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_tool_permission("tool-permission:allow-all", AllowAllChecker)
    }
}

struct AllowAllChecker;

#[async_trait]
impl ToolPermissionChecker for AllowAllChecker {
    async fn check(&self, _ctx: &PhaseContext) -> Result<ToolPermission, StateError> {
        Ok(ToolPermission::Allow)
    }
}
