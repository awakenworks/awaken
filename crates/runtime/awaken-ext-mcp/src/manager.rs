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

    /// Add a connected server. Fails closed on a duplicate raw name, and keeps
    /// the namespace check as a structural backstop: the id encoding is
    /// injective, so distinct raw names already occupy distinct namespaces, and
    /// any future collision would mean the encoding itself regressed.
    pub fn add(&mut self, server: McpServer) -> Result<(), McpError> {
        if self.servers.iter().any(|s| s.name() == server.name()) {
            return Err(McpError::DuplicateServerName(server.name().to_string()));
        }
        if let Some(existing) = self
            .servers
            .iter()
            .find(|s| s.namespace() == server.namespace())
        {
            return Err(McpError::NamespaceCollision {
                name: server.name().to_string(),
                namespace: server.namespace().to_string(),
                existing: existing.name().to_string(),
            });
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
    use awaken_mcp_wire::McpTransportError;
    use awaken_mcp_wire::{CallToolResult, McpToolDefinition, ToolContent};
    use awaken_runtime_contract::plugin::Plugin;
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
            .map(|p| p.resolve().dynamic_tools[0].descriptor().id.clone())
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

    #[tokio::test]
    async fn distinct_raw_server_names_keep_distinct_namespaces() {
        // "a-b" and "a.b" are distinct RAW names. Under the historical encoding
        // both collapsed to `mcp__a_b__`, so one shadowed the other's tool ids
        // and a policy authored for one governed the other. The encoding is now
        // injective, so both register and route unambiguously.
        let mut manager = McpManager::new();
        manager
            .add(server("a-b", "t", true).await)
            .expect("adds a-b");
        manager
            .add(server("a.b", "t", true).await)
            .expect("adds a.b");

        assert_eq!(manager.len(), 2);
        let mut ids: Vec<_> = manager
            .plugins()
            .iter()
            .map(|p| p.resolve().dynamic_tools[0].descriptor().id.clone())
            .collect();
        ids.sort();
        assert_eq!(
            ids,
            vec!["mcp__a-2db__t".to_string(), "mcp__a-2eb__t".to_string()]
        );
    }

    #[tokio::test]
    async fn the_namespace_backstop_still_fails_closed() {
        // The encoding makes this unreachable through `add`'s public inputs, so
        // the backstop is exercised directly: were injectivity ever to regress,
        // the second server must be rejected, never silently registered.
        let mut manager = McpManager::new();
        manager
            .add(server("shared", "t", true).await)
            .expect("first");
        let mut clashing = server("other", "t", true).await;
        clashing.set_namespace_for_test("mcp__shared__".into());
        let err = manager.add(clashing);
        assert!(
            matches!(
                &err,
                Err(McpError::NamespaceCollision { name, namespace, existing })
                    if name == "other" && namespace == "mcp__shared__" && existing == "shared"
            ),
            "collision must fail closed, got {err:?}"
        );
        assert_eq!(manager.len(), 1);
    }
}
