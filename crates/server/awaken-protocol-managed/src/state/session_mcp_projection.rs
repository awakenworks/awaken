//! Managed wire projection helpers for the Session Agent's accepted MCP config.

pub(super) fn typed_mcp_servers(
    values: Vec<awaken_session_contract::VisibleMcpServer>,
) -> Vec<crate::types::agent::AgentMcpServer> {
    values
        .into_iter()
        .filter_map(|server| match server.target {
            awaken_session_contract::McpTarget::Http(target) => {
                Some(crate::types::agent::AgentMcpServer {
                    name: server.name,
                    url: target.url,
                })
            }
            awaken_session_contract::McpTarget::SandboxStdio(_) => None,
        })
        .collect()
}

pub(super) fn profile_mcp_servers(
    values: &[awaken_executable_agent_contract::ExecutableAgentMcpServer],
) -> Vec<crate::types::agent::AgentMcpServer> {
    typed_mcp_servers(
        values
            .iter()
            .map(|server| awaken_session_contract::VisibleMcpServer {
                name: server.name.clone(),
                target: server.target.clone(),
                prompts_as_skills: server.prompts_as_skills,
            })
            .collect(),
    )
}
