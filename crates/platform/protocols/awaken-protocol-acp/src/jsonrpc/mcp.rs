// @formal entrypoint acp.session_load_preserves_the_exact_mcp_projection
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
    context: Option<&awaken_acp_contract::McpExecutionContext>,
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
                    let mut headers: Vec<HttpHeader> = auth
                        .into_iter()
                        .filter(|(name, _)| {
                            ![
                                "x-awaken-run-id",
                                "x-awaken-agent-id",
                                "x-awaken-session-id",
                            ]
                            .iter()
                            .any(|reserved| name.eq_ignore_ascii_case(reserved))
                        })
                        .map(|(name, value)| HttpHeader::new(name.clone(), value.clone()))
                        .collect();
                    if let Some(context) =
                        context.filter(|_| awaken_mcp_wire::http::execution_context_enabled(url))
                    {
                        headers.push(HttpHeader::new("x-awaken-run-id", context.run_id.clone()));
                        headers.push(HttpHeader::new(
                            "x-awaken-agent-id",
                            context.agent_id.clone(),
                        ));
                    }
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

#[cfg(test)]
mod execution_header_tests {
    use super::*;

    #[test]
    fn reserved_auth_never_supplies_run_agent_or_session_identity() {
        for opted in [true, false] {
            for current in [true, false] {
                for name in [
                    "X-Awaken-Run-Id",
                    "X-Awaken-Agent-Id",
                    "X-Awaken-Session-Id",
                    "Authorization",
                ] {
                    let servers = [crate::SessionMcpServer {
                        name: "application".into(),
                        command: None,
                        args: Vec::new(),
                        url: Some(
                            if opted {
                                "http://127.0.0.1/mcp?awaken_execution_context=run-agent-v1"
                            } else {
                                "http://127.0.0.1/mcp"
                            }
                            .into(),
                        ),
                        auth: Some((name.into(), "fixture-auth".into())),
                    }];
                    let context = awaken_acp_contract::McpExecutionContext {
                        run_id: "live-run".into(),
                        agent_id: "live-agent".into(),
                    };
                    let value = serde_json::to_value(to_acp_mcp_servers(
                        &servers,
                        current.then_some(&context),
                    ))
                    .unwrap();
                    let actual = value[0]
                        .get("headers")
                        .and_then(serde_json::Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let mut expected = Vec::new();
                    if name == "Authorization" {
                        expected.push(
                            serde_json::json!({"name":"Authorization","value":"fixture-auth"}),
                        );
                    }
                    if opted && current {
                        expected
                            .push(serde_json::json!({"name":"x-awaken-run-id","value":"live-run"}));
                        expected.push(
                            serde_json::json!({"name":"x-awaken-agent-id","value":"live-agent"}),
                        );
                    }
                    assert_eq!(
                        actual, expected,
                        "opted={opted} current={current} auth={name}"
                    );
                }
            }
        }
    }
}

#[cfg(kani)]
/// @obligation acp.session_load_preserves_the_exact_mcp_projection
/// @kani features=real-acp
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
