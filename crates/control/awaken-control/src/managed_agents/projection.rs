//! Managed Agent wire/domain projections and boundary validation.

use super::*;

pub(super) fn register_roster_name(
    seen: &mut BTreeMap<String, String>,
    agent_id: &str,
    name: &str,
) -> Result<(), ManagedAgentError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ManagedAgentError::Invalid(format!(
            "multiagent Agent `{agent_id}` has an empty callable name"
        )));
    }
    let normalized = name.to_lowercase();
    if matches!(normalized.as_str(), "self" | "anthropic.advisor") {
        return Err(ManagedAgentError::Invalid(format!(
            "multiagent Agent `{agent_id}` uses reserved callable name `{name}`"
        )));
    }
    if let Some(existing) = seen.insert(normalized, agent_id.to_string()) {
        return Err(ManagedAgentError::Invalid(format!(
            "multiagent Agents `{existing}` and `{agent_id}` have the same callable name `{name}`"
        )));
    }
    Ok(())
}

pub(super) fn client_tools(tools: &[AgentTool]) -> Vec<ToolDescriptor> {
    tools
        .iter()
        .filter_map(|tool| match tool {
            AgentTool::Custom {
                name,
                description,
                input_schema,
            } => Some(ToolDescriptor::client_executed(
                name,
                description,
                serde_json::to_value(input_schema).expect("typed custom-tool schema serializes"),
            )),
            AgentTool::AgentToolset20260401 { .. } | AgentTool::McpToolset { .. } => None,
        })
        .collect()
}

pub(super) fn typed_mcp_servers(values: Vec<AgentMcpServer>) -> Vec<AgentMcpServerBinding> {
    values
        .into_iter()
        .map(|server| AgentMcpServerBinding {
            name: server.name,
            transport: awaken_runtime_contract::agent_bindings::AgentMcpTransportBinding::http(
                server.url,
            ),
            prompts_as_skills: false,
            credential: None,
        })
        .collect()
}

pub(super) fn typed_skills(values: Vec<AgentSkill>) -> Vec<AgentSkillBinding> {
    values.into_iter().map(AgentSkill::into_binding).collect()
}

pub(super) fn typed_multiagent(value: WireMultiagent) -> MultiagentConfig {
    let WireMultiagent::Coordinator { agents } = value;
    MultiagentConfig {
        agents: agents
            .into_iter()
            .map(|entry| match entry {
                MultiagentRosterEntry::Id(id) => MultiagentTarget::Agent { id, version: None },
                MultiagentRosterEntry::Reference(reference) => MultiagentTarget::Agent {
                    id: reference.id,
                    version: reference.version,
                },
                MultiagentRosterEntry::SelfReference(_) => MultiagentTarget::SelfReference,
                MultiagentRosterEntry::Advisor(advisor) => MultiagentTarget::Advisor {
                    model: advisor.model,
                },
            })
            .collect(),
    }
}

pub(super) fn config_from_create(
    id: String,
    params: AgentCreateParams,
) -> Result<AgentConfig, ManagedAgentError> {
    validate_agent_tools(&params.tools).map_err(ManagedAgentError::Invalid)?;
    let model = params.model.into_config().into_resolved();
    let inference = model.inference_options();
    let model_binding = parse_managed_model_id(&model.id)
        .map_err(|error| ManagedAgentError::Invalid(error.to_string()))?;
    let multiagent = params.multiagent.map(typed_multiagent);
    let config = AgentConfig {
        id,
        instructions: params.system.unwrap_or_default(),
        max_steps: awaken_runtime_contract::DEFAULT_MAX_STEPS,
        delegation_limits: Default::default(),
        model_binding,
        inference,
        tool_ids: Vec::new(),
        toolsets: toolset_policies(&params.tools),
        client_tools: client_tools(&params.tools),
        plugin_ids: Vec::new(),
        plugin_config: BTreeMap::new(),
        context_policy: Default::default(),
        tool_patterns: Vec::new(),
        model_fallbacks: Vec::new(),
        name: Some(params.name),
        description: params.description,
        metadata: params.metadata,
        mcp_servers: typed_mcp_servers(params.mcp_servers),
        skills: typed_skills(params.skills),
        multiagent,
        disabled_at: None,
        archived_at: None,
        tool_overrides: Vec::new(),
        tool_exposure: Default::default(),
        tool_discovery: Default::default(),
        recovery_policies: BTreeMap::new(),
        compaction: None,
    };
    validate_managed_agent_config(&config)?;
    Ok(config)
}

pub(super) fn validate_managed_agent_config(config: &AgentConfig) -> Result<(), ManagedAgentError> {
    let name_chars = config.name.as_deref().unwrap_or_default().chars().count();
    if !(1..=256).contains(&name_chars) {
        return Err(ManagedAgentError::Invalid(
            "name must be 1-256 characters".into(),
        ));
    }
    if config
        .description
        .as_ref()
        .is_some_and(|description| description.chars().count() > 2_048)
    {
        return Err(ManagedAgentError::Invalid(
            "description supports at most 2048 characters".into(),
        ));
    }
    if config.instructions.chars().count() > 100_000 {
        return Err(ManagedAgentError::Invalid(
            "system supports at most 100000 characters".into(),
        ));
    }
    if config.metadata.len() > 16
        || config
            .metadata
            .iter()
            .any(|(key, value)| key.chars().count() > 64 || value.chars().count() > 512)
    {
        return Err(ManagedAgentError::Invalid(
            "metadata supports at most 16 pairs with 64-character keys and 512-character values"
                .into(),
        ));
    }
    if config.skills.len() > 20 {
        return Err(ManagedAgentError::Invalid(
            "skills supports at most 20 entries".into(),
        ));
    }
    if config.max_steps == 0 {
        return Err(ManagedAgentError::Invalid(
            "max_steps must be greater than or equal to 1".into(),
        ));
    }
    config
        .validate_managed_tool_bindings()
        .map_err(ManagedAgentError::Invalid)?;
    // `validate_agent_tools` already closed incoming Managed wire membership.
    // Runtime-only Agent overrides may still be present because the update owner
    // preserved them from the versioned current config; they are not wire-authored.
    awaken_agent_contract::validate_agent_skills(&config.skills)
        .map_err(ManagedAgentError::Invalid)?;
    if let Some(multiagent) = &config.multiagent {
        multiagent
            .validate(&config.id)
            .map_err(ManagedAgentError::Invalid)?;
    }
    Ok(())
}

pub(super) fn acp_configuration_to_preserve(
    config: &AgentConfig,
    incoming_model_id: &str,
) -> Option<awaken_runtime_contract::resolved::AcpSessionConfiguration> {
    let same_model = render_managed_model_id(&config.model_binding)
        .is_ok_and(|current| current == incoming_model_id);
    (same_model && matches!(config.kind(), AgentKind::Acp(_)))
        .then(|| config.model_binding.acp_configuration().cloned())
        .flatten()
}

pub(super) fn wire_tools(
    toolsets: &[ToolsetPolicy],
    client_tools: &[ToolDescriptor],
) -> Vec<AgentTool> {
    // Cause graph: server tool id -> runtime-only capability (no Managed wire
    // representation); toolset -> typed server capability; client descriptor ->
    // complete custom definition. Projecting a server id as `custom` would change
    // execution ownership and create a second, incomplete representation.
    //
    // Decision table:
    // | domain source          | Managed `tools` projection |
    // | server `tool_ids`      | omitted                    |
    // | typed toolset          | typed toolset              |
    // | client tool descriptor | complete `custom`          |
    let mut tools = resolved_toolsets(toolsets);
    tools.extend(client_tools.iter().map(|tool| {
        AgentTool::Custom {
            name: tool.id.clone(),
            description: tool.description.clone(),
            input_schema: CustomToolInputSchema::from_value(tool.model_parameters())
                .expect("published client-tool schemas were validated at admission"),
        }
    }));
    tools
}

pub(super) fn project(revision: AgentConfigRevision) -> Agent {
    let revision_number = revision.revision;
    let fallback_timestamp = lifecycle_timestamp();
    let created_at = revision
        .created_at_unix_ms
        .map(awaken_session_contract::epoch_millis_to_rfc3339)
        .unwrap_or_else(|| fallback_timestamp.clone());
    let updated_at = revision
        .updated_at_unix_ms
        .map(awaken_session_contract::epoch_millis_to_rfc3339)
        .unwrap_or(fallback_timestamp);
    let config = revision.config;
    let id = config.id.clone();
    let model = render_managed_model_id(&config.model_binding).unwrap_or_default();
    let tools = wire_tools(&config.toolsets, &config.client_tools);
    Agent {
        id: id.clone(),
        object_type: "agent",
        archived_at: config.archived_at,
        created_at,
        updated_at,
        name: config.name.unwrap_or_else(|| id.clone()),
        description: config.description,
        model: awaken_protocol_managed::types::ModelConfig::from_inference(model, config.inference),
        system: (!config.instructions.is_empty()).then_some(config.instructions),
        metadata: config.metadata,
        mcp_servers: config
            .mcp_servers
            .into_iter()
            .filter_map(|server| match server.transport {
                awaken_runtime_contract::agent_bindings::AgentMcpTransportBinding::Http(
                    transport,
                ) => Some(awaken_protocol_managed::types::agent::AgentMcpServer {
                    name: server.name,
                    url: transport.url,
                }),
                awaken_runtime_contract::agent_bindings::AgentMcpTransportBinding::SandboxStdio(
                    _,
                ) => None,
            })
            .collect(),
        skills: config
            .skills
            .into_iter()
            .map(AgentSkill::from_binding)
            .collect(),
        tools,
        multiagent: config.multiagent.map(|value| {
            let mut agents = value
                .agents
                .into_iter()
                .map(|target| match target {
                    MultiagentTarget::Agent { id, version } => {
                        MultiagentRosterEntry::Reference(
                            awaken_protocol_managed::types::agent::AgentRosterReference {
                                id,
                                kind: awaken_protocol_managed::types::agent::AgentRosterReferenceKind::Agent,
                                version: Some(version.unwrap_or(1)),
                            },
                        )
                    }
                    MultiagentTarget::SelfReference => MultiagentRosterEntry::Reference(
                        awaken_protocol_managed::types::agent::AgentRosterReference {
                            id: id.clone(),
                            kind: awaken_protocol_managed::types::agent::AgentRosterReferenceKind::Agent,
                            version: Some(revision_number),
                        },
                    ),
                    MultiagentTarget::Advisor { model } => MultiagentRosterEntry::Advisor(
                        AdvisorRosterEntry {
                            model,
                            kind: AdvisorRosterEntryKind::Advisor,
                        },
                    ),
                })
                .collect::<Vec<_>>();
            // The authoring order remains ConfigPlane truth. Managed wire has one
            // additional representation rule: the optional advisor is always the
            // final roster entry while ordinary/self references keep their stable
            // relative order.
            agents.sort_by_key(|entry| matches!(entry, MultiagentRosterEntry::Advisor(_)));
            WireMultiagent::Coordinator { agents }
        }),
        version: revision.revision,
    }
}
