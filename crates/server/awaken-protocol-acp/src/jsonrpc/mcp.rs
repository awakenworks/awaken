/// Map neutral Session MCP servers onto the ACP `session/new` wire. HTTP
/// transports carry mediated auth as headers; stdio transports carry it as env.
pub(super) fn to_acp_mcp_servers(
    servers: &[crate::SessionMcpServer],
) -> Vec<agent_client_protocol::McpServer> {
    use agent_client_protocol::{
        EnvVariable, HttpHeader, McpServer, McpServerHttp, McpServerStdio,
    };
    servers
        .iter()
        .map(|server| match &server.url {
            Some(url) => {
                let headers = server
                    .auth
                    .iter()
                    .map(|(name, value)| HttpHeader::new(name.clone(), value.clone()))
                    .collect();
                McpServer::Http(
                    McpServerHttp::new(server.name.clone(), url.clone()).headers(headers),
                )
            }
            None => {
                let env = server
                    .auth
                    .iter()
                    .map(|(name, value)| EnvVariable::new(name.clone(), value.clone()))
                    .collect();
                McpServer::Stdio(
                    McpServerStdio::new(
                        server.name.clone(),
                        server.command.clone().unwrap_or_default(),
                    )
                    .args(server.args.clone())
                    .env(env),
                )
            }
        })
        .collect()
}
