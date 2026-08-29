//! Validation and projection of authored integrations into runtime Agent bindings.

use awaken_runtime_contract::agent_bindings::{AgentBindings, ToolsetPolicy, ToolsetSource};
use awaken_runtime_contract::resolved::{ResolvedModelCandidate, ToolDescriptor};

use super::CompileError;
use crate::config::{AgentConfig, MultiagentTarget};

pub(super) fn normalize(
    config: &AgentConfig,
    toolsets: Vec<ToolsetPolicy>,
    advisor_candidate: Option<ResolvedModelCandidate>,
) -> Result<AgentBindings, CompileError> {
    let invalid = |axis, reason| CompileError::InvalidBinding {
        agent: config.id.clone(),
        axis,
        reason,
    };
    let mut mcp_names = std::collections::BTreeSet::new();
    for (index, server) in config.mcp_servers.iter().enumerate() {
        let name = (!server.name.trim().is_empty())
            .then_some(server.name.trim())
            .ok_or_else(|| invalid("mcp_servers", format!("entry {index} requires `name`")))?;
        server.transport.normalize().map_err(|reason| {
            invalid(
                "mcp_servers",
                format!("entry {index} has an invalid transport: {reason}"),
            )
        })?;
        if matches!(
            &server.transport,
            awaken_runtime_contract::agent_bindings::AgentMcpTransportBinding::SandboxStdio(_)
        ) && server.credential.is_some()
        {
            return Err(invalid(
                "mcp_servers",
                format!(
                    "entry {index} sandbox stdio credentials require a secret-environment binding"
                ),
            ));
        }
        if !mcp_names.insert(name.to_string()) {
            return Err(invalid(
                "mcp_servers",
                format!("server name {name:?} is duplicated"),
            ));
        }
        if let Some(credential) = &server.credential {
            if credential.id.trim().is_empty() {
                return Err(invalid(
                    "mcp_servers",
                    format!("entry {index} credential requires a non-empty `id`"),
                ));
            }
            if credential.revision == 0 {
                return Err(invalid(
                    "mcp_servers",
                    format!("entry {index} credential revision must be positive"),
                ));
            }
        }
    }

    awaken_agent_contract::validate_agent_skills(&config.skills)
        .map_err(|reason| invalid("skills", reason))?;
    if let Some(multiagent) = &config.multiagent {
        multiagent
            .validate(&config.id)
            .map_err(|reason| invalid("multiagent", reason))?;
    }
    Ok(AgentBindings {
        mcp_servers: config.mcp_servers.clone(),
        skills: config.skills.clone(),
        delegates: config
            .multiagent
            .as_ref()
            .map(|multiagent| {
                multiagent
                    .agents
                    .iter()
                    .filter(|target| target.advisor_model().is_none())
                    .map(
                        |target| awaken_runtime_contract::agent_bindings::AgentDelegateBinding {
                            agent_id: awaken_runtime_contract::snapshot::AgentId(
                                target.resolved_id(&config.id).to_string(),
                            ),
                            source_revision: target.version(),
                            recursive_self: target.is_self_reference(),
                        },
                    )
                    .collect()
            })
            .unwrap_or_default(),
        advisor: match config.multiagent.as_ref().and_then(|multiagent| {
            multiagent
                .agents
                .iter()
                .find_map(MultiagentTarget::advisor_model)
        }) {
            Some(model) => Some(
                awaken_runtime_contract::agent_bindings::AgentAdvisorBinding {
                    model: model.to_string(),
                    candidate: advisor_candidate.ok_or_else(|| {
                        invalid(
                            "multiagent",
                            "advisor model has no exact published execution candidate".into(),
                        )
                    })?,
                },
            ),
            None => {
                if advisor_candidate.is_some() {
                    return Err(invalid(
                        "multiagent",
                        "an advisor execution candidate exists without advisor authoring".into(),
                    ));
                }
                None
            }
        },
        toolsets,
    })
}

pub(super) fn resolve_toolsets(
    config: &AgentConfig,
    catalog: &[ToolDescriptor],
) -> Result<Vec<ToolsetPolicy>, CompileError> {
    let mut resolved = config.toolsets.clone();
    let mut seen_sources = std::collections::BTreeSet::new();
    for toolset in &mut resolved {
        let source_key = match &toolset.source {
            ToolsetSource::Agent => "agent".to_string(),
            ToolsetSource::Mcp { server_name } => {
                if !config
                    .mcp_servers
                    .iter()
                    .any(|server| server.name == *server_name)
                {
                    return Err(CompileError::InvalidBinding {
                        agent: config.id.clone(),
                        axis: "tools",
                        reason: format!("MCP toolset references undeclared server {server_name:?}"),
                    });
                }
                format!("mcp:{server_name}")
            }
        };
        if !seen_sources.insert(source_key.clone()) {
            return Err(CompileError::InvalidBinding {
                agent: config.id.clone(),
                axis: "tools",
                reason: format!("toolset source {source_key:?} is duplicated"),
            });
        }
        let mut names = std::collections::BTreeSet::new();
        for entry in &toolset.overrides {
            if entry.name.trim().is_empty() || !names.insert(entry.name.clone()) {
                return Err(CompileError::InvalidBinding {
                    agent: config.id.clone(),
                    axis: "tools",
                    reason: format!("invalid or duplicate tool override {:?}", entry.name),
                });
            }
        }
        if toolset.source == ToolsetSource::Agent {
            for entry in &mut toolset.overrides {
                if !catalog.iter().any(|tool| tool.id == entry.name) {
                    entry.policy.enabled = false;
                }
            }
        }
    }
    Ok(resolved)
}
