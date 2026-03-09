use std::collections::HashMap;
use std::sync::Arc;

use crate::contracts::runtime::tool_call::Tool;
use crate::composition::ToolRegistry;

pub use tirea_extension_mcp::*;

impl ToolRegistry for tirea_extension_mcp::McpToolRegistry {
    fn len(&self) -> usize {
        tirea_extension_mcp::McpToolRegistry::len(self)
    }

    fn get(&self, id: &str) -> Option<Arc<dyn Tool>> {
        tirea_extension_mcp::McpToolRegistry::get(self, id)
    }

    fn ids(&self) -> Vec<String> {
        tirea_extension_mcp::McpToolRegistry::ids(self)
    }

    fn snapshot(&self) -> HashMap<String, Arc<dyn Tool>> {
        tirea_extension_mcp::McpToolRegistry::snapshot(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use mcp::transport::{McpServerConnectionConfig, McpTransportError, TransportTypeId};
    use mcp::McpToolDefinition;
    use serde_json::Value;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    use crate::composition::AgentDefinition;
    use crate::runtime::AgentOs;

    #[derive(Debug, Clone)]
    struct MutableTransport {
        tools: Arc<Mutex<Vec<McpToolDefinition>>>,
    }

    impl MutableTransport {
        fn new(tools: Vec<McpToolDefinition>) -> Self {
            Self {
                tools: Arc::new(Mutex::new(tools)),
            }
        }

        fn replace(&self, tools: Vec<McpToolDefinition>) {
            match self.tools.lock() {
                Ok(mut guard) => *guard = tools,
                Err(poisoned) => *poisoned.into_inner() = tools,
            }
        }
    }

    #[async_trait]
    impl McpToolTransport for MutableTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            let tools = match self.tools.lock() {
                Ok(guard) => guard.clone(),
                Err(poisoned) => poisoned.into_inner().clone(),
            };
            Ok(tools)
        }

        async fn call_tool(
            &self,
            _name: &str,
            _args: Value,
            _progress_tx: Option<mpsc::UnboundedSender<McpProgressUpdate>>,
        ) -> Result<mcp::CallToolResult, McpTransportError> {
            Ok(mcp::CallToolResult {
                content: vec![mcp::ToolContent::text("ok")],
                structured_content: None,
                is_error: None,
            })
        }

        fn transport_type(&self) -> TransportTypeId {
            TransportTypeId::Stdio
        }
    }

    fn cfg(name: &str) -> McpServerConnectionConfig {
        McpServerConnectionConfig::stdio(name, "unused", vec![])
    }

    #[tokio::test]
    async fn mcp_registry_implements_dynamic_tool_registry() {
        let transport = Arc::new(MutableTransport::new(vec![McpToolDefinition::new("echo")]));
        let manager = McpToolRegistryManager::from_transports([(
            cfg("mcp_s1"),
            transport.clone() as Arc<dyn McpToolTransport>,
        )])
        .await
        .expect("build manager");
        let registry = Arc::new(manager.registry()) as Arc<dyn ToolRegistry>;

        let os = AgentOs::builder()
            .with_agent("assistant", AgentDefinition::new("gpt-4o-mini"))
            .with_tool_registry(registry)
            .build()
            .expect("build agent os");

        let resolved1 = os.resolve("assistant").expect("resolve first snapshot");
        assert!(resolved1.tools.contains_key("mcp__mcp_s1__echo"));
        assert!(!resolved1.tools.contains_key("mcp__mcp_s1__sum"));

        transport.replace(vec![McpToolDefinition::new("sum")]);
        manager.refresh().await.expect("refresh registry");

        let resolved2 = os.resolve("assistant").expect("resolve refreshed snapshot");
        assert!(!resolved2.tools.contains_key("mcp__mcp_s1__echo"));
        assert!(resolved2.tools.contains_key("mcp__mcp_s1__sum"));
    }
}
