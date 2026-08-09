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
