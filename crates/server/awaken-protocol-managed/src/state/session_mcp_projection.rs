//! Managed wire projection helpers for visible Session MCP servers.

pub(super) fn typed_mcp_servers(
    values: Vec<awaken_session_contract::VisibleMcpServer>,
) -> Vec<crate::types::agent::AgentMcpServer> {
    values
        .into_iter()
        .map(|server| match server.target {
            awaken_session_contract::McpTarget::Http(target) => {
                crate::types::agent::AgentMcpServer::Url {
                    name: server.name,
                    url: target.url,
                    prompts_as_skills: server.prompts_as_skills,
                }
            }
            awaken_session_contract::McpTarget::SandboxStdio(target) => {
                crate::types::agent::AgentMcpServer::SandboxStdio {
                    name: server.name,
                    command: target.command,
                    args: target.args,
                    prompts_as_skills: server.prompts_as_skills,
                }
            }
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
