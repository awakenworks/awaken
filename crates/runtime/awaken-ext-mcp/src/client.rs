//! Connect to an MCP server and project its tools into runtime tools.
//!
//! [`connect_tools`] discovers a server's tools once and returns an
//! [`McpConnection`] carrying both runtime [`ToolDescriptor`]s and executable
//! [`RawTool`]s. Ordinary descriptors are model-visible; task-capable ones are
//! `DetachedOnly` and therefore selectable only through the runtime's detached
//! wrapper. A host adds the descriptors to its run config and registers the
//! tools with the runtime — the same shape as the built-in hand tools. Dynamic
//! re-discovery on `tools/list_changed` is a later phase.

use std::sync::Arc;

use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_runtime_contract::tool::RawTool;

use crate::error::McpError;
use crate::tool::{McpRawTool, mcp_tool_descriptor_for_transport, validate_tools_task_negotiation};
use crate::transport::McpToolTransport;

/// The tools a single MCP server contributes, ready for a host to wire in.
pub struct McpConnection {
    /// The server name used to namespace every tool id (`mcp__<server>__…`).
    pub server_name: String,
    /// Runtime descriptors, one per discovered tool. The model projection
    /// excludes task-capable `DetachedOnly` entries.
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
    validate_tools_task_negotiation(server_name, &defs, transport.as_ref())?;
    let mut descriptors = Vec::with_capacity(defs.len());
    let mut tools: Vec<Arc<dyn RawTool>> = Vec::with_capacity(defs.len());
    for def in &defs {
        descriptors.push(mcp_tool_descriptor_for_transport(
            server_name,
            def,
            transport.as_ref(),
        )?);
        tools.push(Arc::new(McpRawTool::from_definition(
            server_name,
            def,
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
    use awaken_mcp_wire::{
        CallToolResult, CreateTaskResult, McpTask, McpToolDefinition, TaskStatus, ToolContent,
    };
    use awaken_runtime_contract::resolved::ToolKind;
    use awaken_runtime_contract::tool::{ToolCall, ToolTaskStart};
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

    struct TaskConnectionTransport;

    #[async_trait]
    impl McpToolTransport for TaskConnectionTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            Ok(vec![
                serde_json::from_value(serde_json::json!({
                    "name": "slow",
                    "execution": {"taskSupport": "optional"}
                }))
                .unwrap(),
            ])
        }

        async fn call_tool(
            &self,
            _tool_name: &str,
            _arguments: Value,
        ) -> Result<CallToolResult, McpTransportError> {
            unreachable!("task tool starts through call_tool_as_task")
        }

        fn supports_task_tools_call(&self) -> bool {
            true
        }

        async fn call_tool_as_task(
            &self,
            _tool_name: &str,
            _arguments: Value,
            _ttl_ms: Option<u64>,
        ) -> Result<CreateTaskResult, McpTransportError> {
            Ok(CreateTaskResult {
                task: McpTask {
                    task_id: "remote-connect".into(),
                    status: TaskStatus::Working,
                    status_message: None,
                    created_at: "2026-08-30T00:00:00Z".into(),
                    last_updated_at: "2026-08-30T00:00:01Z".into(),
                    ttl: None,
                    poll_interval: Some(100),
                },
            })
        }
    }

    #[tokio::test]
    async fn connection_projects_one_detached_target_without_task_management_tools() {
        // Cause/effect graph: C1=one optional-task MCP tool; C2=server task-call
        // capability is negotiated. Effects E1=one DetachedOnly descriptor and
        // one executable target share the same id; E2=starting it returns the
        // opaque task handle; E3=no tasks/get/result/cancel descriptor is
        // synthesized. Decision rule C1+C2=>E1+E2+E3.
        let connection = connect_tools("srv", Arc::new(TaskConnectionTransport))
            .await
            .expect("connects");
        assert_eq!(connection.descriptors.len(), 1, "E1/E3");
        assert_eq!(connection.tools.len(), 1, "E1");
        assert_eq!(connection.descriptors[0].kind, ToolKind::DetachedOnly, "E1");
        assert_eq!(connection.descriptors[0].id, "mcp__srv__slow", "E1");
        assert!(
            connection
                .descriptors
                .iter()
                .all(|descriptor| !descriptor.id.contains("tasks/")),
            "E3"
        );
        let started = connection.tools[0]
            .start_task(ToolCall {
                call_id: "call-1".into(),
                tool_id: "mcp__srv__slow".into(),
                arguments: serde_json::json!({}),
            })
            .await
            .expect("E2");
        assert!(matches!(
            started,
            ToolTaskStart::Pending(handle)
                if handle.owner == crate::tool::MCP_TASK_OWNER
                    && handle.task_id == "remote-connect"
        ));
    }
}
