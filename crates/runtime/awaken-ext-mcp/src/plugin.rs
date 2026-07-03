//! MCP as a runtime plugin.
//!
//! [`McpServer`] connects to a server, holds its live tool registry, and starts
//! a background task that re-lists tools and bumps the registry version whenever
//! the server fires `tools/list_changed`. [`McpPlugin`] projects that registry
//! into the runtime as dynamic tools: `resolve()` returns the current tool
//! snapshot and `live_version()` returns the registry version, so the kernel
//! re-resolves the tool face at a step boundary after a change (P5b).
//!
//! The plugin depends only on the runtime contract; the kernel is never
//! imported. Its `CapabilityBound` is a namespace (`mcp__{server}__`) since the
//! exact ids are not known at composition time.

use std::sync::{Arc, Mutex};

use awaken_runtime_contract::plugin::{
    CapabilityBound, Contributions, DynamicTool, Plugin, PluginManifest,
};
use mcp::McpToolDefinition;
use tokio::sync::broadcast;

use crate::error::McpError;
use crate::id_mapping::tool_namespace;
use crate::stdio::StdioTransport;
use crate::tool::{McpRawTool, mcp_tool_descriptor};
use crate::transport::{ListChangedKind, McpToolTransport};

/// The plugin id for a server.
fn plugin_id(server_name: &str) -> String {
    format!("mcp:{server_name}")
}

/// A server's live tool registry: the current tool set and a version that
/// advances on every `tools/list_changed`.
struct McpRegistry {
    version: u64,
    tools: Vec<McpToolDefinition>,
}

/// A connected MCP server with a self-refreshing tool registry.
pub struct McpServer {
    server_name: String,
    namespace: String,
    transport: Arc<dyn McpToolTransport>,
    registry: Arc<Mutex<McpRegistry>>,
}

impl McpServer {
    /// Discover `transport`'s tools and start refreshing the registry whenever a
    /// tool `list_changed` arrives on `list_changed`.
    pub async fn start(
        server_name: impl Into<String>,
        transport: Arc<dyn McpToolTransport>,
        mut list_changed: broadcast::Receiver<ListChangedKind>,
    ) -> Result<Self, McpError> {
        let server_name = server_name.into();
        let namespace = tool_namespace(&server_name)?;
        let tools = transport.list_tools().await?;
        let registry = Arc::new(Mutex::new(McpRegistry { version: 1, tools }));

        let refresh_registry = Arc::clone(&registry);
        let refresh_transport = Arc::clone(&transport);
        tokio::spawn(async move {
            while let Ok(kind) = list_changed.recv().await {
                if kind == ListChangedKind::Tools
                    && let Ok(tools) = refresh_transport.list_tools().await
                {
                    let mut registry = refresh_registry.lock().unwrap();
                    registry.tools = tools;
                    registry.version += 1;
                }
            }
        });

        Ok(Self {
            server_name,
            namespace,
            transport,
            registry,
        })
    }

    /// Connect over stdio and wire the server's own `list_changed` stream.
    pub async fn connect_stdio(
        server_name: impl Into<String>,
        transport: StdioTransport,
    ) -> Result<Self, McpError> {
        let list_changed = transport.subscribe_list_changed();
        Self::start(server_name, Arc::new(transport), list_changed).await
    }

    /// Connect over streaming HTTP and wire its `list_changed` stream. The
    /// `transport` must have been built with
    /// [`HttpTransport::connect_streaming`](crate::http::HttpTransport::connect_streaming)
    /// so the background SSE listener is running.
    pub async fn connect_http(
        server_name: impl Into<String>,
        transport: crate::http::HttpTransport,
    ) -> Result<Self, McpError> {
        let list_changed = transport.subscribe_list_changed();
        Self::start(server_name, Arc::new(transport), list_changed).await
    }

    /// The runtime plugin projecting this server's tools.
    pub fn plugin(&self) -> McpPlugin {
        McpPlugin {
            server_name: self.server_name.clone(),
            namespace: self.namespace.clone(),
            transport: Arc::clone(&self.transport),
            registry: Arc::clone(&self.registry),
        }
    }

    /// The current registry version (advances on `tools/list_changed`).
    pub fn version(&self) -> u64 {
        self.registry.lock().unwrap().version
    }

    /// The server's name.
    pub fn name(&self) -> &str {
        &self.server_name
    }

    /// Whether the underlying transport is still usable.
    pub fn is_alive(&self) -> bool {
        self.transport.is_alive()
    }
}

/// The plugin projecting an [`McpServer`]'s live tools into the runtime.
pub struct McpPlugin {
    server_name: String,
    namespace: String,
    transport: Arc<dyn McpToolTransport>,
    registry: Arc<Mutex<McpRegistry>>,
}

impl Plugin for McpPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: plugin_id(&self.server_name),
            requires: Vec::new(),
            config_sections: Vec::new(),
            bound: CapabilityBound {
                tool_namespaces: vec![self.namespace.clone()],
                ..Default::default()
            },
        }
    }

    fn resolve(&self) -> Contributions {
        let mut contributions = Contributions::new(plugin_id(&self.server_name));
        let registry = self.registry.lock().unwrap();
        for def in &registry.tools {
            // A tool whose name sanitizes empty is skipped rather than failing
            // the whole resolution.
            if let (Ok(descriptor), Ok(tool)) = (
                mcp_tool_descriptor(&self.server_name, def),
                McpRawTool::new(&self.server_name, &def.name, Arc::clone(&self.transport)),
            ) {
                contributions.dynamic_tools.push(DynamicTool {
                    descriptor,
                    tool: Arc::new(tool),
                });
            }
        }
        contributions
    }

    fn live_version(&self) -> Option<u64> {
        Some(self.registry.lock().unwrap().version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use mcp::transport::McpTransportError;
    use mcp::{CallToolResult, ToolContent};
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A transport whose tool list is swapped on the second `list_tools` call, so
    /// a refresh can be observed.
    struct SwappingTransport {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl McpToolTransport for SwappingTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let name = if n == 0 { "alpha" } else { "beta" };
            Ok(vec![
                serde_json::from_value(serde_json::json!({ "name": name })).unwrap(),
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
    }

    #[tokio::test]
    async fn plugin_projects_the_registry_as_dynamic_tools() {
        let transport = Arc::new(SwappingTransport {
            calls: AtomicUsize::new(0),
        });
        let (_tx, rx) = broadcast::channel(4);
        let server = McpServer::start("srv", transport, rx)
            .await
            .expect("starts");
        let plugin = server.plugin();

        // Manifest declares the server's namespace bound.
        assert_eq!(plugin.manifest().bound.tool_namespaces, vec!["mcp__srv__"]);
        assert_eq!(plugin.live_version(), Some(1));

        let contributions = plugin.resolve();
        assert_eq!(contributions.dynamic_tools.len(), 1);
        assert_eq!(
            contributions.dynamic_tools[0].descriptor.id,
            "mcp__srv__alpha"
        );
    }

    #[tokio::test]
    async fn list_changed_refreshes_the_registry_and_bumps_the_version() {
        let transport = Arc::new(SwappingTransport {
            calls: AtomicUsize::new(0),
        });
        let (tx, rx) = broadcast::channel(4);
        let server = McpServer::start("srv", transport, rx)
            .await
            .expect("starts");
        assert_eq!(server.version(), 1);
        assert_eq!(
            server.plugin().resolve().dynamic_tools[0].descriptor.id,
            "mcp__srv__alpha"
        );

        // Fire a tools/list_changed; the background task re-lists and bumps.
        tx.send(ListChangedKind::Tools).expect("send");
        // Wait for the refresh to land.
        for _ in 0..50 {
            if server.version() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(server.version(), 2, "version advanced on list_changed");
        assert_eq!(
            server.plugin().resolve().dynamic_tools[0].descriptor.id,
            "mcp__srv__beta",
            "the refreshed tool set is projected"
        );
    }
}
