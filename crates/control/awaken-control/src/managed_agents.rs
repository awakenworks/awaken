//! ACL between the Managed Agent wire API and the durable configuration plane.
//!
//! This adapter owns only representation mapping and lifecycle orchestration. The
//! ConfigPlane remains the single authoring source of truth; IAM policy stays in
//! the HTTP edge and never enters either repository.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_agent_config::{
    AgentConfig, AgentConfigRevision, AgentKind, AgentLifecycle, ConfigWrite, ModelSelection,
    MultiagentConfig, MultiagentTarget,
};
use awaken_agent_contract::AgentSkillBinding;
use awaken_config_service::{
    ConfigPlane, RESERVED_ADMIN_SCOPE, parse_managed_model_id, render_managed_model_id,
};
use awaken_protocol_managed::types::agent::{
    Agent, AgentCreateParams, AgentListParams, AgentMcpServer, AgentSkill, AgentStatus,
    AgentUpdateParams, AwakenAgentExtensions, MultiagentConfig as WireMultiagent,
    MultiagentRosterEntry,
};
use awaken_protocol_managed::types::{AwakenModelExtensions, ModelConfig, ModelEffort, ModelSpeed};
use awaken_protocol_managed::{ManagedAgentError, ManagedAgentRepository};
use awaken_runtime_contract::agent_bindings::AgentMcpServerBinding;
use awaken_runtime_contract::agent_bindings::{InferenceOptions, InferenceSpeed, ReasoningEffort};
use awaken_runtime_contract::agent_bindings::{ToolsetPolicy, ToolsetSource};
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_session_contract::{
    AGENT_TOOLSET_TOOL_IDS, AgentTool, CustomToolInputSchema, is_agent_toolset_member,
    resolved_toolsets, toolset_policies,
};
use awaken_tenancy::ScopeId;
use sha2::{Digest, Sha256};

const OBJECT_AT: &str = "2026-01-01T00:00:00Z";
const STATE_MACHINE_PLUGIN_ID: &str = "state_machine";
const MAX_MCP_SERVER_URL_BYTES: usize = 2048;
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
    awaken_session_contract::epoch_millis_to_rfc3339(milliseconds)
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

    async fn publication_for_read(
        &self,
        workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Result<Option<awaken_agent_config::StoredPublication>, ManagedAgentError> {
        let direct = self
            .plane
            .publication_at_revision(&Self::scope(workspace_id), agent_id, source_revision)
            .await
            .map_err(ManagedAgentError::Storage)?;
        if direct.is_some() || workspace_id != self.platform_workspace {
            return Ok(direct);
        }
        self.plane
            .publication_at_revision(
                &ScopeId::from(RESERVED_ADMIN_SCOPE),
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
        if workspace_id != self.platform_workspace {
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
        let coordinator_geo = config.inference.inference_geo.clone();
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
        .map(|server| {
            let (name, transport, prompts_as_skills) = match server {
                AgentMcpServer::Url {
                    name,
                    url,
                    prompts_as_skills,
                } => (
                    name,
                    awaken_runtime_contract::agent_bindings::AgentMcpTransportBinding::http(url),
                    prompts_as_skills,
                ),
                AgentMcpServer::SandboxStdio {
                    name,
                    command,
                    args,
                    prompts_as_skills,
                } => (
                    name,
                    awaken_runtime_contract::agent_bindings::AgentMcpTransportBinding::sandbox_stdio(
                        command, args,
                    ),
                    prompts_as_skills,
                ),
            };
            AgentMcpServerBinding {
                name,
                transport,
                prompts_as_skills,
                credential: None,
            }
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
    let model = params.model.into_config();
    let inference = inference_from_wire(
        model.speed,
        model.effort.map(|value| value.resolved()),
        model.inference_geo,
    );
    let mut model_binding = parse_managed_model_id(&model.id)
        .map_err(|error| ManagedAgentError::Invalid(error.to_string()))?;
    apply_model_extensions(&mut model_binding, model.x_awaken)?;
    let multiagent = params.multiagent.map(typed_multiagent);
    let extensions = params.x_awaken.unwrap_or(AwakenAgentExtensions {
        max_steps: None,
        state_machine: None,
    });
    let plugin_ids = extensions
        .state_machine
        .as_ref()
        .map(|_| vec![STATE_MACHINE_PLUGIN_ID.to_string()])
        .unwrap_or_default();
    let plugin_config = extensions
        .state_machine
        .map(|config| BTreeMap::from([(STATE_MACHINE_PLUGIN_ID.to_string(), config)]))
        .unwrap_or_default();
    let config = AgentConfig {
        id,
        instructions: params.system.unwrap_or_default(),
        max_steps: extensions.max_steps.unwrap_or(8),
        delegation_limits: Default::default(),
        model_binding,
        inference,
        tool_ids: Vec::new(),
        toolsets: toolset_policies(&params.tools),
        client_tools: client_tools(&params.tools),
        plugin_ids,
        plugin_config,
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
    if config.max_steps == 0 {
        return Err(ManagedAgentError::Invalid(
            "x_awaken.max_steps must be greater than or equal to 1".into(),
        ));
    }
    if config
        .inference
        .inference_geo
        .as_deref()
        .is_some_and(|geo| geo.trim().is_empty())
    {
        return Err(ManagedAgentError::Invalid(
            "model.inference_geo must not be empty".into(),
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
    let declared_count = config.client_tools.len()
        + config
            .toolsets
            .iter()
            .map(|toolset| match toolset.source {
                ToolsetSource::Agent => AGENT_TOOLSET_TOOL_IDS.len(),
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

fn inference_from_wire(
    speed: Option<ModelSpeed>,
    effort: Option<ModelEffort>,
    inference_geo: Option<String>,
) -> InferenceOptions {
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
        inference_geo,
    }
}

fn apply_model_extensions(
    selection: &mut ModelSelection,
    extensions: Option<AwakenModelExtensions>,
) -> Result<(), ManagedAgentError> {
    let Some(AwakenModelExtensions { acp }) = extensions else {
        return Ok(());
    };
    let Some(configuration) = acp else {
        return Ok(());
    };
    selection
        .set_acp_configuration(configuration)
        .map_err(|error| ManagedAgentError::Invalid(error.into()))
}

fn acp_configuration_to_preserve(
    config: &AgentConfig,
    incoming_model_id: &str,
    extension_omitted: bool,
) -> Option<awaken_runtime_contract::resolved::AcpSessionConfiguration> {
    let same_model = render_managed_model_id(&config.model_binding)
        .is_ok_and(|current| current == incoming_model_id);
    (same_model
        && extension_omitted
        && matches!(config.kind(), AgentKind::Acp { ref cli } if !cli.is_empty()))
    .then(|| config.model_binding.acp_configuration().cloned())
    .flatten()
}

fn model_config(
    model: String,
    inference: InferenceOptions,
    acp: Option<&awaken_runtime_contract::resolved::AcpSessionConfiguration>,
) -> ModelConfig {
    let mut projected = ModelConfig::from_inference(model, inference);
    projected.x_awaken =
        acp.filter(|configuration| !configuration.is_empty())
            .map(|configuration| AwakenModelExtensions {
                acp: Some(configuration.clone()),
            });
    projected
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
    let model = render_managed_model_id(&config.model_binding).unwrap_or_default();
    let tools = wire_tools(&config.toolsets, &config.client_tools);
    let status = match config.lifecycle() {
        AgentLifecycle::Published => AgentStatus::Published,
        AgentLifecycle::Disabled => AgentStatus::Disabled,
        AgentLifecycle::Archived => AgentStatus::Archived,
    };
    let state_machine = config
        .plugin_ids
        .iter()
        .any(|id| id == STATE_MACHINE_PLUGIN_ID)
        .then(|| config.plugin_config.get(STATE_MACHINE_PLUGIN_ID).cloned())
        .flatten();
    let x_awaken =
        (config.max_steps != 8 || state_machine.is_some()).then_some(AwakenAgentExtensions {
            max_steps: (config.max_steps != 8).then_some(config.max_steps),
            state_machine,
        });
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
        model: model_config(
            model,
            config.inference,
            config.model_binding.acp_configuration(),
        ),
        system: (!config.instructions.is_empty()).then_some(config.instructions),
        metadata: config.metadata,
        mcp_servers: config
            .mcp_servers
            .into_iter()
            .map(|server| match server.transport {
                awaken_runtime_contract::agent_bindings::AgentMcpTransportBinding::Http(
                    transport,
                ) => awaken_protocol_managed::types::agent::AgentMcpServer::Url {
                    name: server.name,
                    url: transport.url,
                    prompts_as_skills: server.prompts_as_skills,
                },
                awaken_runtime_contract::agent_bindings::AgentMcpTransportBinding::SandboxStdio(
                    transport,
                ) => awaken_protocol_managed::types::agent::AgentMcpServer::SandboxStdio {
                    name: server.name,
                    command: transport.command,
                    args: transport.args,
                    prompts_as_skills: server.prompts_as_skills,
                },
            })
            .collect(),
        skills: config
            .skills
            .into_iter()
            .map(AgentSkill::from_binding)
            .collect(),
        tools,
        multiagent: config.multiagent.map(|value| {
            let mut advisor = None;
            let mut agents = value
                .agents
                .into_iter()
                .filter_map(|target| match target {
                    MultiagentTarget::Agent { id, version } => {
                        Some(MultiagentRosterEntry::Reference(
                            awaken_protocol_managed::types::agent::AgentRosterReference {
                                id,
                                kind: awaken_protocol_managed::types::agent::AgentRosterReferenceKind::Agent,
                                version: Some(version.unwrap_or(1)),
                            },
                        ))
                    }
                    MultiagentTarget::SelfReference => Some(MultiagentRosterEntry::Reference(
                        awaken_protocol_managed::types::agent::AgentRosterReference {
                            id: id.clone(),
                            kind: awaken_protocol_managed::types::agent::AgentRosterReferenceKind::Agent,
                            version: Some(revision_number),
                        },
                    )),
                    MultiagentTarget::Advisor { model } => {
                        advisor = Some(MultiagentRosterEntry::Advisor(
                            awaken_protocol_managed::types::agent::AdvisorRosterReference {
                                model,
                                kind: awaken_protocol_managed::types::agent::AdvisorRosterReferenceKind::Advisor,
                            },
                        ));
                        None
                    }
                })
                .collect::<Vec<_>>();
            agents.extend(advisor);
            WireMultiagent::Coordinator { agents }
        }),
        x_awaken,
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
            let prior_acp =
                acp_configuration_to_preserve(&config, &model.id, model.x_awaken.is_none());
            config.inference = inference_from_wire(
                model.speed,
                model.effort.map(|value| value.resolved()),
                model.inference_geo,
            );
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
            } else {
                apply_model_extensions(&mut config.model_binding, model.x_awaken)?;
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
        if let Some(extensions) = params.x_awaken {
            if let Some(max_steps) = extensions.max_steps {
                config.max_steps = max_steps;
            }
            if let Some(state_machine) = extensions.state_machine {
                if !config
                    .plugin_ids
                    .iter()
                    .any(|id| id == STATE_MACHINE_PLUGIN_ID)
                {
                    config.plugin_ids.push(STATE_MACHINE_PLUGIN_ID.to_string());
                }
                config
                    .plugin_config
                    .insert(STATE_MACHINE_PLUGIN_ID.to_string(), state_machine);
            }
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
                self.plane
                    .withdraw(workspace_id, id, revision)
                    .await
                    .map_err(ManagedAgentError::Storage)?;
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
                self.plane
                    .withdraw(workspace_id, id, revision)
                    .await
                    .map_err(ManagedAgentError::Storage)?;
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
        if revisions.is_empty() && workspace_id == self.platform_workspace {
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
    use awaken_runtime_contract::resolved::{ModelBinding, ToolDescriptor, ToolKind};
    use serde_json::json;

    use super::*;

    struct TestModelResolver;

    struct RejectModelResolver;

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
            Ok(ResolvedPublicationModels::host(
                primary,
                candidates.to_vec(),
                None,
                None,
            ))
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
            x_awaken: None,
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
            x_awaken: None,
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

        let AgentMcpServer::Url { url, .. } = &mut at_max.mcp_servers[0] else {
            unreachable!("HTTP fixture")
        };
        url.push('x');
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
    async fn managed_create_fails_fast_and_never_exposes_an_unpublished_draft() {
        // Causes: C1 valid Managed request; C2 model planning rejects before
        // persistence; C3 a race could reject again after a staged CAS write.
        // Effects: E1 create returns Invalid; E2 retrieve/list expose no Agent;
        // E3 config-authoring may retain an internal draft without becoming a
        // second Managed aggregate. The rejecting resolver covers C2; E2 also
        // protects the C3 staged-write boundary.
        let temp = tempfile::tempdir().unwrap();
        let plane = rejecting_plane(temp.path().join("config.sqlite").to_str().unwrap());
        let repository = ConfigPlaneManagedAgentRepository::new(plane, "workspace-a");
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
    async fn managed_agent_stdio_mcp_create_update_decision_table() {
        // Cause/effect graph: the tagged Managed MCP union is normalized into
        // the one AgentMcpTransportBinding before persistence/publication. A
        // valid stdio replacement produces an exact executable MCP target;
        // an invalid command fails before a new Agent revision is committed.
        //
        // | rule | current transport | update transport       | effect |
        // | S1   | URL               | sandbox_stdio valid    | revision + exact stdio target |
        // | S2   | sandbox_stdio     | sandbox_stdio invalid  | Invalid; current revision retained |
        // | S3   | sandbox_stdio     | read projection        | same tagged command/args returned |
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.sqlite");
        let (plane, catalog) = plane_with_catalog(path.to_str().unwrap());
        let repository = ConfigPlaneManagedAgentRepository::new(plane, "workspace-a");
        let created = repository
            .create("workspace-a", create_params("browser"))
            .await
            .unwrap();

        let mut update = update_params(created.version);
        update.mcp_servers = Some(Some(vec![
            serde_json::from_value(json!({
                "type": "sandbox_stdio",
                "name": "docs",
                "command": "playwright-mcp",
                "args": ["--headless"]
            }))
            .unwrap(),
        ]));
        let updated = repository
            .update("workspace-a", &created.id, update)
            .await
            .unwrap();
        assert_eq!(updated.version, created.version + 1, "S1");
        assert!(
            matches!(
                &updated.mcp_servers[0],
                AgentMcpServer::SandboxStdio { command, args, .. }
                    if command == "playwright-mcp" && args == &["--headless"]
            ),
            "S3"
        );
        let registration = catalog
            .current("workspace-a", &created.id)
            .expect("S1 registered");
        assert!(
            matches!(
                &registration.session_profile.mcp_servers[0].target,
                awaken_agent_contract::McpTarget::SandboxStdio(target)
                    if target.command == "playwright-mcp" && target.args == ["--headless"]
            ),
            "S1"
        );

        let mut invalid = update_params(updated.version);
        invalid.mcp_servers = Some(Some(vec![
            serde_json::from_value(json!({
                "type": "sandbox_stdio",
                "name": "docs",
                "command": ""
            }))
            .unwrap(),
        ]));
        assert!(
            matches!(
                repository.update("workspace-a", &created.id, invalid).await,
                Err(ManagedAgentError::Invalid(_))
            ),
            "S2"
        );
        assert_eq!(
            repository
                .retrieve("workspace-a", &created.id, None)
                .await
                .unwrap()
                .version,
            updated.version,
            "S2"
        );
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
        let (plane, catalog) = plane_with_catalog(path.to_str().unwrap());
        let repository = ConfigPlaneManagedAgentRepository::new(plane.clone(), "workspace-a");

        let created = repository
            .create("workspace-a", create_params("assistant"))
            .await
            .unwrap();
        assert_eq!(created.version, 1);
        assert!(matches!(
            &created.mcp_servers[0],
            awaken_protocol_managed::types::agent::AgentMcpServer::Url { name, .. }
                if name == "docs"
        ));
        assert!(matches!(
            &created.skills[0],
            AgentSkill::Custom { skill_id, .. } if skill_id == "skill-docs"
        ));
        assert!(catalog.current("workspace-a", &created.id).is_some());
        assert_eq!(created.status, AgentStatus::Published);
        let fingerprint = catalog
            .current("workspace-a", &created.id)
            .expect("published")
            .snapshot
            .fingerprint
            .0;

        let disabled = repository
            .disable("workspace-a", &created.id)
            .await
            .unwrap();
        assert_eq!(disabled.version, 2, "L1");
        assert_eq!(disabled.status, AgentStatus::Disabled, "L1");
        assert!(disabled.disabled_at.is_some(), "L1");
        assert!(catalog.current("workspace-a", &created.id).is_none(), "L1");
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
        // | fast + xhigh + us     | exact typed values | exact typed values   |
        // | repository restart    | values preserved   | republished values   |
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.sqlite");
        let id = {
            let (plane, catalog) = plane_with_catalog(path.to_str().unwrap());
            let repository = ConfigPlaneManagedAgentRepository::new(plane.clone(), "workspace-a");
            let mut params = create_params("controlled");
            params.model = serde_json::from_value(json!({
                "id": "claude-opus-4-8",
                "speed": "fast",
                "effort": {"type": "xhigh"},
                "inference_geo": "us"
            }))
            .unwrap();
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
                    inference_geo: Some("us".into()),
                }
            );
            created.id
        };

        let (plane, catalog) = plane_with_catalog(path.to_str().unwrap());
        let repository = ConfigPlaneManagedAgentRepository::new(plane.clone(), "workspace-a");
        let restored = repository.retrieve("workspace-a", &id, None).await.unwrap();
        assert_eq!(restored.model.speed, Some(ModelSpeed::Fast));
        assert_eq!(restored.model.effort, Some(ModelEffort::Xhigh));
        assert_eq!(restored.model.inference_geo.as_deref(), Some("us"));
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
                inference_geo: Some("us".into()),
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
        worker.model = serde_json::from_value(json!({
            "id": "model-a",
            "inference_geo": "us"
        }))
        .unwrap();
        let worker = repository.create("workspace-a", worker).await.unwrap();

        let coordinator = |geo: Option<&str>| {
            let mut params = create_params("coordinator");
            params.model = match geo {
                Some(geo) => serde_json::from_value(json!({
                    "id": "model-a",
                    "inference_geo": geo
                }))
                .unwrap(),
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
            .create("workspace-a", coordinator(Some("us")))
            .await
            .expect("G1");
        assert_eq!(accepted.model.inference_geo.as_deref(), Some("us"), "G1");
        for (rule, geo) in [("G2", Some("global")), ("G3", None)] {
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

    #[tokio::test]
    async fn advisor_pairing_and_projection_follow_the_official_matrix() {
        // Cause/effect graph: Managed advisor DTO -> canonical roster validation
        // -> official executor/advisor compatibility matrix -> exact advisor
        // publication. Projection always places the reserved advisor last.
        //
        // Decision table:
        // | Rule | executor     | advisor      | effect                       |
        // | D1   | sonnet-5    | opus-5      | publish, advisor projected    |
        // | D2   | opus-4-6    | opus-4-6    | supported matrix edge         |
        // | D3   | opus-5      | opus-4-8    | reject before Agent mutation  |
        // | D4   | unknown     | opus-5      | reject before Agent mutation  |
        assert!(
            advisor_pair_supported("claude-opus-4-6", "claude-opus-4-6"),
            "D2"
        );
        assert!(
            !advisor_pair_supported("claude-opus-5", "claude-opus-4-8"),
            "D3"
        );
        assert!(!advisor_pair_supported("unknown", "claude-opus-5"), "D4");

        let temp = tempfile::tempdir().unwrap();
        let repository = ConfigPlaneManagedAgentRepository::new(
            plane(temp.path().join("config.sqlite").to_str().unwrap()),
            "workspace-a",
        );
        let mut accepted = create_params("advisor-compatible");
        accepted.model = ModelInput::Id("claude-sonnet-5".into());
        accepted.multiagent = Some(
            serde_json::from_value(json!({
                "type": "coordinator",
                "agents": [{"type":"advisor", "model":"claude-opus-5"}]
            }))
            .unwrap(),
        );
        let created = repository
            .create("workspace-a", accepted)
            .await
            .expect("D1");
        let projected = serde_json::to_value(created.multiagent).unwrap();
        assert_eq!(projected["agents"][0]["type"], "advisor", "D1");
        assert_eq!(projected["agents"][0]["model"], "claude-opus-5", "D1");

        let mut rejected = create_params("advisor-incompatible");
        rejected.model = ModelInput::Id("claude-opus-5".into());
        rejected.multiagent = Some(
            serde_json::from_value(json!({
                "type": "coordinator",
                "agents": [{"type":"advisor", "model":"claude-opus-4-8"}]
            }))
            .unwrap(),
        );
        assert!(
            matches!(
                repository.create("workspace-a", rejected).await,
                Err(ManagedAgentError::Invalid(ref message))
                    if message.contains("unsupported advisor model pairing")
            ),
            "D3"
        );
    }

    #[tokio::test]
    async fn model_update_preserves_acp_configuration_only_for_an_explicit_acp_executor() {
        // Cause/effect graph: a Managed update may repeat the current model id
        // while omitting x_awaken.acp. The durable ModelSelection carries a
        // configuration field for both native and ACP targets, but only an
        // explicit acp:<cli> executor gives that field ACP semantics.
        //
        // Decision table:
        // | rule | current executor | same model | extension omitted | effect |
        // | U1   | native           | yes        | yes               | update succeeds; no ACP attachment |
        // | U2   | acp:<cli>        | yes        | yes               | prior ACP mode/options preserved |
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
        acp_params.model = serde_json::from_value(json!({
            "id": "acp:codex/gpt-5",
            "x_awaken": {"acp": {"mode": "plan", "options": {"reasoning_effort": "high"}}}
        }))
        .unwrap();
        let acp_config = config_from_create("agent_acp".into(), acp_params).expect("U2");
        let incoming = ModelInput::Id("acp:codex/gpt-5".into()).into_config();
        let preserved =
            acp_configuration_to_preserve(&acp_config, &incoming.id, incoming.x_awaken.is_none())
                .expect("U2");
        assert_eq!(preserved.mode.as_deref(), Some("plan"), "U2");
        assert_eq!(preserved.options["reasoning_effort"], "high", "U2");
    }

    #[tokio::test]
    async fn managed_agent_namespaced_step_budget_is_validated_and_versioned() {
        // Cause/effect graph: namespaced Managed controls project onto existing
        // AgentConfig max-step and plugin sources of truth. Omission selects or
        // preserves defaults; explicit values replace them; zero steps would
        // eliminate the first inference and must fail admission.
        //
        // | rule | operation | extension | effect |
        // | S1 | create | omitted | stores 8; response omits extension |
        // | S2 | create | 40 + state machine | stores and returns both |
        // | S3 | update | omitted | preserves 40 + state machine |
        // | S4 | update | 24, machine omitted | versions 24; preserves machine |
        // | S5 | create | 0 | rejects before persistence |
        let temp = tempfile::tempdir().unwrap();
        let repository = ConfigPlaneManagedAgentRepository::new(
            plane(temp.path().join("config.sqlite").to_str().unwrap()),
            "workspace-a",
        );

        let default = repository
            .create("workspace-a", create_params("default"))
            .await
            .expect("S1");
        assert!(default.x_awaken.is_none(), "S1");

        let mut configured_params = create_params("configured");
        configured_params.x_awaken = Some(AwakenAgentExtensions {
            max_steps: Some(40),
            state_machine: Some(json!({
                "machines": [{
                    "name": "submit",
                    "scope": "run",
                    "key": "",
                    "initial": "pending",
                    "terminal": ["done"],
                    "transitions": [
                        {"on":{"event":"step.after_inference"},"from":"pending","to":"pending"},
                        {"on":"design_submit_artifact","from":"pending","to":"done","when":"success"}
                    ]
                }],
                "continuation": {"max_continuations": 2}
            })),
        });
        let configured = repository
            .create("workspace-a", configured_params)
            .await
            .expect("S2");
        let configured_extensions = configured.x_awaken.unwrap();
        assert_eq!(configured_extensions.max_steps, Some(40), "S2");
        assert!(configured_extensions.state_machine.is_some(), "S2");

        let preserved = repository
            .update(
                "workspace-a",
                &configured.id,
                update_params(configured.version),
            )
            .await
            .expect("S3");
        let preserved_extensions = preserved.x_awaken.unwrap();
        assert_eq!(preserved_extensions.max_steps, Some(40), "S3");
        assert!(preserved_extensions.state_machine.is_some(), "S3");

        let mut replacement = update_params(preserved.version);
        replacement.x_awaken = Some(AwakenAgentExtensions {
            max_steps: Some(24),
            state_machine: None,
        });
        let replaced = repository
            .update("workspace-a", &configured.id, replacement)
            .await
            .expect("S4");
        let replaced_extensions = replaced.x_awaken.unwrap();
        assert_eq!(replaced_extensions.max_steps, Some(24), "S4");
        assert!(replaced_extensions.state_machine.is_some(), "S4");

        let mut invalid = create_params("invalid");
        invalid.x_awaken = Some(AwakenAgentExtensions {
            max_steps: Some(0),
            state_machine: None,
        });
        assert!(
            matches!(
                repository.create("workspace-a", invalid).await,
                Err(ManagedAgentError::Invalid(message)) if message.contains("max_steps")
            ),
            "S5"
        );
    }

    #[test]
    fn managed_model_extension_configures_only_the_selected_acp_executor() {
        // Causes: C1 model id selects explicit ACP/native; C2 x_awaken.acp is
        // absent/present. Effects: E1 omitted extension preserves the official
        // Managed shape; E2 explicit ACP stores native mode/options on the same
        // ModelSelection; E3 native+Acp extension fails at the ACL.
        //
        // | Rule | executor | extension | Effect |
        // | X1   | ACP      | present   | E2     |
        // | X2   | native   | present   | E3     |
        let configured = |id: &str| {
            let mut params = create_params("configured");
            params.model = serde_json::from_value(json!({
                "id": id,
                "x_awaken": {"acp": {
                    "mode": "plan",
                    "options": {"reasoning_effort": "high"}
                }}
            }))
            .unwrap();
            config_from_create("agent_configured".into(), params)
        };
        let config = configured("acp:codex/gpt-5").expect("X1");
        let acp = config.model_binding.acp_configuration().expect("X1");
        assert_eq!(acp.mode.as_deref(), Some("plan"), "X1");
        assert_eq!(acp.options["reasoning_effort"], "high", "X1");
        assert!(configured("gpt-5").is_err(), "X2");
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
