//! McpPlugin: integrates MCP tool registry with awaken's Plugin system.

use awaken_contract::StateError;
use awaken_runtime::plugins::{Plugin, PluginDescriptor, PluginRegistrar};

/// Plugin that registers MCP-related state and capabilities with the awaken runtime.
///
/// The actual tool discovery and registration is handled by
/// [`McpToolRegistryManager`](crate::manager::McpToolRegistryManager); this plugin
/// provides the integration point for the awaken plugin lifecycle.
pub struct McpPlugin;

impl Plugin for McpPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor { name: "mcp" }
    }

    fn register(&self, _registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        // MCP tools are registered dynamically via McpToolRegistryManager,
        // not through the static plugin registration path.
        // This plugin exists as a namespace marker for the MCP extension.
        Ok(())
    }
}
