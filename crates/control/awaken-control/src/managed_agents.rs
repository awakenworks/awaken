//! ACL between the Managed Agent wire API and the durable configuration plane.
//!
//! This adapter owns only representation mapping and lifecycle orchestration. The
//! ConfigPlane remains the single authoring source of truth; IAM policy stays in
//! the HTTP edge and never enters either repository.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_agent_contract::AgentSkillBinding;
use awaken_config_service::{ConfigPlane, RESERVED_ADMIN_SCOPE};
use awaken_config_store::{
    AgentConfig, AgentConfigRevision, AgentLifecycle, ConfigWrite, ModelSelection,
    MultiagentConfig, MultiagentTarget,
};
use awaken_protocol_managed::types::agent::{
    Agent, AgentCreateParams, AgentListParams, AgentSkill, AgentStatus, AgentTool,
    AgentUpdateParams, CustomToolInputSchema, MultiagentConfig as WireMultiagent,
    MultiagentRosterEntry, UrlMcpServer, UrlMcpServerKind,
};
use awaken_protocol_managed::types::{ModelConfig, ModelEffort, ModelSpeed};
use awaken_protocol_managed::{ManagedAgentError, ManagedAgentRepository};
use awaken_runtime_contract::agent_bindings::AgentMcpServerBinding;
use awaken_runtime_contract::agent_bindings::{InferenceOptions, InferenceSpeed, ReasoningEffort};
use awaken_runtime_contract::agent_bindings::{ToolsetPolicy, ToolsetSource};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_tenancy::ScopeId;
use sha2::{Digest, Sha256};

const OBJECT_AT: &str = "2026-01-01T00:00:00Z";
static AGENT_ID_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn new_agent_id(workspace_id: &str) -> String {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let sequence = AGENT_ID_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let entropy = format!(
        "{workspace_id}:{}:{timestamp}:{sequence}",
        std::process::id()
    );
    let digest = Sha256::digest(entropy.as_bytes());
    let encoded = format!("{digest:x}");
    format!("agent_{}", &encoded[..32])
}

fn lifecycle_timestamp() -> String {
    let milliseconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default();
    awaken_protocol_managed::cron::to_rfc3339(milliseconds)
}

pub struct ConfigPlaneManagedAgentRepository {
    plane: ConfigPlane,
    platform_workspace: String,
}

impl ConfigPlaneManagedAgentRepository {
    pub fn new(plane: ConfigPlane, platform_workspace: impl Into<String>) -> Self {
        Self {
            plane,
            platform_workspace: platform_workspace.into(),
        }
    }

    fn scope(workspace_id: &str) -> ScopeId {
        ScopeId::from(workspace_id)
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
        if current.is_some()
            || workspace_id != self.platform_workspace
            || self
                .plane
                .service()
                .installed_in(workspace_id, id)
                .is_none()
        {
            return Ok(current);
        }
        self.plane
            .get_versioned(&ScopeId::from(RESERVED_ADMIN_SCOPE), id)
            .await
            .map_err(ManagedAgentError::Storage)
    }

    fn project_current(&self, workspace_id: &str, mut revision: AgentConfigRevision) -> Agent {
        if let Some(snapshot) = self
            .plane
            .service()
            .installed_in(workspace_id, &revision.config.id)
        {
            let binding = snapshot.resolved_spec.model_binding.binding;
            revision.config.model_binding = ModelSelection::Pinned(binding);
        }
        project(revision)
    }

    async fn publish_if_resolvable(
        &self,
        scope: &ScopeId,
        id: &str,
    ) -> Result<(), ManagedAgentError> {
        match self.plane.publish(scope, id).await {
            Ok(_) => Ok(()),
            // A Managed Agent is also an authoring resource. It remains a durable
            // draft when its model/tool inputs cannot yet be resolved; a later
            // config publish activates the same aggregate.
            Err(
                awaken_config_service::PublishError::Unresolvable(_)
                | awaken_config_service::PublishError::Compile(_),
            ) => Ok(()),
            Err(error) => Err(ManagedAgentError::Storage(error.to_string())),
        }
    }

    async fn resolve_multiagent_references(
        &self,
        workspace_id: &str,
        config: &mut AgentConfig,
    ) -> Result<(), ManagedAgentError> {
        let Some(multiagent) = config.multiagent.as_mut() else {
            return Ok(());
        };
        multiagent
            .validate(&config.id)
            .map_err(ManagedAgentError::Invalid)?;
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
            *version = Some(selected.revision);
        }
        Ok(())
    }
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

fn typed_mcp_servers(values: Vec<UrlMcpServer>) -> Vec<AgentMcpServerBinding> {
    values
        .into_iter()
        .map(|server| AgentMcpServerBinding {
            name: server.name,
            url: server.url,
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
            })
            .collect(),
    }
}

fn config_from_create(
    id: String,
    params: AgentCreateParams,
) -> Result<AgentConfig, ManagedAgentError> {
    let model = params.model.into_config();
    let inference = inference_from_wire(model.speed, model.effort.map(|value| value.resolved()));
    let multiagent = params.multiagent.map(typed_multiagent);
    let config = AgentConfig {
        id,
        instructions: params.system.unwrap_or_default(),
        max_steps: 8,
        delegation_limits: Default::default(),
        model_binding: ModelSelection::pinned("", model.id, ""),
        inference,
        tool_ids: Vec::new(),
        toolsets: awaken_protocol_managed::project::toolset_policies(&params.tools),
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
        hand: None,
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
        if server.url.chars().count() > 2048
            || awaken_agent_contract::McpTarget::identity(&server.url).is_err()
        {
            return Err(ManagedAgentError::Invalid(
                "mcp_server url must be an HTTP(S) URL of at most 2048 characters".into(),
            ));
        }
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
            if toolset.source == ToolsetSource::Agent
                && !awaken_protocol_managed::project::is_agent_toolset_member(&entry.name)
            {
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
    let declared_count = config.client_tools.len()
        + config
            .toolsets
            .iter()
            .map(|toolset| match toolset.source {
                ToolsetSource::Agent => {
                    awaken_protocol_managed::project::AGENT_TOOLSET_TOOL_IDS.len()
                }
                ToolsetSource::Mcp { .. } => toolset.overrides.len(),
            })
            .sum::<usize>();
    if declared_count > 128 {
        return Err(ManagedAgentError::Invalid(
            "tools supports at most 128 declared entries".into(),
        ));
    }
    Ok(())
}

fn inference_from_wire(speed: Option<ModelSpeed>, effort: Option<ModelEffort>) -> InferenceOptions {
    InferenceOptions {
        speed: speed.map(|value| match value {
            ModelSpeed::Standard => InferenceSpeed::Standard,
            ModelSpeed::Fast => InferenceSpeed::Fast,
        }),
        effort: effort.map(|value| match value {
            ModelEffort::Low => ReasoningEffort::Low,
            ModelEffort::Medium => ReasoningEffort::Medium,
            ModelEffort::High => ReasoningEffort::High,
            ModelEffort::Xhigh => ReasoningEffort::Xhigh,
            ModelEffort::Max => ReasoningEffort::Max,
        }),
    }
}

fn model_config(model: String, inference: InferenceOptions) -> ModelConfig {
    ModelConfig {
        id: model,
        speed: inference.speed.map(|value| match value {
            InferenceSpeed::Standard => ModelSpeed::Standard,
            InferenceSpeed::Fast => ModelSpeed::Fast,
        }),
        effort: inference.effort.map(|value| match value {
            ReasoningEffort::Low => ModelEffort::Low,
            ReasoningEffort::Medium => ModelEffort::Medium,
            ReasoningEffort::High => ModelEffort::High,
            ReasoningEffort::Xhigh => ModelEffort::Xhigh,
            ReasoningEffort::Max => ModelEffort::Max,
        }),
    }
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
    let mut tools = awaken_protocol_managed::project::resolved_toolsets(toolsets);
    tools.extend(client_tools.iter().map(|tool| {
        AgentTool::Custom {
            name: tool.id.clone(),
            description: tool.description.clone(),
            input_schema: CustomToolInputSchema::from_value(tool.parameters.clone())
                .expect("published client-tool schemas were validated at admission"),
        }
    }));
    tools
}

fn project(revision: AgentConfigRevision) -> Agent {
    let revision_number = revision.revision;
    let config = revision.config;
    let id = config.id.clone();
    let model = config
        .model_binding
        .resolved()
        .map(|binding| binding.model_ref.clone())
        .unwrap_or_default();
    let tools = wire_tools(&config.toolsets, &config.client_tools);
    let status = match config.lifecycle() {
        AgentLifecycle::Published => AgentStatus::Published,
        AgentLifecycle::Disabled => AgentStatus::Disabled,
        AgentLifecycle::Archived => AgentStatus::Archived,
    };
    Agent {
        id: id.clone(),
        object_type: "agent",
        archived_at: config.archived_at,
        disabled_at: config.disabled_at,
        status,
        created_at: OBJECT_AT.to_string(),
        updated_at: OBJECT_AT.to_string(),
        name: config.name.unwrap_or_else(|| id.clone()),
        description: config.description,
        model: model_config(model, config.inference),
        system: (!config.instructions.is_empty()).then_some(config.instructions),
        metadata: config.metadata,
        mcp_servers: config
            .mcp_servers
            .into_iter()
            .map(|server| UrlMcpServer {
                name: server.name,
                url: server.url,
                kind: UrlMcpServerKind::Url,
            })
            .collect(),
        skills: config
            .skills
            .into_iter()
            .map(AgentSkill::from_binding)
            .collect(),
        tools,
        multiagent: config.multiagent.map(|value| WireMultiagent::Coordinator {
            agents: value
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
                })
                .collect(),
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
        match self
            .plane
            .put_if_revision(&scope, &config, 0)
            .await
            .map_err(ManagedAgentError::Storage)?
        {
            ConfigWrite::Applied { revision } => {
                self.publish_if_resolvable(&scope, &id).await?;
                Ok(project(AgentConfigRevision { config, revision }))
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
        self.versioned_for_read(workspace_id, id)
            .await?
            .map(|revision| self.project_current(workspace_id, revision))
            .ok_or(ManagedAgentError::NotFound)
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
            let agent = self.project_current(workspace_id, versioned);
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
            let current_model = config
                .model_binding
                .resolved()
                .map(|binding| binding.model_ref.as_str());
            let preserve_effort =
                current_model == Some(model.id.as_str()) && model.effort.is_none();
            let prior_effort = config.inference.effort;
            config.inference =
                inference_from_wire(model.speed, model.effort.map(|value| value.resolved()));
            if preserve_effort {
                config.inference.effort = prior_effort;
            }
            config.model_binding = ModelSelection::pinned("", model.id, "");
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
            config.toolsets = awaken_protocol_managed::project::toolset_policies(&tools);
            config.client_tools = client_tools(&tools);
        }
        if let Some(multiagent) = params.multiagent {
            config.multiagent = multiagent.map(typed_multiagent);
        }
        validate_managed_agent_config(&config)?;
        self.resolve_multiagent_references(workspace_id, &mut config)
            .await?;
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
                self.publish_if_resolvable(&scope, id).await?;
                Ok(project(AgentConfigRevision { config, revision }))
            }
            ConfigWrite::Conflict { current_revision } => Err(ManagedAgentError::Conflict(
                format!("Agent changed concurrently (current version: {current_revision:?})"),
            )),
        }
    }

    async fn disable(&self, workspace_id: &str, id: &str) -> Result<Agent, ManagedAgentError> {
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
        match current.config.lifecycle() {
            AgentLifecycle::Disabled => return Ok(project(current)),
            AgentLifecycle::Archived => {
                return Err(ManagedAgentError::Invalid(
                    "archived Agent cannot be disabled".into(),
                ));
            }
            AgentLifecycle::Published => {}
        }
        let mut config = current.config;
        config.disabled_at = Some(lifecycle_timestamp());
        match self
            .plane
            .put_if_revision(&scope, &config, current.revision)
            .await
            .map_err(ManagedAgentError::Storage)?
        {
            ConfigWrite::Applied { revision } => {
                self.plane.uninstall(workspace_id, id);
                Ok(project(AgentConfigRevision { config, revision }))
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
                self.plane.uninstall(workspace_id, id);
                Ok(project(AgentConfigRevision { config, revision }))
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
        if revisions.is_empty()
            && workspace_id == self.platform_workspace
            && self
                .plane
                .service()
                .installed_in(workspace_id, id)
                .is_some()
        {
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

    use awaken_config_service::{
        ConfigService, ModelPublicationResolver, ResolvedPublicationModels, StaticToolCatalog,
    };
    use awaken_config_store::{ModelSelection, SqliteConfigStore};
    use awaken_protocol_managed::types::agent::{AgentCreateParams, AgentUpdateParams, ModelInput};
    use awaken_runtime_contract::resolved::ModelBinding;
    use serde_json::json;

    use super::*;

    struct TestModelResolver;

    #[async_trait::async_trait]
    impl ModelPublicationResolver for TestModelResolver {
        async fn resolve_models(
            &self,
            _workspace: &awaken_tenancy::ScopeId,
            selection: &ModelSelection,
            candidates: &[ModelBinding],
        ) -> Result<ResolvedPublicationModels, awaken_config_service::PublicationResolutionError>
        {
            let primary = selection
                .resolved()
                .cloned()
                .ok_or_else(|| "test requires a pinned model".to_string())?;
            Ok(ResolvedPublicationModels::host(
                primary,
                candidates.to_vec(),
                None,
                None,
            ))
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

    fn plane(path: &str) -> ConfigPlane {
        ConfigPlane::new(
            Arc::new(ConfigService::new(Arc::new(TestModelResolver))),
            Arc::new(SqliteConfigStore::open(path).expect("config store")),
            Arc::new(StaticToolCatalog(Vec::new())),
        )
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
        });
        assert!(projected.tools.is_empty());
    }

    #[tokio::test]
    async fn managed_agent_lifecycle_follows_disable_archive_retention_rules() {
        // Cause/effect graph:
        // C1 Published + disable -> E1 current execution is unavailable while
        // the immutable publication remains addressable; C2 Disabled + disable
        // -> E2 idempotent; C3 Disabled + update/publish -> E3 fail closed;
        // C4 Disabled + archive -> E4 Archived terminal state with the same
        // historical publication retained; C5 Archived + archive -> E5
        // idempotent.
        //
        // Decision table:
        // | rule | current   | command | current executable | exact snapshot | result |
        // | L1   | Published | disable | no                 | yes            | Disabled |
        // | L2   | Disabled  | disable | no                 | yes            | no new revision |
        // | L3   | Disabled  | update  | no                 | yes            | reject |
        // | L4   | Disabled  | archive | no                 | yes            | Archived |
        // | L5   | Archived  | archive | no                 | yes            | no new revision |
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.sqlite");
        let plane = plane(path.to_str().unwrap());
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
        assert!(
            plane
                .service()
                .installed_in("workspace-a", &created.id)
                .is_some()
        );
        assert_eq!(created.status, AgentStatus::Published);
        let fingerprint = plane
            .service()
            .installed_in("workspace-a", &created.id)
            .expect("published")
            .fingerprint
            .0;

        let disabled = repository
            .disable("workspace-a", &created.id)
            .await
            .unwrap();
        assert_eq!(disabled.version, 2, "L1");
        assert_eq!(disabled.status, AgentStatus::Disabled, "L1");
        assert!(disabled.disabled_at.is_some(), "L1");
        assert!(
            plane
                .service()
                .installed_in("workspace-a", &created.id)
                .is_none(),
            "L1"
        );
        assert!(
            plane
                .publication(&ScopeId::from("workspace-a"), &fingerprint)
                .await
                .unwrap()
                .is_some(),
            "L1"
        );

        let disabled_again = repository
            .disable("workspace-a", &created.id)
            .await
            .unwrap();
        assert_eq!(disabled_again.version, 2, "L2");
        assert!(
            matches!(
                repository
                    .update("workspace-a", &created.id, update_params(2))
                    .await,
                Err(ManagedAgentError::Invalid(_))
            ),
            "L3"
        );
        assert!(
            plane
                .publish(&ScopeId::from("workspace-a"), &created.id)
                .await
                .is_err(),
            "L3"
        );

        let archived = repository
            .archive("workspace-a", &created.id)
            .await
            .unwrap();
        assert_eq!(archived.version, 3, "L4");
        assert_eq!(archived.status, AgentStatus::Archived, "L4");
        assert!(archived.disabled_at.is_none(), "L4");
        assert!(archived.archived_at.is_some(), "L4");
        assert!(
            plane
                .publication(&ScopeId::from("workspace-a"), &fingerprint)
                .await
                .unwrap()
                .is_some(),
            "L4"
        );
        let archived_again = repository
            .archive("workspace-a", &created.id)
            .await
            .unwrap();
        assert_eq!(archived_again.version, 3, "L5");
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
        // | fast + {type:xhigh}   | exact typed values | exact typed values   |
        // | repository restart    | values preserved   | republished values   |
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.sqlite");
        let id = {
            let plane = plane(path.to_str().unwrap());
            let repository = ConfigPlaneManagedAgentRepository::new(plane.clone(), "workspace-a");
            let mut params = create_params("controlled");
            params.model = serde_json::from_value(json!({
                "id": "claude-opus-4-8",
                "speed": "fast",
                "effort": {"type": "xhigh"}
            }))
            .unwrap();
            let created = repository.create("workspace-a", params).await.unwrap();
            assert_eq!(created.model.speed, Some(ModelSpeed::Fast));
            assert_eq!(created.model.effort, Some(ModelEffort::Xhigh));
            let installed = plane
                .service()
                .installed_in("workspace-a", &created.id)
                .expect("create publishes an executable revision");
            assert_eq!(
                installed.resolved_spec.plugin_config.inference,
                InferenceOptions {
                    speed: Some(InferenceSpeed::Fast),
                    effort: Some(ReasoningEffort::Xhigh),
                }
            );
            created.id
        };

        let plane = plane(path.to_str().unwrap());
        let repository = ConfigPlaneManagedAgentRepository::new(plane.clone(), "workspace-a");
        let restored = repository.retrieve("workspace-a", &id, None).await.unwrap();
        assert_eq!(restored.model.speed, Some(ModelSpeed::Fast));
        assert_eq!(restored.model.effort, Some(ModelEffort::Xhigh));
        plane
            .publish(&ScopeId::from("workspace-a"), &id)
            .await
            .expect("reconciliation republishes the restored authoring revision");
        let installed = plane
            .service()
            .installed_in("workspace-a", &id)
            .expect("restart restores the same executable publication");
        assert_eq!(
            installed.resolved_spec.plugin_config.inference,
            InferenceOptions {
                speed: Some(InferenceSpeed::Fast),
                effort: Some(ReasoningEffort::Xhigh),
            }
        );
    }

    #[tokio::test]
    async fn reserved_assistant_projects_only_into_the_platform_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.sqlite");
        let plane = plane(path.to_str().unwrap());
        let mut config = awaken_admin_assistant::admin_assistant_config();
        config.model_binding = ModelSelection::pinned("provider", "model-a", "backend");
        config.tool_ids.clear();
        plane
            .put(&ScopeId::from(RESERVED_ADMIN_SCOPE), &config)
            .await
            .unwrap();
        plane
            .publish_for_execution_workspace(
                &ScopeId::from(RESERVED_ADMIN_SCOPE),
                "workspace-a",
                &config.id,
            )
            .await
            .unwrap();
        let repository = ConfigPlaneManagedAgentRepository::new(plane, "workspace-a");

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
    }
}
