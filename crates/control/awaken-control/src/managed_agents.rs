//! ACL between the Managed Agent wire API and the durable configuration plane.
//!
//! This adapter owns only representation mapping and lifecycle orchestration. The
//! ConfigPlane remains the single authoring source of truth; IAM policy stays in
//! the HTTP edge and never enters either repository.

use std::collections::BTreeMap;

use awaken_agent_config::{
    AgentConfig, AgentConfigRevision, AgentKind, AgentLifecycle, ConfigWrite, ModelSelection,
    MultiagentConfig, MultiagentTarget,
};
use awaken_agent_contract::AgentSkillBinding;
use awaken_config_service::{
    ConfigPlane, RESERVED_ADMIN_SCOPE, parse_managed_model_id, render_managed_model_id,
};
use awaken_protocol_managed::types::agent::{
    AdvisorRosterEntry, AdvisorRosterEntryKind, Agent, AgentCreateParams, AgentListParams,
    AgentMcpServer, AgentSkill, AgentUpdateParams, MultiagentConfig as WireMultiagent,
    MultiagentRosterEntry,
};
use awaken_protocol_managed::{ManagedAgentError, ManagedAgentRepository};
use awaken_runtime_contract::agent_bindings::AgentMcpServerBinding;
use awaken_runtime_contract::agent_bindings::{ToolsetPolicy, ToolsetSource};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_session_contract::{
    AgentTool, CustomToolInputSchema, is_agent_toolset_member, resolved_toolsets, toolset_policies,
};
use awaken_tenancy::ScopeId;

mod lifecycle_identity;
use lifecycle_identity::{lifecycle_timestamp, new_agent_id};

const MAX_MCP_SERVER_URL_BYTES: usize = 2048;
pub struct ConfigPlaneManagedAgentRepository {
    plane: ConfigPlane,
    fixed_platform_workspace: Option<String>,
}

impl ConfigPlaneManagedAgentRepository {
    pub fn new(plane: ConfigPlane, platform_workspace: impl Into<String>) -> Self {
        Self {
            plane,
            fixed_platform_workspace: Some(platform_workspace.into()),
        }
    }

    /// Project the reserved Assistant through the authenticated request
    /// Workspace. Hosted Control has no single process-owned tenant Workspace;
    /// its IAM edge supplies the exact scope for every repository call.
    pub fn request_scoped(plane: ConfigPlane) -> Self {
        Self {
            plane,
            fixed_platform_workspace: None,
        }
    }

    fn reserved_visible_in(&self, workspace_id: &str) -> bool {
        self.fixed_platform_workspace
            .as_deref()
            .is_none_or(|fixed| fixed == workspace_id)
    }

    fn scope(workspace_id: &str) -> ScopeId {
        ScopeId::from(workspace_id)
    }

    async fn publication_for_read(
        &self,
        workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Result<Option<awaken_agent_config::StoredPublication>, ManagedAgentError> {
        let direct = self
            .plane
            .publication_at_revision_for_execution_workspace(
                &Self::scope(workspace_id),
                workspace_id,
                agent_id,
                source_revision,
            )
            .await
            .map_err(ManagedAgentError::Storage)?;
        if direct.is_some() || !self.reserved_visible_in(workspace_id) {
            return Ok(direct);
        }
        self.plane
            .publication_at_revision_for_execution_workspace(
                &ScopeId::from(RESERVED_ADMIN_SCOPE),
                workspace_id,
                agent_id,
                source_revision,
            )
            .await
            .map_err(ManagedAgentError::Storage)
    }

    async fn versioned_for_read(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Option<AgentConfigRevision>, ManagedAgentError> {
        if workspace_id == RESERVED_ADMIN_SCOPE {
            return Ok(None);
        }
        let current = self
            .plane
            .get_versioned(&Self::scope(workspace_id), id)
            .await
            .map_err(ManagedAgentError::Storage)?;
        if let Some(current) = current {
            let visible = current.config.lifecycle() != AgentLifecycle::Published
                || self
                    .publication_for_read(workspace_id, id, current.revision)
                    .await?
                    .is_some();
            return Ok(visible.then_some(current));
        }
        if !self.reserved_visible_in(workspace_id) {
            return Ok(None);
        }
        let reserved = self
            .plane
            .get_versioned(&ScopeId::from(RESERVED_ADMIN_SCOPE), id)
            .await
            .map_err(ManagedAgentError::Storage)?;
        let Some(reserved) = reserved else {
            return Ok(None);
        };
        Ok(self
            .publication_for_read(workspace_id, id, reserved.revision)
            .await?
            .is_some()
            .then_some(reserved))
    }

    async fn project_current(
        &self,
        workspace_id: &str,
        mut revision: AgentConfigRevision,
    ) -> Result<Agent, ManagedAgentError> {
        if let Some(publication) = self
            .publication_for_read(workspace_id, &revision.config.id, revision.revision)
            .await?
            && matches!(
                &revision.config.model_binding,
                ModelSelection::Auto | ModelSelection::Profile { .. }
            )
        {
            let binding = publication.snapshot.resolved_spec.model_binding.binding;
            revision.config.model_binding = ModelSelection::Pinned(binding);
        }
        Ok(project(revision))
    }

    async fn publish_strict(&self, scope: &ScopeId, id: &str) -> Result<(), ManagedAgentError> {
        self.plane
            .publish(scope, id)
            .await
            .map(|_| ())
            .map_err(|error| match error {
                awaken_config_service::PublishError::Unresolvable(message)
                | awaken_config_service::PublishError::Compile(message) => {
                    ManagedAgentError::Invalid(message)
                }
                other => ManagedAgentError::Storage(other.to_string()),
            })
    }

    async fn resolve_multiagent_references(
        &self,
        workspace_id: &str,
        config: &mut AgentConfig,
    ) -> Result<(), ManagedAgentError> {
        let coordinator_geo = config.inference.inference_geo;
        let Some(multiagent) = config.multiagent.as_mut() else {
            return Ok(());
        };
        multiagent
            .validate(&config.id)
            .map_err(ManagedAgentError::Invalid)?;
        if let Some(advisor_model) = multiagent
            .agents
            .iter()
            .find_map(MultiagentTarget::advisor_model)
        {
            let executor_model = render_managed_model_id(&config.model_binding)
                .map_err(|error| ManagedAgentError::Invalid(error.to_string()))?;
            if !advisor_pair_supported(&executor_model, advisor_model) {
                return Err(ManagedAgentError::Invalid(format!(
                    "unsupported advisor model pairing: executor `{executor_model}`, advisor `{advisor_model}`"
                )));
            }
        }
        for target in &mut multiagent.agents {
            let MultiagentTarget::Agent { id, version } = target else {
                continue;
            };
            let current = self
                .versioned_for_read(workspace_id, id)
                .await?
                .ok_or_else(|| {
                    ManagedAgentError::Invalid(format!(
                        "multiagent references unknown Agent `{id}`"
                    ))
                })?;
            if current.config.lifecycle() != AgentLifecycle::Published {
                return Err(ManagedAgentError::Invalid(format!(
                    "multiagent Agent `{id}` is disabled or archived"
                )));
            }
            let selected = match *version {
                None => current,
                Some(expected) => self
                    .plane
                    .list_revisions(&Self::scope(workspace_id), id)
                    .await
                    .map_err(ManagedAgentError::Storage)?
                    .into_iter()
                    .find(|revision| revision.revision == expected)
                    .ok_or_else(|| {
                        ManagedAgentError::Invalid(format!(
                            "multiagent Agent `{id}` has no version {expected}"
                        ))
                    })?,
            };
            if selected.config.multiagent.is_some() {
                return Err(ManagedAgentError::Invalid(format!(
                    "multiagent Agent `{id}` is itself a coordinator; delegation depth is limited to one referenced level"
                )));
            }
            if selected.config.inference.inference_geo != coordinator_geo {
                return Err(ManagedAgentError::Invalid(format!(
                    "multiagent inference_geo mismatch: coordinator is {:?}, Agent `{id}` is {:?}",
                    coordinator_geo, selected.config.inference.inference_geo
                )));
            }
            *version = Some(selected.revision);
        }
        Ok(())
    }
}

fn advisor_pair_supported(executor: &str, advisor: &str) -> bool {
    let allowed: &[&str] = match executor {
        "claude-haiku-4-5" | "claude-sonnet-4-6" => &[
            "claude-mythos-5",
            "claude-fable-5",
            "claude-opus-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-sonnet-5",
            "claude-sonnet-4-6",
        ],
        "claude-sonnet-5" => &[
            "claude-mythos-5",
            "claude-fable-5",
            "claude-opus-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-sonnet-5",
        ],
        "claude-opus-4-6" => &[
            "claude-mythos-5",
            "claude-fable-5",
            "claude-opus-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-sonnet-5",
        ],
        "claude-opus-4-7" | "claude-opus-4-8" => &[
            "claude-mythos-5",
            "claude-fable-5",
            "claude-opus-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
        ],
        "claude-opus-5" | "claude-fable-5" | "claude-mythos-5" => {
            &["claude-mythos-5", "claude-fable-5", "claude-opus-5"]
        }
        _ => return false,
    };
    allowed.contains(&advisor)
}

fn client_tools(tools: &[AgentTool]) -> Vec<ToolDescriptor> {
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

fn typed_mcp_servers(values: Vec<AgentMcpServer>) -> Vec<AgentMcpServerBinding> {
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

fn typed_skills(values: Vec<AgentSkill>) -> Vec<AgentSkillBinding> {
    values.into_iter().map(AgentSkill::into_binding).collect()
}

fn typed_multiagent(value: WireMultiagent) -> MultiagentConfig {
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

fn config_from_create(
    id: String,
    params: AgentCreateParams,
) -> Result<AgentConfig, ManagedAgentError> {
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
        recovery_policies: BTreeMap::new(),
        compaction: None,
    };
    validate_managed_agent_config(&config)?;
    Ok(config)
}

fn validate_managed_agent_config(config: &AgentConfig) -> Result<(), ManagedAgentError> {
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
    if config.mcp_servers.len() > 20 {
        return Err(ManagedAgentError::Invalid(
            "mcp_servers supports at most 20 entries".into(),
        ));
    }
    let server_names = config
        .mcp_servers
        .iter()
        .map(|server| server.name.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for server in &config.mcp_servers {
        if !(1..=255).contains(&server.name.chars().count()) {
            return Err(ManagedAgentError::Invalid(
                "mcp_server name must be 1-255 characters".into(),
            ));
        }
        if let awaken_runtime_contract::agent_bindings::AgentMcpTransportBinding::Http(transport) =
            &server.transport
            && transport.url.len() > MAX_MCP_SERVER_URL_BYTES
        {
            return Err(ManagedAgentError::Invalid(format!(
                "mcp_server URL must be at most {MAX_MCP_SERVER_URL_BYTES} bytes"
            )));
        }
        server.transport.normalize().map_err(|error| {
            ManagedAgentError::Invalid(format!("mcp_server transport is invalid: {error}"))
        })?;
    }
    if server_names.len() != config.mcp_servers.len() {
        return Err(ManagedAgentError::Invalid(
            "mcp_servers names must be unique".into(),
        ));
    }
    let mut sources = std::collections::BTreeSet::new();
    let mut referenced_mcp = std::collections::BTreeSet::new();
    for toolset in &config.toolsets {
        let source = match &toolset.source {
            ToolsetSource::Agent => "agent".to_string(),
            ToolsetSource::Mcp { server_name } => {
                if !server_names.contains(server_name.as_str()) {
                    return Err(ManagedAgentError::Invalid(format!(
                        "mcp_toolset references undeclared server `{server_name}`"
                    )));
                }
                referenced_mcp.insert(server_name.as_str());
                format!("mcp:{server_name}")
            }
        };
        if !sources.insert(source.clone()) {
            return Err(ManagedAgentError::Invalid(format!(
                "toolset source `{source}` is duplicated"
            )));
        }
        let mut names = std::collections::BTreeSet::new();
        for entry in &toolset.overrides {
            if entry.name.is_empty() || !names.insert(entry.name.as_str()) {
                return Err(ManagedAgentError::Invalid(format!(
                    "tool config name {:?} is empty or duplicated",
                    entry.name
                )));
            }
            if toolset.source == ToolsetSource::Agent && !is_agent_toolset_member(&entry.name) {
                return Err(ManagedAgentError::Invalid(format!(
                    "unknown agent tool `{}`",
                    entry.name
                )));
            }
        }
    }
    if referenced_mcp != server_names {
        let missing = server_names
            .difference(&referenced_mcp)
            .copied()
            .collect::<Vec<_>>();
        return Err(ManagedAgentError::Invalid(format!(
            "every MCP server must have one mcp_toolset; missing {missing:?}"
        )));
    }
    awaken_agent_contract::validate_agent_skills(&config.skills)
        .map_err(ManagedAgentError::Invalid)?;
    if let Some(multiagent) = &config.multiagent {
        multiagent
            .validate(&config.id)
            .map_err(ManagedAgentError::Invalid)?;
    }
    let declared_count = config.client_tools.len() + config.toolsets.len();
    if declared_count > 128 {
        return Err(ManagedAgentError::Invalid(
            "tools supports at most 128 declared entries".into(),
        ));
    }
    Ok(())
}

fn acp_configuration_to_preserve(
    config: &AgentConfig,
    incoming_model_id: &str,
) -> Option<awaken_runtime_contract::resolved::AcpSessionConfiguration> {
    let same_model = render_managed_model_id(&config.model_binding)
        .is_ok_and(|current| current == incoming_model_id);
    (same_model && matches!(config.kind(), AgentKind::Acp(_)))
        .then(|| config.model_binding.acp_configuration().cloned())
        .flatten()
}

fn wire_tools(toolsets: &[ToolsetPolicy], client_tools: &[ToolDescriptor]) -> Vec<AgentTool> {
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

fn project(revision: AgentConfigRevision) -> Agent {
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
            let agents = value
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
            WireMultiagent::Coordinator { agents }
        }),
        version: revision.revision,
    }
}

#[async_trait::async_trait]
impl ManagedAgentRepository for ConfigPlaneManagedAgentRepository {
    async fn create(
        &self,
        workspace_id: &str,
        params: AgentCreateParams,
    ) -> Result<Agent, ManagedAgentError> {
        if workspace_id == RESERVED_ADMIN_SCOPE {
            return Err(ManagedAgentError::Invalid(
                "reserved configuration scope is not an execution Workspace".into(),
            ));
        }
        let scope = Self::scope(workspace_id);
        let id = new_agent_id(workspace_id);
        let mut config = config_from_create(id.clone(), params)?;
        self.resolve_multiagent_references(workspace_id, &mut config)
            .await?;
        self.plane
            .validate(&scope, &config)
            .await
            .map_err(|issue| {
                ManagedAgentError::Invalid(format!("{}: {}", issue.path, issue.message))
            })?;
        match self
            .plane
            .put_if_revision(&scope, &config, 0)
            .await
            .map_err(ManagedAgentError::Storage)?
        {
            ConfigWrite::Applied { revision } => {
                self.publish_strict(&scope, &id).await?;
                let current = self
                    .versioned_for_read(workspace_id, &id)
                    .await?
                    .ok_or_else(|| {
                        ManagedAgentError::Storage("published Agent is not readable".into())
                    })?;
                let _ = revision;
                self.project_current(workspace_id, current).await
            }
            ConfigWrite::Conflict { .. } => Err(ManagedAgentError::Conflict(
                "generated Agent id already exists".into(),
            )),
        }
    }

    async fn retrieve(
        &self,
        workspace_id: &str,
        id: &str,
        version: Option<u64>,
    ) -> Result<Agent, ManagedAgentError> {
        if let Some(version) = version {
            return self
                .versions(workspace_id, id)
                .await?
                .into_iter()
                .find(|revision| revision.version == version)
                .ok_or(ManagedAgentError::NotFound);
        }
        let revision = self
            .versioned_for_read(workspace_id, id)
            .await?
            .ok_or(ManagedAgentError::NotFound)?;
        self.project_current(workspace_id, revision).await
    }

    async fn list(
        &self,
        workspace_id: &str,
        params: &AgentListParams,
    ) -> Result<Vec<Agent>, ManagedAgentError> {
        if workspace_id == RESERVED_ADMIN_SCOPE {
            return Ok(Vec::new());
        }
        let scope = Self::scope(workspace_id);
        let configs = self
            .plane
            .list(&scope)
            .await
            .map_err(ManagedAgentError::Storage)?;
        let mut agents = Vec::with_capacity(configs.len());
        for config in configs {
            let versioned = self
                .plane
                .get_versioned(&scope, &config.id)
                .await
                .map_err(ManagedAgentError::Storage)?
                .ok_or_else(|| ManagedAgentError::Storage("listed Agent disappeared".into()))?;
            if versioned.config.lifecycle() == AgentLifecycle::Published
                && self
                    .publication_for_read(workspace_id, &config.id, versioned.revision)
                    .await?
                    .is_none()
            {
                continue;
            }
            let agent = self.project_current(workspace_id, versioned).await?;
            if !params.include_archived && agent.archived_at.is_some() {
                continue;
            }
            if params
                .created_at_gte
                .as_deref()
                .is_some_and(|lower| agent.created_at.as_str() < lower)
                || params
                    .created_at_lte
                    .as_deref()
                    .is_some_and(|upper| agent.created_at.as_str() > upper)
            {
                continue;
            }
            agents.push(agent);
        }
        Ok(agents)
    }

    async fn update(
        &self,
        workspace_id: &str,
        id: &str,
        params: AgentUpdateParams,
    ) -> Result<Agent, ManagedAgentError> {
        if workspace_id == RESERVED_ADMIN_SCOPE {
            return Err(ManagedAgentError::NotFound);
        }
        let scope = Self::scope(workspace_id);
        let current = self
            .plane
            .get_versioned(&scope, id)
            .await
            .map_err(ManagedAgentError::Storage)?
            .ok_or(ManagedAgentError::NotFound)?;
        if params.version == Some(0) {
            return Err(ManagedAgentError::Invalid(
                "version must be greater than or equal to 1".into(),
            ));
        }
        if params
            .version
            .is_some_and(|version| current.revision != version)
        {
            return Err(ManagedAgentError::Conflict(format!(
                "version mismatch: expected {}, got {}",
                current.revision,
                params.version.unwrap_or_default()
            )));
        }
        if current.config.lifecycle() != AgentLifecycle::Published {
            return Err(ManagedAgentError::Invalid(
                "disabled or archived Agent cannot be updated".into(),
            ));
        }
        let mut config = current.config.clone();
        if let Some(name) = params.name {
            config.name = Some(name);
        }
        if let Some(model) = params.model {
            let model = model.into_config();
            let current_model = render_managed_model_id(&config.model_binding).ok();
            let preserve_effort =
                current_model.as_deref() == Some(model.id.as_str()) && model.effort.is_none();
            let prior_effort = config.inference.effort;
            let prior_acp = acp_configuration_to_preserve(&config, &model.id);
            let model = model.into_resolved();
            config.inference = model.inference_options();
            if preserve_effort {
                config.inference.effort = prior_effort;
            }
            config.model_binding = parse_managed_model_id(&model.id)
                .map_err(|error| ManagedAgentError::Invalid(error.to_string()))?;
            if let Some(configuration) = prior_acp {
                config
                    .model_binding
                    .set_acp_configuration(configuration)
                    .map_err(|error| ManagedAgentError::Invalid(error.into()))?;
            }
        }
        if let Some(description) = params.description {
            config.description = description;
        }
        if let Some(system) = params.system {
            config.instructions = system.unwrap_or_default();
        }
        if let Some(metadata) = params.metadata {
            match metadata {
                None => config.metadata.clear(),
                Some(patch) => {
                    for (key, value) in patch {
                        match value {
                            Some(value) => {
                                config.metadata.insert(key, value);
                            }
                            None => {
                                config.metadata.remove(&key);
                            }
                        }
                    }
                }
            }
        }
        if let Some(mcp_servers) = params.mcp_servers {
            config.mcp_servers = typed_mcp_servers(mcp_servers.unwrap_or_default());
        }
        if let Some(skills) = params.skills {
            config.skills = typed_skills(skills.unwrap_or_default());
        }
        if let Some(tools) = params.tools {
            let tools = tools.unwrap_or_default();
            config.tool_ids.clear();
            config.toolsets = toolset_policies(&tools);
            config.client_tools = client_tools(&tools);
        }
        if let Some(multiagent) = params.multiagent {
            config.multiagent = multiagent.map(typed_multiagent);
        }
        validate_managed_agent_config(&config)?;
        self.resolve_multiagent_references(workspace_id, &mut config)
            .await?;
        self.plane
            .validate(&scope, &config)
            .await
            .map_err(|issue| {
                ManagedAgentError::Invalid(format!("{}: {}", issue.path, issue.message))
            })?;
        if config == current.config {
            return Ok(project(current));
        }
        match self
            .plane
            .put_if_revision(&scope, &config, current.revision)
            .await
            .map_err(ManagedAgentError::Storage)?
        {
            ConfigWrite::Applied { revision } => {
                self.publish_strict(&scope, id).await?;
                let current = self
                    .versioned_for_read(workspace_id, id)
                    .await?
                    .ok_or_else(|| {
                        ManagedAgentError::Storage("published Agent is not readable".into())
                    })?;
                let _ = revision;
                self.project_current(workspace_id, current).await
            }
            ConfigWrite::Conflict { current_revision } => Err(ManagedAgentError::Conflict(
                format!("Agent changed concurrently (current version: {current_revision:?})"),
            )),
        }
    }

    async fn archive(&self, workspace_id: &str, id: &str) -> Result<Agent, ManagedAgentError> {
        if workspace_id == RESERVED_ADMIN_SCOPE {
            return Err(ManagedAgentError::NotFound);
        }
        let scope = Self::scope(workspace_id);
        let current = self
            .plane
            .get_versioned(&scope, id)
            .await
            .map_err(ManagedAgentError::Storage)?
            .ok_or(ManagedAgentError::NotFound)?;
        if current.config.lifecycle() == AgentLifecycle::Archived {
            return Ok(project(current));
        }
        let mut config = current.config;
        config.disabled_at = None;
        config.archived_at = Some(lifecycle_timestamp());
        match self
            .plane
            .put_if_revision(&scope, &config, current.revision)
            .await
            .map_err(ManagedAgentError::Storage)?
        {
            ConfigWrite::Applied { revision } => {
                self.plane
                    .withdraw(workspace_id, id, revision)
                    .await
                    .map_err(ManagedAgentError::Storage)?;
                self.plane
                    .get_versioned(&scope, id)
                    .await
                    .map_err(ManagedAgentError::Storage)?
                    .map(project)
                    .ok_or_else(|| {
                        ManagedAgentError::Storage("archived Agent is not readable".into())
                    })
            }
            ConfigWrite::Conflict { current_revision } => Err(ManagedAgentError::Conflict(
                format!("Agent changed concurrently (current version: {current_revision:?})"),
            )),
        }
    }

    async fn versions(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<Vec<Agent>, ManagedAgentError> {
        if workspace_id == RESERVED_ADMIN_SCOPE {
            return Err(ManagedAgentError::NotFound);
        }
        let mut revisions = self
            .plane
            .list_revisions(&Self::scope(workspace_id), id)
            .await
            .map_err(ManagedAgentError::Storage)?;
        if revisions.is_empty() && self.reserved_visible_in(workspace_id) {
            revisions = self
                .plane
                .list_revisions(&ScopeId::from(RESERVED_ADMIN_SCOPE), id)
                .await
                .map_err(ManagedAgentError::Storage)?;
        }
        if revisions.is_empty() {
            return Err(ManagedAgentError::NotFound);
        }
        Ok(revisions.into_iter().map(project).collect())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use awaken_agent_config::ModelSelection;
    use awaken_config_service::{
        ConfigService, ModelPublicationResolver, ResolvedPublicationModels, StaticToolCatalog,
    };
    use awaken_config_store::SqliteConfigStore;
    use awaken_executable_agent_catalog::{ExecutableAgentCatalog, LocalExecutableAgentRegistrar};
    use awaken_protocol_managed::types::agent::{AgentCreateParams, AgentUpdateParams, ModelInput};
    use awaken_protocol_managed::types::{
        ModelConfigParams, ModelEffort, ModelEffortInput, ModelInferenceGeo, ModelSpeed,
    };
    use awaken_runtime_contract::agent_bindings::{
        InferenceGeography, InferenceOptions, InferenceSpeed, ReasoningEffort,
    };
    use awaken_runtime_contract::resolved::{
        InferenceEndpoint, InferencePlacement, InferencePlacementMechanism, ModelBinding,
        ResolvedModelCandidate, ToolDescriptor, ToolKind,
    };
    use serde_json::json;

    use super::*;

    struct TestModelResolver;

    struct RejectModelResolver;

    #[async_trait::async_trait]
    impl ModelPublicationResolver for TestModelResolver {
        async fn resolve_models(
            &self,
            workspace: &awaken_tenancy::ScopeId,
            selection: &ModelSelection,
            candidates: &[ModelBinding],
        ) -> Result<ResolvedPublicationModels, awaken_config_service::PublicationResolutionError>
        {
            let primary = selection
                .resolved()
                .cloned()
                .or_else(|| {
                    selection.target().map(|(target, backend_ref)| {
                        ModelBinding::new(
                            target.provider_id.as_deref().unwrap_or_default(),
                            &target.model_id,
                            backend_ref,
                        )
                    })
                })
                .ok_or_else(|| "test requires a pinned model".to_string())?;
            let resolved = |binding: ModelBinding| {
                let model = binding.model_ref.clone();
                ResolvedModelCandidate::provider(
                    binding,
                    "test-provider",
                    format!("test-route:{model}"),
                    workspace.clone(),
                    None,
                    InferenceEndpoint {
                        adapter_kind: "anthropic_messages".into(),
                        api_dialect: "anthropic_messages".into(),
                        base_url: "https://provider.example.test".into(),
                        upstream_model: model,
                        processing_placement: Some(InferencePlacement {
                            geography: InferenceGeography::Us,
                            mechanism: InferencePlacementMechanism::AnthropicRequestBody,
                        }),
                    },
                )
            };
            Ok(ResolvedPublicationModels {
                primary: resolved(primary),
                candidates: candidates.iter().cloned().map(resolved).collect(),
                context_window: None,
                max_output_tokens: None,
            })
        }
    }

    #[async_trait::async_trait]
    impl ModelPublicationResolver for RejectModelResolver {
        async fn resolve_models(
            &self,
            _workspace: &awaken_tenancy::ScopeId,
            _selection: &ModelSelection,
            _candidates: &[ModelBinding],
        ) -> Result<ResolvedPublicationModels, awaken_config_service::PublicationResolutionError>
        {
            Err(awaken_config_service::PublicationResolutionError::MissingPrimary)
        }
    }

    fn create_params(name: &str) -> AgentCreateParams {
        AgentCreateParams {
            name: name.into(),
            model: ModelInput::Id("model-a".into()),
            description: None,
            system: Some("be helpful".into()),
            metadata: BTreeMap::new(),
            mcp_servers: vec![
                serde_json::from_value(json!({
                    "type": "url",
                    "name": "docs",
                    "url": "https://mcp.example.test"
                }))
                .unwrap(),
            ],
            skills: vec![
                serde_json::from_value(json!({
                    "type": "custom",
                    "skill_id": "skill-docs"
                }))
                .unwrap(),
            ],
            tools: vec![
                serde_json::from_value(json!({
                    "type": "mcp_toolset",
                    "mcp_server_name": "docs"
                }))
                .unwrap(),
            ],
            multiagent: None,
        }
    }

    fn update_params(version: u64) -> AgentUpdateParams {
        AgentUpdateParams {
            version: Some(version),
            name: Some("renamed".into()),
            model: None,
            description: None,
            system: None,
            metadata: None,
            mcp_servers: None,
            skills: None,
            tools: None,
            multiagent: None,
        }
    }

    #[test]
    fn managed_mcp_http_url_length_boundary_is_exact() {
        // Cause/effect decision table: U1 a valid absolute HTTP(S) URL of 2048
        // wire bytes is admitted; U2 max+1 is rejected before authoring. The
        // generic MCP identity parser remains the one syntax/normalization owner;
        // this edge owns only the Managed Agent resource bound.
        let prefix = "https://mcp.example.test/";
        let mut at_max = create_params("url-at-max");
        at_max.mcp_servers = vec![
            serde_json::from_value(json!({
                "type": "url",
                "name": "docs",
                "url": format!("{prefix}{}", "x".repeat(2048 - prefix.len()))
            }))
            .unwrap(),
        ];
        assert!(
            config_from_create("agent_at_max".into(), at_max.clone()).is_ok(),
            "U1"
        );

        at_max.mcp_servers[0].url.push('x');
        assert!(
            matches!(
                config_from_create("agent_over_max".into(), at_max),
                Err(ManagedAgentError::Invalid(_))
            ),
            "U2"
        );
    }

    fn plane(path: &str) -> ConfigPlane {
        plane_with_catalog(path).0
    }

    fn plane_with_catalog(path: &str) -> (ConfigPlane, Arc<ExecutableAgentCatalog>) {
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        (
            ConfigPlane::new(
                Arc::new(ConfigService::new(
                    Arc::new(TestModelResolver),
                    Arc::new(LocalExecutableAgentRegistrar::new(catalog.clone())),
                )),
                Arc::new(SqliteConfigStore::open(path).expect("config store")),
                Arc::new(StaticToolCatalog(Vec::new())),
            ),
            catalog,
        )
    }

    fn rejecting_plane(path: &str) -> ConfigPlane {
        ConfigPlane::new(
            Arc::new(ConfigService::new(
                Arc::new(RejectModelResolver),
                Arc::new(LocalExecutableAgentRegistrar::new(Arc::new(
                    ExecutableAgentCatalog::new(),
                ))),
            )),
            Arc::new(SqliteConfigStore::open(path).expect("config store")),
            Arc::new(StaticToolCatalog(Vec::new())),
        )
    }

    #[tokio::test]
    async fn managed_create_fails_fast_before_authoring_persistence() {
        // Causes: C1 valid Managed request; C2 the shared publication resolver
        // rejects its model/executor route during the write-free validation.
        // Effects: E1 create returns Invalid; E2 neither Managed reads nor the
        // authoritative ConfigPlane contain an Agent.
        //
        // Decision table:
        // | rule | request | resolver | Managed result | ConfigPlane write |
        // | F1   | valid   | rejects  | Invalid        | none              |
        let temp = tempfile::tempdir().unwrap();
        let plane = rejecting_plane(temp.path().join("config.sqlite").to_str().unwrap());
        let repository = ConfigPlaneManagedAgentRepository::new(plane.clone(), "workspace-a");
        let error = repository
            .create("workspace-a", create_params("invalid"))
            .await
            .unwrap_err();
        assert!(matches!(error, ManagedAgentError::Invalid(_)), "E1");
        assert!(
            repository
                .list("workspace-a", &AgentListParams::default())
                .await
                .unwrap()
                .is_empty(),
            "E2"
        );
        assert!(
            plane
                .list(&ScopeId::from("workspace-a"))
                .await
                .unwrap()
                .is_empty(),
            "E2: validation must reject before the authoring CAS"
        );
    }

    #[test]
    fn server_tool_id_is_not_retyped_as_a_managed_custom_tool() {
        // Cause graph: config-only server tool id -> executable publication;
        // Managed read projection has no individual server-tool wire variant, so
        // it omits the id. Only a client-owned descriptor may become `custom`.
        //
        // Decision table:
        // | server id | toolset | client descriptor | projected tools |
        // | bash      | no      | no                | empty           |
        let mut params = create_params("server-tool");
        params.mcp_servers.clear();
        params.tools.clear();
        let mut config = config_from_create("agent_server".into(), params).unwrap();
        config.tool_ids.push("bash".into());

        let projected = project(AgentConfigRevision {
            config,
            revision: 1,
            created_at_unix_ms: None,
            updated_at_unix_ms: None,
        });
        assert!(projected.tools.is_empty());
    }

    #[tokio::test]
    async fn managed_agent_archive_retains_immutable_publication() {
        // Cause/effect graph:
        // C1 Published + archive -> E1 current execution becomes unavailable while
        // the immutable publication remains addressable; C2 Archived + archive ->
        // E2 idempotent. Disable remains a native configuration-plane lifecycle
        // operation and is deliberately absent from the Managed SDK repository.
        //
        // Decision table:
        // | rule | current   | command | current executable | exact snapshot | result |
        // | L1   | Published | archive | no                 | yes            | archived |
        // | L2   | Archived  | archive | no                 | yes            | no new revision |
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.sqlite");
        let (plane, catalog) = plane_with_catalog(path.to_str().unwrap());
        let repository = ConfigPlaneManagedAgentRepository::new(plane.clone(), "workspace-a");

        let created = repository
            .create("workspace-a", create_params("assistant"))
            .await
            .unwrap();
        assert_eq!(created.version, 1);
        assert_eq!(created.mcp_servers[0].name, "docs");
        assert!(matches!(
            &created.skills[0],
            AgentSkill::Custom { skill_id, .. } if skill_id == "skill-docs"
        ));
        assert!(catalog.current("workspace-a", &created.id).is_some());
        let fingerprint = catalog
            .current("workspace-a", &created.id)
            .expect("published")
            .snapshot
            .fingerprint
            .0;

        let archived = repository
            .archive("workspace-a", &created.id)
            .await
            .unwrap();
        assert_eq!(archived.version, 2, "L1");
        assert!(archived.archived_at.is_some(), "L1");
        assert!(catalog.current("workspace-a", &created.id).is_none(), "L1");
        assert!(
            plane
                .publication(&ScopeId::from("workspace-a"), &fingerprint)
                .await
                .unwrap()
                .is_some(),
            "L1"
        );
        let archived_again = repository
            .archive("workspace-a", &created.id)
            .await
            .unwrap();
        assert_eq!(archived_again.version, 2, "L2");
    }

    #[tokio::test]
    async fn revisions_survive_restart_and_remain_workspace_fenced() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.sqlite");
        let id = {
            let repository = ConfigPlaneManagedAgentRepository::new(
                plane(path.to_str().unwrap()),
                "workspace-a",
            );
            let created = repository
                .create("workspace-a", create_params("assistant"))
                .await
                .unwrap();
            repository
                .update("workspace-a", &created.id, update_params(created.version))
                .await
                .unwrap();
            created.id
        };

        let repository =
            ConfigPlaneManagedAgentRepository::new(plane(path.to_str().unwrap()), "workspace-a");
        let current = repository.retrieve("workspace-a", &id, None).await.unwrap();
        assert_eq!(current.name, "renamed");
        assert_eq!(current.version, 2);
        let versions = repository.versions("workspace-a", &id).await.unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].name, "assistant");
        assert_eq!(versions[1].name, "renamed");
        assert!(matches!(
            repository.retrieve("workspace-b", &id, None).await,
            Err(ManagedAgentError::NotFound)
        ));
        assert!(matches!(
            repository.versions("workspace-b", &id).await,
            Err(ManagedAgentError::NotFound)
        ));
    }

    #[tokio::test]
    async fn model_controls_survive_revision_restart_and_enter_the_executable_snapshot() {
        // Causal graph:
        // tagged/bare Managed model controls -> typed authoring revision
        // -> publication snapshot -> runtime inference controls.
        //
        // Decision table:
        // | create controls       | persisted response | executable snapshot |
        // | fast + xhigh + us     | exact typed values | exact typed values   |
        // | repository restart    | values preserved   | republished values   |
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.sqlite");
        let id = {
            let (plane, catalog) = plane_with_catalog(path.to_str().unwrap());
            let repository = ConfigPlaneManagedAgentRepository::new(plane.clone(), "workspace-a");
            let mut params = create_params("controlled");
            params.model = ModelInput::Config(ModelConfigParams {
                id: "claude-opus-4-8".into(),
                speed: Some(ModelSpeed::Fast),
                effort: Some(ModelEffortInput::Tagged(ModelEffort::Xhigh)),
                inference_geo: Some(ModelInferenceGeo::Us),
            });
            let created = repository.create("workspace-a", params).await.unwrap();
            assert_eq!(created.model.speed, Some(ModelSpeed::Fast));
            assert_eq!(created.model.effort, Some(ModelEffort::Xhigh));
            let installed = catalog
                .current("workspace-a", &created.id)
                .expect("create publishes an executable revision");
            assert_eq!(
                installed.snapshot.resolved_spec.plugin_config.inference,
                InferenceOptions {
                    speed: Some(InferenceSpeed::Fast),
                    effort: Some(ReasoningEffort::Xhigh),
                    inference_geo: Some(InferenceGeography::Us),
                }
            );
            created.id
        };

        let (plane, catalog) = plane_with_catalog(path.to_str().unwrap());
        let repository = ConfigPlaneManagedAgentRepository::new(plane.clone(), "workspace-a");
        let restored = repository.retrieve("workspace-a", &id, None).await.unwrap();
        assert_eq!(restored.model.speed, Some(ModelSpeed::Fast));
        assert_eq!(restored.model.effort, Some(ModelEffort::Xhigh));
        assert_eq!(restored.model.inference_geo, Some(ModelInferenceGeo::Us));
        plane
            .publish(&ScopeId::from("workspace-a"), &id)
            .await
            .expect("reconciliation republishes the restored authoring revision");
        let installed = catalog
            .current("workspace-a", &id)
            .expect("restart restores the same executable publication");
        assert_eq!(
            installed.snapshot.resolved_spec.plugin_config.inference,
            InferenceOptions {
                speed: Some(InferenceSpeed::Fast),
                effort: Some(ReasoningEffort::Xhigh),
                inference_geo: Some(InferenceGeography::Us),
            }
        );
    }

    #[tokio::test]
    async fn multiagent_geo_is_validated_against_exact_published_roster() {
        // Cause/effect graph: each Agent publication freezes an optional geo;
        // resolving a coordinator roster loads the exact referenced revisions
        // and compares them before the coordinator write/publish boundary.
        //
        // Decision table:
        // | Rule | coordinator | delegate | effect                         |
        // | G1   | us          | us       | create and freeze exact roster |
        // | G2   | global      | us       | 400-equivalent, no Agent       |
        // | G3   | omitted     | us       | 400-equivalent, no Agent       |
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.sqlite");
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        let plane = ConfigPlane::new(
            Arc::new(ConfigService::new(
                Arc::new(TestModelResolver),
                Arc::new(LocalExecutableAgentRegistrar::new(catalog)),
            )),
            Arc::new(SqliteConfigStore::open(path.to_str().unwrap()).expect("config store")),
            Arc::new(StaticToolCatalog(vec![
                ToolDescriptor::pinned(
                    "managed",
                    "agent_run",
                    "Run an exact roster Agent",
                    json!({"type": "object"}),
                )
                .with_kind(ToolKind::AgentDelegation),
            ])),
        );
        let repository = ConfigPlaneManagedAgentRepository::new(plane, "workspace-a");
        let mut worker = create_params("worker");
        worker.model = ModelInput::Config(ModelConfigParams {
            id: "model-a".into(),
            speed: None,
            effort: None,
            inference_geo: Some(ModelInferenceGeo::Us),
        });
        let worker = repository.create("workspace-a", worker).await.unwrap();

        let coordinator = |geo: Option<ModelInferenceGeo>| {
            let mut params = create_params("coordinator");
            params.model = match geo {
                Some(geo) => ModelInput::Config(ModelConfigParams {
                    id: "model-a".into(),
                    speed: None,
                    effort: None,
                    inference_geo: Some(geo),
                }),
                None => ModelInput::Id("model-a".into()),
            };
            params.multiagent = Some(
                serde_json::from_value(json!({
                    "type": "coordinator",
                    "agents": [{"type":"agent", "id":worker.id.clone()}]
                }))
                .unwrap(),
            );
            params
        };
        let accepted = repository
            .create("workspace-a", coordinator(Some(ModelInferenceGeo::Us)))
            .await
            .expect("G1");
        assert_eq!(
            accepted.model.inference_geo,
            Some(ModelInferenceGeo::Us),
            "G1"
        );
        for (rule, geo) in [("G2", Some(ModelInferenceGeo::Global)), ("G3", None)] {
            let error = repository
                .create("workspace-a", coordinator(geo))
                .await
                .expect_err(rule);
            assert!(
                matches!(error, ManagedAgentError::Invalid(ref message) if message.contains("inference_geo mismatch")),
                "{rule}: {error}"
            );
        }
    }

    #[test]
    fn managed_multiagent_accepts_the_official_advisor_entry() {
        // Causes: C1 Agent reference; C2 official advisor entry; C3 unknown tag.
        // Effects: E1/E2 typed roster; E3 fail-fast decode before repository access.
        // Decision table: C1 -> E1; C2 -> E2; C3 -> E3.
        assert!(
            serde_json::from_value::<WireMultiagent>(json!({
                "type": "coordinator",
                "agents": [{"type":"agent", "id":"researcher"}]
            }))
            .is_ok(),
            "E1"
        );
        assert!(
            serde_json::from_value::<WireMultiagent>(json!({
                "type": "coordinator",
                "agents": [{"type":"advisor", "model":"claude-opus-5"}]
            }))
            .is_ok(),
            "E2"
        );
        assert!(
            serde_json::from_value::<WireMultiagent>(json!({
                "type": "coordinator",
                "agents": [{"type":"future", "model":"claude-opus-5"}]
            }))
            .is_err(),
            "E3"
        );
    }

    #[tokio::test]
    async fn model_update_preserves_acp_configuration_only_for_an_explicit_acp_executor() {
        // Cause/effect graph: advanced ACP configuration is authored in the
        // canonical AgentConfig control plane and is absent from the Managed
        // wire. Repeating the same public model id must preserve that intent;
        // native selection must never acquire it.
        //
        // | rule | current executor | same model | effect |
        // | U1   | native           | yes        | no ACP attachment |
        // | U2   | acp:<cli>        | yes        | control-plane ACP intent preserved |
        let native_temp = tempfile::tempdir().unwrap();
        let native_repository = ConfigPlaneManagedAgentRepository::new(
            plane(native_temp.path().join("config.sqlite").to_str().unwrap()),
            "workspace-a",
        );
        let native = native_repository
            .create("workspace-a", create_params("native"))
            .await
            .unwrap();
        let mut native_update = update_params(native.version);
        native_update.model = Some(ModelInput::Id("model-a".into()));
        let updated = native_repository
            .update("workspace-a", &native.id, native_update)
            .await
            .expect("U1");
        assert_eq!(updated.model.id, "model-a", "U1");

        let mut acp_params = create_params("acp");
        acp_params.model = ModelInput::Id("gpt-5;executor=acp:codex".into());
        let mut acp_config = config_from_create("agent_acp".into(), acp_params).expect("U2");
        acp_config
            .model_binding
            .set_acp_configuration(
                serde_json::from_value(json!({
                    "mode": "plan",
                    "options": {"reasoning_effort": "high"}
                }))
                .unwrap(),
            )
            .unwrap();
        let incoming = ModelInput::Id("gpt-5;executor=acp:codex".into()).into_config();
        let preserved = acp_configuration_to_preserve(&acp_config, &incoming.id).expect("U2");
        assert_eq!(preserved.mode.as_deref(), Some("plan"), "U2");
        assert_eq!(preserved.options["reasoning_effort"], "high", "U2");
    }

    #[test]
    fn managed_agent_uses_standard_defaults_without_projecting_private_controls() {
        // Cause/effect graph: C1 a standard Managed create has no private
        // controls; C2 the canonical control-plane AgentConfig may contain a
        // non-default step limit and plugins; C3 it may contain a native
        // sandbox-stdio MCP binding. Effects: E1 Managed authoring uses the
        // runtime default; E2 projection remains the official Agent/URL-MCP
        // shape; E3 internal controls and stdio binding remain in AgentConfig.
        //
        // | rule | source | private controls | effect |
        // | S1 | Managed create | absent | E1 |
        // | S2 | control-plane config | present | E2,E3 |
        // | S3 | control-plane stdio MCP | present | omitted from wire; no fake URL |
        let mut config = config_from_create("default-proof".into(), create_params("default-proof"))
            .expect("S1 config");
        assert_eq!(
            config.max_steps,
            awaken_runtime_contract::DEFAULT_MAX_STEPS,
            "S1"
        );
        config.max_steps = 40;
        config.plugin_ids.push("state_machine".into());
        config
            .plugin_config
            .insert("state_machine".into(), json!({"machines": []}));
        config.mcp_servers.push(AgentMcpServerBinding {
            name: "browser".into(),
            transport:
                awaken_runtime_contract::agent_bindings::AgentMcpTransportBinding::sandbox_stdio(
                    "playwright-mcp",
                    vec!["--headless".into()],
                ),
            prompts_as_skills: false,
            credential: None,
        });
        let projected = serde_json::to_value(project(AgentConfigRevision {
            revision: 1,
            config: config.clone(),
            created_at_unix_ms: Some(1_000),
            updated_at_unix_ms: Some(2_000),
        }))
        .unwrap();
        // T1: durable first-write time owns created_at; T2: this exact revision's
        // write time owns updated_at. Neither value is a protocol constant.
        assert_eq!(projected["created_at"], "1970-01-01T00:00:01Z", "T1");
        assert_eq!(projected["updated_at"], "1970-01-01T00:00:02Z", "T2");
        assert!(projected.get("max_steps").is_none(), "S2/E2");
        assert!(projected.get("state_machine").is_none(), "S2/E2");
        assert_eq!(projected["mcp_servers"].as_array().unwrap().len(), 1, "S3");
        assert_eq!(projected["mcp_servers"][0]["type"], "url", "S3");
        assert_eq!(config.max_steps, 40, "S2/E3");
        assert!(config.plugin_config.contains_key("state_machine"), "S2/E3");
        assert_eq!(config.mcp_servers.len(), 2, "S3/E3");
    }

    #[tokio::test]
    async fn reserved_assistant_projection_follows_the_composed_workspace_authority() {
        // Cause/effect graph: local composition owns one fixed Workspace and
        // hides the reserved Assistant elsewhere; hosted composition delegates
        // scope selection to the authenticated request and projects the same
        // reserved config after publication into that tenant Workspace.
        //
        // Decision table:
        // | Rule | repository authority | requested Workspace | effect |
        // | P1 | fixed A | A | project |
        // | P2 | fixed A | B | not found |
        // | P3 | request-scoped | B | project reserved Assistant |
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.sqlite");
        let (plane, catalog) = plane_with_catalog(path.to_str().unwrap());
        let mut config = awaken_admin_assistant::admin_assistant_config();
        config.model_binding = ModelSelection::pinned("provider", "model-a", "backend");
        config.tool_ids.clear();
        plane
            .put(&ScopeId::from(RESERVED_ADMIN_SCOPE), &config)
            .await
            .unwrap();
        let publication_a = plane
            .publish_for_execution_workspace(
                &ScopeId::from(RESERVED_ADMIN_SCOPE),
                "workspace-a",
                &config.id,
            )
            .await
            .unwrap();
        let publication_b = plane
            .publish_for_execution_workspace(
                &ScopeId::from(RESERVED_ADMIN_SCOPE),
                "workspace-b",
                &config.id,
            )
            .await
            .unwrap();
        assert_ne!(publication_a.fingerprint, publication_b.fingerprint);
        for (workspace_id, expected) in [
            ("workspace-a", &publication_a),
            ("workspace-b", &publication_b),
        ] {
            let durable = plane
                .publication_at_revision_for_execution_workspace(
                    &ScopeId::from(RESERVED_ADMIN_SCOPE),
                    workspace_id,
                    &config.id,
                    1,
                )
                .await
                .unwrap()
                .expect("targeted durable publication");
            assert_eq!(durable.fingerprint, expected.fingerprint);
            assert_eq!(
                catalog
                    .current(workspace_id, &config.id)
                    .expect("targeted executable registration")
                    .snapshot
                    .fingerprint
                    .0,
                expected.fingerprint
            );
        }
        let repository = ConfigPlaneManagedAgentRepository::new(plane.clone(), "workspace-a");

        let projected = repository
            .retrieve(
                "workspace-a",
                awaken_admin_assistant::ADMIN_ASSISTANT_AGENT_ID,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            projected.id,
            awaken_admin_assistant::ADMIN_ASSISTANT_AGENT_ID
        );
        assert!(matches!(
            repository
                .retrieve(
                    "workspace-b",
                    awaken_admin_assistant::ADMIN_ASSISTANT_AGENT_ID,
                    None,
                )
                .await,
            Err(ManagedAgentError::NotFound)
        ));
        assert_eq!(
            ConfigPlaneManagedAgentRepository::request_scoped(plane)
                .retrieve(
                    "workspace-b",
                    awaken_admin_assistant::ADMIN_ASSISTANT_AGENT_ID,
                    None,
                )
                .await
                .expect("P3 request-scoped projection")
                .id,
            awaken_admin_assistant::ADMIN_ASSISTANT_AGENT_ID
        );
    }
}
