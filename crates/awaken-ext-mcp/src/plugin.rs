//! McpPlugin: integrates MCP tool registry with awaken's Plugin system.

use awaken_contract::StateError;
use awaken_runtime::plugins::{Plugin, PluginDescriptor, PluginRegistrar};

use crate::manager::McpToolRegistry;

/// Plugin that registers MCP tools with the awaken runtime via the Plugin lifecycle.
///
/// Takes a snapshot of the [`McpToolRegistry`] at `register()` time and registers
/// each discovered MCP tool through the [`PluginRegistrar`].
///
/// **Known limitation**: tools are snapshotted once during `register()`. The
/// underlying manager registry can refresh in response to periodic refresh or
/// `notifications/tools/list_changed`, but an already-built awaken runtime
/// keeps its registered tool set until the next resolve/register cycle.
pub struct McpPlugin {
    registry: McpToolRegistry,
}

impl McpPlugin {
    /// Create a new `McpPlugin` backed by the given MCP tool registry.
    pub fn new(registry: McpToolRegistry) -> Self {
        Self { registry }
    }
}

impl Plugin for McpPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor { name: "mcp" }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        let snapshot = self.registry.snapshot();
        for (id, tool) in snapshot {
            registrar.register_tool(id, tool)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpServerConnectionConfig;
    use crate::manager::McpToolRegistryManager;
    use crate::progress::McpProgressUpdate;
    use crate::transport::McpToolTransport;
    use async_trait::async_trait;
    use mcp::transport::{McpTransportError, ServerCapabilities, TransportTypeId};
    use mcp::{CallToolResult, McpToolDefinition};
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    #[derive(Debug, Default)]
    struct MockTransport {
        tools: Vec<McpToolDefinition>,
    }

    impl MockTransport {
        fn with_tools(tools: Vec<McpToolDefinition>) -> Self {
            Self { tools }
        }
    }

    #[derive(Debug, Default)]
    struct MutableMockTransport {
        tools: Arc<Mutex<Vec<McpToolDefinition>>>,
    }

    impl MutableMockTransport {
        fn with_tools(tools: Vec<McpToolDefinition>) -> Self {
            Self {
                tools: Arc::new(Mutex::new(tools)),
            }
        }

        fn set_tools(&self, tools: Vec<McpToolDefinition>) {
            *self.tools.lock().unwrap() = tools;
        }
    }

    #[async_trait]
    impl McpToolTransport for MockTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            Ok(self.tools.clone())
        }

        async fn call_tool(
            &self,
            name: &str,
            _args: Value,
            _progress_tx: Option<mpsc::Sender<McpProgressUpdate>>,
            _context: crate::transport::McpCallContext,
        ) -> Result<CallToolResult, McpTransportError> {
            Ok(CallToolResult {
                content: vec![mcp::ToolContent::Text {
                    text: format!("called {name}"),
                    annotations: None,
                    meta: None,
                }],
                structured_content: None,
                is_error: None,
            })
        }

        fn transport_type(&self) -> TransportTypeId {
            TransportTypeId::Stdio
        }

        async fn server_capabilities(
            &self,
        ) -> Result<Option<ServerCapabilities>, McpTransportError> {
            Ok(None)
        }
    }

    #[async_trait]
    impl McpToolTransport for MutableMockTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            Ok(self.tools.lock().unwrap().clone())
        }

        async fn call_tool(
            &self,
            name: &str,
            _args: Value,
            _progress_tx: Option<mpsc::Sender<McpProgressUpdate>>,
            _context: crate::transport::McpCallContext,
        ) -> Result<CallToolResult, McpTransportError> {
            Ok(CallToolResult {
                content: vec![mcp::ToolContent::Text {
                    text: format!("called {name}"),
                    annotations: None,
                    meta: None,
                }],
                structured_content: None,
                is_error: None,
            })
        }

        fn transport_type(&self) -> TransportTypeId {
            TransportTypeId::Stdio
        }
    }

    fn cfg(name: &str) -> McpServerConnectionConfig {
        McpServerConnectionConfig::stdio(name, "echo", vec!["ok".to_string()])
    }

    async fn make_manager_with(
        entries: Vec<(&str, Vec<McpToolDefinition>)>,
    ) -> McpToolRegistryManager {
        let transports: Vec<(McpServerConnectionConfig, Arc<dyn McpToolTransport>)> = entries
            .into_iter()
            .map(|(name, tools)| {
                (
                    cfg(name),
                    Arc::new(MockTransport::with_tools(tools)) as Arc<dyn McpToolTransport>,
                )
            })
            .collect();
        McpToolRegistryManager::from_transports(transports)
            .await
            .unwrap()
    }

    fn tool_def(name: &str) -> McpToolDefinition {
        McpToolDefinition {
            name: name.to_string(),
            title: Some(format!("{name} title")),
            description: Some(format!("{name} desc")),
            input_schema: json!({"type": "object"}),
            group: None,
            meta: None,
            icons: None,
            output_schema: None,
            execution: None,
            annotations: None,
        }
    }

    #[tokio::test]
    async fn register_populates_tools_via_registrar() {
        let manager = make_manager_with(vec![("server_a", vec![tool_def("alpha")])]).await;
        let registry = manager.registry();

        let plugin = McpPlugin::new(registry);
        let mut registrar = PluginRegistrar::new_for_test();

        plugin.register(&mut registrar).unwrap();

        let tool_ids = registrar.tool_ids_for_test();
        assert_eq!(tool_ids.len(), 1, "expected 1 tool, got: {tool_ids:?}");
        assert!(
            tool_ids[0].contains("alpha"),
            "tool ID should contain 'alpha', got: {}",
            tool_ids[0]
        );
    }

    #[tokio::test]
    async fn register_with_empty_registry_registers_nothing() {
        let manager = make_manager_with(vec![("empty", vec![])]).await;
        let registry = manager.registry();

        let plugin = McpPlugin::new(registry);
        let mut registrar = PluginRegistrar::new_for_test();

        plugin.register(&mut registrar).unwrap();

        assert!(
            registrar.tool_ids_for_test().is_empty(),
            "registrar should be empty when registry has no tools"
        );
    }

    #[tokio::test]
    async fn register_multiple_tools_from_multiple_servers() {
        let manager = make_manager_with(vec![
            ("server_a", vec![tool_def("tool_one"), tool_def("tool_two")]),
            ("server_b", vec![tool_def("tool_three")]),
        ])
        .await;
        let registry = manager.registry();

        let plugin = McpPlugin::new(registry);
        let mut registrar = PluginRegistrar::new_for_test();

        plugin.register(&mut registrar).unwrap();

        let tool_ids = registrar.tool_ids_for_test();
        assert_eq!(tool_ids.len(), 3, "expected 3 tools, got: {tool_ids:?}");
    }

    #[tokio::test]
    async fn register_uses_current_registry_snapshot_per_resolve_cycle() {
        let transport = Arc::new(MutableMockTransport::with_tools(vec![tool_def("alpha")]));
        let manager = McpToolRegistryManager::from_transports([(
            cfg("server_a"),
            Arc::clone(&transport) as Arc<dyn McpToolTransport>,
        )])
        .await
        .unwrap();
        let plugin = McpPlugin::new(manager.registry());

        let mut first_registrar = PluginRegistrar::new_for_test();
        plugin.register(&mut first_registrar).unwrap();
        assert_eq!(
            first_registrar.tool_ids_for_test(),
            vec!["mcp__server_a__alpha".to_string()]
        );

        transport.set_tools(vec![tool_def("beta")]);
        manager.refresh().await.unwrap();

        assert_eq!(
            first_registrar.tool_ids_for_test(),
            vec!["mcp__server_a__alpha".to_string()],
            "an already-built runtime registrar is not mutated after MCP list_changed/refresh"
        );

        let mut second_registrar = PluginRegistrar::new_for_test();
        plugin.register(&mut second_registrar).unwrap();
        assert_eq!(
            second_registrar.tool_ids_for_test(),
            vec!["mcp__server_a__beta".to_string()],
            "a new resolve/register cycle observes the refreshed MCP registry snapshot"
        );
    }
}
