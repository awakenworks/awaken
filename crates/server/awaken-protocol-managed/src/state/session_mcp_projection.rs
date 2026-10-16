//! Managed wire projection helpers for the Session Agent's accepted MCP config.

pub(super) fn typed_mcp_servers(
    values: Vec<awaken_session_configuration_contract::VisibleMcpServer>,
) -> Vec<crate::types::agent::AgentMcpServer> {
    values
        .into_iter()
        .filter_map(|server| match server.target {
            awaken_agent_contract::McpTarget::Http(target) => {
                Some(crate::types::agent::AgentMcpServer {
                    name: server.name,
                    url: target.url,
                })
            }
            awaken_agent_contract::McpTarget::SandboxStdio(_) => None,
        })
        .collect()
}
