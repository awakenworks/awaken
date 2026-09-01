//! Pure projections from frozen Session inputs into runtime-owned values.

use super::*;

pub(super) fn pre_authorized_tool_ids(
    mcp_ids: &[String],
    admin_ids: &[String],
    has_authored_permission: bool,
) -> Vec<String> {
    if has_authored_permission {
        admin_ids.to_vec()
    } else {
        mcp_ids
            .iter()
            .cloned()
            .chain(admin_ids.iter().cloned())
            .collect()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::host) enum ManagedCoordinationRole {
    Primary,
    Child,
}

fn is_replaced_managed_coordination_descriptor(
    descriptor: &awaken_runtime_contract::resolved::ToolDescriptor,
) -> bool {
    descriptor.kind == awaken_runtime_contract::resolved::ToolKind::AgentDelegation
        || descriptor.id == awaken_ext_builtin_tools::AGENT_RUN
        || descriptor.id == awaken_ext_builtin_tools::LIST_AGENTS
        || descriptor.id == awaken_ext_builtin_tools::SEND_MESSAGE
}

/// Project the one-level Managed coordination surface on a per-Session clone.
/// Primary and child projections share one removal predicate; only the primary
/// receives the fixed coordination descriptors, while a child also loses its
/// Advisor/delegate capability. The immutable publication remains unchanged.
pub(in crate::host) fn project_managed_coordination_surface(
    config: &mut awaken_runtime_contract::ExecutableAgentSnapshot,
    role: ManagedCoordinationRole,
) -> bool {
    let before = config.resolved_spec.tool_descriptors.clone();
    let before_delegates = config.resolved_spec.plugin_config.agent.delegates.clone();
    let before_advisor = config.resolved_spec.plugin_config.agent.advisor.clone();
    if role == ManagedCoordinationRole::Child {
        config.resolved_spec.plugin_config.agent.delegates.clear();
        config.resolved_spec.plugin_config.agent.advisor = None;
    }
    config.resolved_spec.tool_descriptors.retain(|descriptor| {
        !is_replaced_managed_coordination_descriptor(descriptor)
            && (role == ManagedCoordinationRole::Primary
                || descriptor.kind != awaken_runtime_contract::resolved::ToolKind::Advisor)
    });
    if role == ManagedCoordinationRole::Primary {
        config.resolved_spec.tool_descriptors.extend(
            awaken_ext_builtin_tools::builtin_tools()
                .into_iter()
                .filter(|tool| tool.toolset() == awaken_ext_builtin_tools::Toolset::Coordination)
                .map(awaken_ext_builtin_tools::BuiltinTool::into_descriptor),
        );
    }
    config.resolved_spec.tool_descriptors != before
        || config.resolved_spec.plugin_config.agent.delegates != before_delegates
        || config.resolved_spec.plugin_config.agent.advisor != before_advisor
}

pub(super) fn merge_acp_mcp_servers(
    publication: Vec<awaken_runtime_contract::resolved::AcpMcpServer>,
    staged: impl IntoIterator<Item = awaken_run_executor_acp::SessionMcpServer>,
) -> Result<Vec<awaken_run_executor_acp::SessionMcpServer>, HostError> {
    let publication = publication
        .into_iter()
        .map(|server| match server.transport {
            awaken_runtime_contract::resolved::AcpMcpTransport::Stdio { command, args } => {
                awaken_run_executor_acp::SessionMcpServer {
                    name: server.name,
                    command: Some(command),
                    args,
                    url: None,
                    auth: None,
                }
            }
            awaken_runtime_contract::resolved::AcpMcpTransport::Http { url } => {
                awaken_run_executor_acp::SessionMcpServer {
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
    existing: Vec<awaken_run_executor_acp::SessionMcpServer>,
    staged: impl IntoIterator<Item = awaken_run_executor_acp::SessionMcpServer>,
) -> Result<Vec<awaken_run_executor_acp::SessionMcpServer>, HostError> {
    let mut names = std::collections::BTreeSet::new();
    let mut merged = Vec::new();
    for server in existing.into_iter().chain(staged) {
        if !names.insert(server.name.clone()) {
            return Err(HostError::bad_request(format!(
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

    fn staged_stdio(name: &str) -> awaken_run_executor_acp::SessionMcpServer {
        awaken_run_executor_acp::SessionMcpServer {
            name: name.into(),
            command: Some("playwright-mcp".into()),
            args: vec!["--headless".into()],
            url: None,
            auth: None,
        }
    }

    #[test]
    fn authored_permission_is_the_only_mcp_confirmation_authority() {
        let mcp = vec!["mcp__calc__add".to_string()];
        let admin = vec!["awaken_admin_get".to_string()];

        assert_eq!(
            pre_authorized_tool_ids(&mcp, &admin, false),
            vec!["mcp__calc__add", "awaken_admin_get"]
        );
        assert_eq!(
            pre_authorized_tool_ids(&mcp, &admin, true),
            vec!["awaken_admin_get"]
        );
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
