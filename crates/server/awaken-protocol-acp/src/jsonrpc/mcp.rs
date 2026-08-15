#[derive(Debug, PartialEq, Eq)]
enum ExactMcpProjection<Name, Command, Args, Url, Auth> {
    Http {
        name: Name,
        url: Url,
        auth: Auth,
    },
    Stdio {
        name: Name,
        command: Command,
        args: Args,
        auth: Auth,
    },
}

/// Allocation-free transport selector shared by `session/new` and
/// `session/load`. Presence of the authored URL is the sole discriminator: an
/// HTTP projection cannot inherit stdio command/args, while a stdio projection
/// cannot inherit an HTTP URL. Every field belonging to the selected transport
/// is moved through unchanged.
fn exact_mcp_projection<Name, Command, Args, Url, Auth>(
    name: Name,
    command: Command,
    args: Args,
    url: Option<Url>,
    auth: Auth,
) -> ExactMcpProjection<Name, Command, Args, Url, Auth> {
    match url {
        Some(url) => ExactMcpProjection::Http { name, url, auth },
        None => ExactMcpProjection::Stdio {
            name,
            command,
            args,
            auth,
        },
    }
}

/// Map neutral Session MCP servers onto the ACP `session/new` and
/// `session/load` wire. HTTP transports carry mediated auth as headers; stdio
/// transports carry it as env.
pub(super) fn to_acp_mcp_servers(
    servers: &[crate::SessionMcpServer],
) -> Vec<agent_client_protocol::McpServer> {
    use agent_client_protocol::{
        EnvVariable, HttpHeader, McpServer, McpServerHttp, McpServerStdio,
    };
    servers
        .iter()
        .map(|server| {
            match exact_mcp_projection(
                &server.name,
                server.command.as_ref(),
                server.args.as_slice(),
                server.url.as_ref(),
                server.auth.as_ref(),
            ) {
                ExactMcpProjection::Http { name, url, auth } => {
                    let headers = auth
                        .into_iter()
                        .map(|(name, value)| HttpHeader::new(name.clone(), value.clone()))
                        .collect();
                    McpServer::Http(McpServerHttp::new(name.clone(), url.clone()).headers(headers))
                }
                ExactMcpProjection::Stdio {
                    name,
                    command,
                    args,
                    auth,
                } => {
                    let env = auth
                        .into_iter()
                        .map(|(name, value)| EnvVariable::new(name.clone(), value.clone()))
                        .collect();
                    McpServer::Stdio(
                        McpServerStdio::new(name.clone(), command.cloned().unwrap_or_default())
                            .args(args.to_vec())
                            .env(env),
                    )
                }
            }
        })
        .collect()
}

#[cfg(kani)]
#[kani::proof]
fn acp_session_load_preserves_the_exact_mcp_projection() {
    let name: u8 = kani::any();
    let command_value: u8 = kani::any();
    let args: u8 = kani::any();
    let url_value: u8 = kani::any();
    let auth_name: u8 = kani::any();
    let auth_value: u8 = kani::any();
    let command = kani::any::<bool>().then_some(command_value);
    let url = kani::any::<bool>().then_some(url_value);
    let auth = kani::any::<bool>().then_some((auth_name, auth_value));

    let projected = exact_mcp_projection(name, command, args, url, auth);
    match (url, projected) {
        (
            Some(expected_url),
            ExactMcpProjection::Http {
                name: actual_name,
                url: actual_url,
                auth: actual_auth,
            },
        ) => {
            assert_eq!(actual_name, name);
            assert_eq!(actual_url, expected_url);
            assert_eq!(actual_auth, auth);
        }
        (
            None,
            ExactMcpProjection::Stdio {
                name: actual_name,
                command: actual_command,
                args: actual_args,
                auth: actual_auth,
            },
        ) => {
            assert_eq!(actual_name, name);
            assert_eq!(actual_command, command);
            assert_eq!(actual_args, args);
            assert_eq!(actual_auth, auth);
        }
        (Some(_), ExactMcpProjection::Stdio { .. }) | (None, ExactMcpProjection::Http { .. }) => {
            panic!("MCP transport discriminator was mixed")
        }
    }
}
