//! Pure projections from frozen Session inputs into runtime-owned values.

use super::*;

/// The tool ids that bypass the authored policy and are allowed outright.
///
/// Only the read-only management tools (ADR-0052) qualify: the reserved-scope
/// assistant names them and they grant perception, not authority. MCP tool ids
/// are deliberately absent. An MCP tool's authorization comes from its
/// `mcp_toolset` policy, whose default is ask — the same default the Managed
/// Agents wire publishes — so an undeclared MCP server can never be allowed by
/// the mere absence of an authored policy.
pub(super) fn pre_authorized_tool_ids(admin_ids: &[String]) -> Vec<String> {
    admin_ids.to_vec()
}

pub(super) fn merge_acp_mcp_servers(
    publication: Vec<awaken_runtime_contract::resolved::AcpMcpServer>,
    staged: impl IntoIterator<Item = awaken_acp_contract::SessionMcpServer>,
) -> Result<Vec<awaken_acp_contract::SessionMcpServer>, SessionExecutionError> {
    let publication = publication
        .into_iter()
        .map(|server| match server.transport {
            awaken_runtime_contract::resolved::AcpMcpTransport::Stdio { command, args } => {
                awaken_acp_contract::SessionMcpServer {
                    name: server.name,
                    command: Some(command),
                    args,
                    url: None,
                    auth: None,
                }
            }
            awaken_runtime_contract::resolved::AcpMcpTransport::Http { url } => {
                awaken_acp_contract::SessionMcpServer {
                    name: server.name,
                    command: None,
                    args: Vec::new(),
                    url: Some(url),
                    auth: None,
                }
            }
        });
    merge_process_local_mcp_servers(publication.collect(), staged)
}

pub(super) fn merge_process_local_mcp_servers(
    existing: Vec<awaken_acp_contract::SessionMcpServer>,
    staged: impl IntoIterator<Item = awaken_acp_contract::SessionMcpServer>,
) -> Result<Vec<awaken_acp_contract::SessionMcpServer>, SessionExecutionError> {
    let mut names = std::collections::BTreeSet::new();
    let mut merged = Vec::new();
    for server in existing.into_iter().chain(staged) {
        if !names.insert(server.name.clone()) {
            return Err(SessionExecutionError::bad_request(format!(
                "duplicate ACP MCP server name `{}`",
                server.name
            )));
        }
        merged.push(server);
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::{merge_acp_mcp_servers, pre_authorized_tool_ids};
    use awaken_runtime_contract::resolved::{AcpMcpServer, AcpMcpTransport};

    fn published_stdio(name: &str) -> AcpMcpServer {
        AcpMcpServer {
            name: name.into(),
            transport: AcpMcpTransport::Stdio {
                command: "playwright-mcp".into(),
                args: vec!["--headless".into()],
            },
        }
    }

    fn staged_stdio(name: &str) -> awaken_acp_contract::SessionMcpServer {
        awaken_acp_contract::SessionMcpServer {
            name: name.into(),
            command: Some("playwright-mcp".into()),
            args: vec!["--headless".into()],
            url: None,
            auth: None,
        }
    }

    #[test]
    fn only_read_only_management_tools_are_pre_authorized() {
        // Cause/effect graph: C1 read-only management ids are present; C2 an MCP
        // tool id is present. E1 management ids bypass the authored policy; E2 an
        // MCP id never does, whatever else is configured, so its authorization
        // stays with its `mcp_toolset` policy (default ask, Managed-wire parity).
        //
        // | Rule | C1 | C2 | Effect |
        // | R1 | T | T | E1 only |
        // | R2 | F | T | empty — the MCP id is not lent authority |
        let admin = vec!["awaken_admin_get".to_string()];
        assert_eq!(
            pre_authorized_tool_ids(&admin),
            vec!["awaken_admin_get"],
            "R1"
        );
        assert!(pre_authorized_tool_ids(&[]).is_empty(), "R2");
    }

    #[test]
    fn publication_stdio_and_session_mcp_routes_merge_without_shadowing() {
        let merged = merge_acp_mcp_servers(
            vec![published_stdio("playwright")],
            [staged_stdio("session")],
        )
        .expect("distinct routes");
        assert_eq!(
            merged
                .iter()
                .map(|server| server.name.as_str())
                .collect::<Vec<_>>(),
            ["playwright", "session"]
        );

        let error = merge_acp_mcp_servers(
            vec![published_stdio("playwright")],
            [staged_stdio("playwright")],
        )
        .expect_err("duplicate names must not silently shadow a publication route");
        assert!(
            error
                .to_string()
                .contains("duplicate ACP MCP server name `playwright`")
        );
    }
}
