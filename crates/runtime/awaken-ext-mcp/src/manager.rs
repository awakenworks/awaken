//! Multi-server MCP manager.
//!
//! Aggregates a set of named [`McpServer`]s: it hands the host one
//! [`McpPlugin`] per server to register on the runtime, and reports a health
//! snapshot per server (tool version + liveness). It owns the servers, so their
//! background refresh tasks live as long as the manager.
//!
//! Automatic health-budget reconnect (re-spawning a dead server) is a further
//! enhancement; today a dead server is reported through [`ServerStatus::alive`]
//! and the host decides whether to replace it.

use crate::error::McpError;
use crate::plugin::{McpPlugin, McpServer};

/// A health snapshot of one managed server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStatus {
    pub name: String,
    /// The server's tool-registry version (advances on `tools/list_changed`).
    pub tool_version: u64,
    /// Whether the underlying transport is still usable.
    pub alive: bool,
}

/// Manages a set of named MCP servers.
#[derive(Default)]
pub struct McpManager {
    servers: Vec<McpServer>,
}

impl McpManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a connected server. Fails on a duplicate name.
    pub fn add(&mut self, server: McpServer) -> Result<(), McpError> {
        if self.servers.iter().any(|s| s.name() == server.name()) {
            return Err(McpError::DuplicateServerName(server.name().to_string()));
        }
        self.servers.push(server);
        Ok(())
    }

    /// One plugin per managed server, for registration on the runtime.
    pub fn plugins(&self) -> Vec<McpPlugin> {
        self.servers.iter().map(McpServer::plugin).collect()
    }

    /// A health snapshot per server.
    pub fn status(&self) -> Vec<ServerStatus> {
        self.servers
            .iter()
            .map(|s| ServerStatus {
                name: s.name().to_string(),
                tool_version: s.version(),
                alive: s.is_alive(),
            })
            .collect()
    }

    /// The names of every managed server.
    pub fn server_names(&self) -> Vec<String> {
        self.servers.iter().map(|s| s.name().to_string()).collect()
    }

    pub fn len(&self) -> usize {
        self.servers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::McpToolTransport;
    use async_trait::async_trait;
    use awaken_runtime_contract::plugin::Plugin;
    use mcp::transport::McpTransportError;
    use mcp::{CallToolResult, McpToolDefinition, ToolContent};
    use serde_json::Value;
    use std::sync::Arc;
    use tokio::sync::broadcast;

    struct FakeTransport {
        tool: String,
        alive: bool,
    }

    #[async_trait]
    impl McpToolTransport for FakeTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            Ok(vec![
                serde_json::from_value(serde_json::json!({ "name": self.tool })).unwrap(),
            ])
        }
        async fn call_tool(
            &self,
            _tool_name: &str,
            _arguments: Value,
        ) -> Result<CallToolResult, McpTransportError> {
            Ok(CallToolResult {
                content: vec![ToolContent::Text {
                    text: "ok".to_string(),
                    annotations: None,
                    meta: None,
                }],
                structured_content: None,
                is_error: Some(false),
            })
        }
        fn is_alive(&self) -> bool {
            self.alive
        }
    }

    async fn server(name: &str, tool: &str, alive: bool) -> McpServer {
        let transport = Arc::new(FakeTransport {
            tool: tool.to_string(),
            alive,
        });
        let (_tx, rx) = broadcast::channel(4);
        McpServer::start(name, transport, rx).await.expect("starts")
    }

    #[tokio::test]
    async fn aggregates_plugins_and_status() {
        let mut manager = McpManager::new();
        manager.add(server("a", "one", true).await).expect("adds a");
        manager
            .add(server("b", "two", false).await)
            .expect("adds b");

        assert_eq!(manager.len(), 2);
        assert_eq!(manager.plugins().len(), 2);
        assert_eq!(manager.server_names(), vec!["a", "b"]);

        let status = manager.status();
        assert_eq!(status[0].name, "a");
        assert!(status[0].alive);
        assert_eq!(status[0].tool_version, 1);
        assert_eq!(status[1].name, "b");
        assert!(!status[1].alive, "a dead transport reports not-alive");

        // Each plugin projects its own server's tool.
        let ids: Vec<_> = manager
            .plugins()
            .iter()
            .map(|p| p.resolve().dynamic_tools[0].descriptor.id.clone())
            .collect();
        assert_eq!(ids, vec!["mcp__a__one", "mcp__b__two"]);
    }

    #[tokio::test]
    async fn a_duplicate_server_name_is_rejected() {
        let mut manager = McpManager::new();
        manager.add(server("dup", "x", true).await).expect("first");
        let err = manager.add(server("dup", "y", true).await);
        assert!(matches!(err, Err(McpError::DuplicateServerName(_))));
    }
}
