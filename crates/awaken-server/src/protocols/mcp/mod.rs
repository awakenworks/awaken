//! MCP (Model Context Protocol) server integration.
//!
//! Exposes Awaken agents as MCP tools so that external MCP clients
//! (Claude Desktop, Cursor, etc.) can discover and invoke them.

pub mod adapter;
pub mod http;
pub mod stdio;

use std::sync::Arc;

use mcp::server::{McpServer, McpServerChannels, McpServerConfig};

use awaken_runtime::AgentRuntime;

use self::adapter::AgentMcpTool;

/// Build an [`McpServerConfig`] that exposes all known agents as MCP tools.
///
/// Each agent returned by [`awaken_runtime::AgentResolver::agent_ids`] becomes one MCP tool
/// whose name is the agent ID and whose single parameter is `message`.
///
/// If `outbound_tx` is provided, each tool will send progress/log notifications
/// through it during execution.
pub fn build_mcp_server_config(
    runtime: &Arc<AgentRuntime>,
    outbound_tx: Option<tokio::sync::mpsc::Sender<mcp::protocol::ServerOutbound>>,
) -> McpServerConfig {
    let resolver = runtime.resolver();
    let agent_ids = resolver.agent_ids();

    let mut builder = McpServerConfig::builder()
        .name("awaken-mcp")
        .version(env!("CARGO_PKG_VERSION"))
        .with_logging();

    for agent_id in &agent_ids {
        let description = match resolver.resolve(agent_id) {
            Ok(agent) => {
                // Use first 200 chars of system prompt as description.
                let prompt = &agent.spec.system_prompt;
                if prompt.len() > 200 {
                    format!("{}…", &prompt[..200])
                } else if prompt.is_empty() {
                    format!("Awaken agent: {agent_id}")
                } else {
                    prompt.clone()
                }
            }
            Err(_) => format!("Awaken agent: {agent_id}"),
        };

        let mut tool = AgentMcpTool::new(agent_id.clone(), description, Arc::clone(runtime));
        if let Some(ref tx) = outbound_tx {
            tool = tool.with_outbound(tx.clone());
        }
        builder = builder.with_tool(tool);
    }

    builder.build()
}

/// Create and start an MCP server backed by all known Awaken agents.
///
/// Returns the server handle and channel pair for wiring to a transport
/// (HTTP or Stdio). Tools will send progress/log notifications through the
/// server's outbound channel.
pub fn create_mcp_server(runtime: &Arc<AgentRuntime>) -> (Arc<McpServer>, McpServerChannels) {
    // First pass: create config without outbound_tx to get the channels.
    // We need the outbound_tx from the server, but the server needs the config.
    // Solution: create a forwarding channel.
    let (notify_tx, mut notify_rx) =
        tokio::sync::mpsc::channel::<mcp::protocol::ServerOutbound>(256);

    let config = build_mcp_server_config(runtime, Some(notify_tx));
    let (server, channels) = McpServer::new(config);

    // Spawn a task that forwards tool notifications to the server's outbound channel.
    let outbound_tx = channels.outbound_tx.clone();
    tokio::spawn(async move {
        while let Some(msg) = notify_rx.recv().await {
            if outbound_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    (server, channels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime::{AgentResolver, ResolvedAgent, RuntimeError};

    struct MultiAgentResolver;
    impl AgentResolver for MultiAgentResolver {
        fn resolve(&self, agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
            Err(RuntimeError::AgentNotFound {
                agent_id: agent_id.to_string(),
            })
        }
        fn agent_ids(&self) -> Vec<String> {
            vec!["agent-a".into(), "agent-b".into()]
        }
    }

    struct EmptyResolver;
    impl AgentResolver for EmptyResolver {
        fn resolve(&self, agent_id: &str) -> Result<ResolvedAgent, RuntimeError> {
            Err(RuntimeError::AgentNotFound {
                agent_id: agent_id.to_string(),
            })
        }
        fn agent_ids(&self) -> Vec<String> {
            Vec::new()
        }
    }

    #[test]
    fn config_registers_all_agents_as_tools() {
        let runtime = Arc::new(AgentRuntime::new(Arc::new(MultiAgentResolver)));
        let config = build_mcp_server_config(&runtime, None);
        assert_eq!(config.name(), "awaken-mcp");
        let defs = config.registry().definitions();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"agent-a"));
        assert!(names.contains(&"agent-b"));
        assert_eq!(defs.len(), 2);
    }

    #[test]
    fn config_with_no_agents_has_empty_registry() {
        let runtime = Arc::new(AgentRuntime::new(Arc::new(EmptyResolver)));
        let config = build_mcp_server_config(&runtime, None);
        assert_eq!(config.registry().len(), 0);
    }

    #[tokio::test]
    async fn server_responds_to_initialize() {
        let runtime = Arc::new(AgentRuntime::new(Arc::new(MultiAgentResolver)));
        let (_server, mut channels) = create_mcp_server(&runtime);

        let request = mcp::JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: mcp::JsonRpcId::Number(1),
            method: "initialize".to_string(),
            params: None,
        };

        channels
            .inbound_tx
            .send(mcp::protocol::ClientInbound::Request(request))
            .await
            .unwrap();

        let outbound = channels.outbound_rx.recv().await.unwrap();
        match outbound {
            mcp::protocol::ServerOutbound::Response(resp) => {
                assert!(resp.is_success());
                let result = resp.result().unwrap();
                assert!(result.get("protocolVersion").is_some());
            }
            _ => panic!("expected Response"),
        }
    }

    #[tokio::test]
    async fn server_lists_agent_tools() {
        let runtime = Arc::new(AgentRuntime::new(Arc::new(MultiAgentResolver)));
        let (_server, mut channels) = create_mcp_server(&runtime);

        let request = mcp::JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: mcp::JsonRpcId::Number(2),
            method: "tools/list".to_string(),
            params: None,
        };

        channels
            .inbound_tx
            .send(mcp::protocol::ClientInbound::Request(request))
            .await
            .unwrap();

        let outbound = channels.outbound_rx.recv().await.unwrap();
        match outbound {
            mcp::protocol::ServerOutbound::Response(resp) => {
                assert!(resp.is_success());
                let result = resp.result().unwrap();
                let tools = result["tools"].as_array().unwrap();
                assert_eq!(tools.len(), 2);
            }
            _ => panic!("expected Response"),
        }
    }
}
