//! Connect to an MCP server and project its tools into runtime tools.
//!
//! [`connect_tools`] discovers a server's tools once and returns an
//! [`McpConnection`] carrying both the model-visible [`ToolDescriptor`]s and the
//! executable [`RawTool`]s. A host adds the descriptors to its run config and
//! registers the tools with the runtime — the same shape as the built-in hand
//! tools. Dynamic re-discovery on `tools/list_changed` is a later phase.

use std::sync::Arc;

use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::RawTool;

use crate::error::McpError;
use crate::tool::{McpRawTool, mcp_tool_descriptor};
use crate::transport::McpToolTransport;

/// The tools a single MCP server contributes, ready for a host to wire in.
pub struct McpConnection {
    /// The server name used to namespace every tool id (`mcp__<server>__…`).
    pub server_name: String,
    /// Model-visible descriptors, one per discovered tool.
    pub descriptors: Vec<ToolDescriptor>,
    /// Executable raw tools, one per discovered tool, keyed by the same ids.
    pub tools: Vec<Arc<dyn RawTool>>,
}

/// Discover `transport`'s tools and project them into an [`McpConnection`]. The
/// `transport` is shared by every produced tool, so calls route back to the one
/// connection.
pub async fn connect_tools(
    server_name: &str,
    transport: Arc<dyn McpToolTransport>,
) -> Result<McpConnection, McpError> {
    if server_name.trim().is_empty() {
        return Err(McpError::EmptyServerName);
    }
    let defs = transport.list_tools().await?;
    let mut descriptors = Vec::with_capacity(defs.len());
    let mut tools: Vec<Arc<dyn RawTool>> = Vec::with_capacity(defs.len());
    for def in &defs {
        descriptors.push(mcp_tool_descriptor(server_name, def)?);
        tools.push(Arc::new(McpRawTool::new(
            server_name,
            &def.name,
            Arc::clone(&transport),
        )?));
    }
    Ok(McpConnection {
        server_name: server_name.to_string(),
        descriptors,
        tools,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_mcp_wire::McpTransportError;
    use awaken_mcp_wire::{CallToolResult, McpToolDefinition, ToolContent};
    use serde_json::Value;

    struct FakeTransport {
        tools: Vec<McpToolDefinition>,
    }

    #[async_trait]
    impl McpToolTransport for FakeTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            Ok(self.tools.clone())
        }
        async fn call_tool(
            &self,
            tool_name: &str,
            _arguments: Value,
        ) -> Result<CallToolResult, McpTransportError> {
            Ok(CallToolResult {
                content: vec![ToolContent::Text {
                    text: format!("called {tool_name}"),
                    annotations: None,
                    meta: None,
                }],
                structured_content: None,
                is_error: Some(false),
            })
        }
    }

    fn tool_def(name: &str) -> McpToolDefinition {
        serde_json::from_value(serde_json::json!({ "name": name })).expect("valid tool definition")
    }

    #[tokio::test]
    async fn connect_projects_every_tool_with_a_namespaced_id() {
        let transport = Arc::new(FakeTransport {
            tools: vec![tool_def("alpha"), tool_def("beta")],
        });
        let conn = connect_tools("srv", transport).await.expect("connects");
        assert_eq!(conn.server_name, "srv");
        assert_eq!(conn.descriptors.len(), 2);
        assert_eq!(conn.tools.len(), 2);
        let ids: Vec<_> = conn.descriptors.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, vec!["mcp__srv__alpha", "mcp__srv__beta"]);
        assert_eq!(conn.tools[0].id(), "mcp__srv__alpha");
        assert_eq!(conn.tools[1].id(), "mcp__srv__beta");
    }

    #[tokio::test]
    async fn empty_server_name_is_rejected() {
        let transport = Arc::new(FakeTransport { tools: vec![] });
        match connect_tools("  ", transport).await {
            Err(McpError::EmptyServerName) => {}
            other => panic!("expected EmptyServerName, got {:?}", other.err()),
        }
    }

    #[tokio::test]
    async fn a_no_tool_server_yields_an_empty_connection() {
        let transport = Arc::new(FakeTransport { tools: vec![] });
        let conn = connect_tools("srv", transport).await.expect("connects");
        assert!(conn.descriptors.is_empty());
        assert!(conn.tools.is_empty());
    }
}
