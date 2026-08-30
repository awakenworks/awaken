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
    MultiagentRosterEntry, managed_advisor_pair_supported,
};
use awaken_protocol_managed::{ManagedAgentError, ManagedAgentRepository};
use awaken_runtime_contract::agent_bindings::AgentMcpServerBinding;
use awaken_runtime_contract::agent_bindings::ToolsetPolicy;
use awaken_runtime_contract::resolved::ToolDescriptor;
use awaken_session_contract::{
    AgentTool, CustomToolInputSchema, preserve_runtime_agent_overrides, resolved_toolsets,
    toolset_policies, validate_agent_tools,
};
use awaken_tenancy::ScopeId;

mod lifecycle_identity;
mod projection;
use lifecycle_identity::{lifecycle_timestamp, new_agent_id};
use projection::{
    acp_configuration_to_preserve, client_tools, config_from_create, project, register_roster_name,
    typed_mcp_servers, typed_multiagent, typed_skills, validate_managed_agent_config,
};

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
            if current.config.lifecycle() != AgentLifecycle::Published
                || self
                    .publication_for_read(workspace_id, id, current.revision)
                    .await?
                    .is_some()
            {
                return Ok(Some(current));
            }

            // The config plane owns drafts independently from Managed Agent
            // publications. Saving draft rN must not hide the latest published
            // rN-1 from SDK clients or promote rN through a read path. Reuse the
            // immutable publication index as the sole visibility authority.
            let mut revisions = self
                .plane
                .list_revisions(&Self::scope(workspace_id), id)
                .await
                .map_err(ManagedAgentError::Storage)?;
            revisions.sort_by_key(|revision| revision.revision);
            for revision in revisions.into_iter().rev() {
                if revision.config.lifecycle() == AgentLifecycle::Published
                    && self
                        .publication_for_read(workspace_id, id, revision.revision)
                        .await?
                        .is_some()
                {
                    return Ok(Some(revision));
                }
            }
            return Ok(None);
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
            let binding = publication
                .snapshot
                .resolved_spec
                .model_binding
                .binding()
                .clone();
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
        let coordinator_name = config.name.clone().unwrap_or_else(|| config.id.clone());
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
            if !managed_advisor_pair_supported(&executor_model, advisor_model) {
                return Err(ManagedAgentError::Invalid(format!(
                    "unsupported advisor model pairing: executor `{executor_model}`, advisor `{advisor_model}`"
                )));
            }
        }
        let mut roster_names = BTreeMap::<String, String>::new();
        for target in &mut multiagent.agents {
            if target.is_self_reference() {
                register_roster_name(&mut roster_names, &config.id, &coordinator_name)?;
                continue;
            }
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
            register_roster_name(
                &mut roster_names,
                id,
                selected.config.name.as_deref().unwrap_or(id),
            )?;
            *version = Some(selected.revision);
        }
        Ok(())
    }
}

/// Validate the exact callable names resolved from frozen Agent revisions.
/// Identity-only validation belongs to `MultiagentConfig`; only this repository
/// boundary can see the names of every referenced publication without looking
/// them up twice or trusting a protocol projection.
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
            let Some(versioned) = self.versioned_for_read(workspace_id, &config.id).await? else {
                continue;
            };
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
            validate_agent_tools(&tools).map_err(ManagedAgentError::Invalid)?;
            config.tool_ids.clear();
            let mut replacement = toolset_policies(&tools);
            preserve_runtime_agent_overrides(&current.config.toolsets, &mut replacement);
            config.toolsets = replacement;
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
            self.plane
                .withdraw(workspace_id, id, current.revision)
                .await
                .map_err(ManagedAgentError::Storage)?;
            return Ok(project(current));
        }
        let mut config = current.config;
        config.disabled_at = None;
        config.archived_at = Some(lifecycle_timestamp());
        match self
            .plane
            .archive_if_revision(&scope, &config, current.revision)
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
        let latest_revision = revisions.iter().map(|revision| revision.revision).max();
        let mut visible = Vec::with_capacity(revisions.len());
        for revision in revisions {
            let terminal_current = Some(revision.revision) == latest_revision
                && revision.config.lifecycle() != AgentLifecycle::Published;
            let published = revision.config.lifecycle() == AgentLifecycle::Published
                && self
                    .publication_for_read(workspace_id, id, revision.revision)
                    .await?
                    .is_some();
            if terminal_current || published {
                visible.push(self.project_current(workspace_id, revision).await?);
            }
        }
        if visible.is_empty() {
            return Err(ManagedAgentError::NotFound);
        }
        Ok(visible)
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
        InferenceGeography, InferenceOptions, InferenceSpeed, ReasoningEffort, ToolExecutionPolicy,
        ToolPermissionRequirement, ToolPolicyOverride, ToolsetPolicy, ToolsetSource,
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
                            target.provider_id.as_deref().unwrap_or("test-provider"),
                            &target.model_id,
                            backend_ref,
                        )
                    })
                })
                .ok_or_else(|| "test requires a pinned model".to_string())?;
            let resolved = |binding: ModelBinding| {
                let model = binding.model_ref.clone();
                ResolvedModelCandidate::try_provider(
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
                primary: resolved(primary).map_err(|error| error.to_string())?,
                candidates: candidates
                    .iter()
                    .cloned()
                    .map(resolved)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| error.to_string())?,
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

    #[test]
    fn resolved_roster_callable_names_follow_one_decision_table() {
        // Cause/effect graph: C1 first ordinary name -> E1 admit; C2 another id
        // with the same normalized name -> E2 reject ambiguity; C3 reserved
        // `self`/`anthropic.advisor` -> E3 reject; C4 blank -> E4 reject.
        // Constraints/invariants: case and surrounding whitespace normalize
        // before the sole uniqueness/reserved-name decision. Decision rules:
        // R1=C1=>E1; R2=C2=>E2; R3=C3=>E3; R4=C4=>E4.
        let mut seen = BTreeMap::new();
        register_roster_name(&mut seen, "agent-a", " Researcher ").expect("R1 admit");
        assert!(
            register_roster_name(&mut seen, "agent-b", "researcher").is_err(),
            "R2 duplicate callable name"
        );
        assert!(
            register_roster_name(&mut BTreeMap::new(), "agent-self", "SELF").is_err(),
            "R3 self is reserved"
        );
        assert!(
            register_roster_name(&mut BTreeMap::new(), "agent-advisor", "Anthropic.Advisor")
                .is_err(),
            "R3 advisor is reserved"
        );
        assert!(
            register_roster_name(&mut BTreeMap::new(), "agent-empty", " \t").is_err(),
            "R4 blank is not callable"
        );
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

    fn plane_with_delegation(path: &str) -> ConfigPlane {
        plane_with_delegation_and_catalog(path).0
    }

    fn plane_with_delegation_and_catalog(path: &str) -> (ConfigPlane, Arc<ExecutableAgentCatalog>) {
        let catalog = Arc::new(ExecutableAgentCatalog::new());
        (
            ConfigPlane::new(
                Arc::new(ConfigService::new(
                    Arc::new(TestModelResolver),
                    Arc::new(LocalExecutableAgentRegistrar::new(catalog.clone())),
                )),
                Arc::new(SqliteConfigStore::open(path).expect("config store")),
                Arc::new(StaticToolCatalog(vec![
                    ToolDescriptor::pinned(
                        "managed",
                        "agent_run",
                        "Run an exact roster Agent",
                        json!({"type": "object"}),
                    )
                    .with_kind(ToolKind::AgentDelegation),
                ])),
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
    async fn managed_update_preserves_versioned_runtime_only_policy() {
        // Cause/effect table for the Managed repository caller of the shared codec:
        // | rule | current opaque | replacement wire | effect |
        // | M1   | agent_run ask  | closed tools     | exact opaque persists and publishes ask |
        // The retrieval projection omits the opaque member; only the versioned current
        // config can supply it to the revision-fenced update.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("managed-opaque.sqlite");
        let (plane, executable) = plane_with_delegation_and_catalog(path.to_str().unwrap());
        let repository = ConfigPlaneManagedAgentRepository::new(plane.clone(), "workspace-a");
        let created = repository
            .create("workspace-a", create_params("opaque"))
            .await
            .unwrap();
        let scope = ScopeId::from("workspace-a");
        let current = plane
            .get_versioned(&scope, &created.id)
            .await
            .unwrap()
            .unwrap();
        let runtime_only = ToolPolicyOverride::new(
            "agent_run",
            ToolExecutionPolicy {
                enabled: true,
                permission: ToolPermissionRequirement::AlwaysAsk,
            },
        );
        let current_revision = current.revision;
        let mut seeded = current.config;
        seeded.toolsets.push(ToolsetPolicy {
            source: ToolsetSource::Agent,
            default: ToolExecutionPolicy {
                enabled: false,
                permission: ToolPermissionRequirement::AlwaysAllow,
            },
            overrides: vec![runtime_only.clone()],
        });
        assert!(matches!(
            plane
                .put_if_revision(&scope, &seeded, current_revision)
                .await
                .unwrap(),
            ConfigWrite::Applied { revision: 2 }
        ));
        plane.publish(&scope, &created.id).await.unwrap();
        let retrieved = repository
            .retrieve("workspace-a", &created.id, None)
            .await
            .unwrap();
        assert!(
            !serde_json::to_string(&retrieved.tools)
                .unwrap()
                .contains("agent_run"),
            "M1 closed retrieval"
        );
        let updated = repository
            .update(
                "workspace-a",
                &created.id,
                AgentUpdateParams {
                    version: Some(retrieved.version),
                    name: None,
                    model: None,
                    description: None,
                    system: None,
                    metadata: None,
                    mcp_servers: None,
                    skills: None,
                    tools: Some(Some(retrieved.tools)),
                    multiagent: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.version, 3, "M1");
        let stored = plane
            .get_versioned(&scope, &created.id)
            .await
            .unwrap()
            .unwrap();
        let agent = stored
            .config
            .toolsets
            .iter()
            .find(|toolset| toolset.source == ToolsetSource::Agent)
            .unwrap();
        assert_eq!(
            agent
                .overrides
                .iter()
                .find(|entry| entry.name == "agent_run")
                .unwrap(),
            &runtime_only,
            "M1 exact opaque"
        );
        let runtime = executable
            .current("workspace-a", &created.id)
            .unwrap()
            .snapshot
            .resolved_spec
            .plugin_config
            .agent
            .tool_policy("agent_run")
            .unwrap();
        assert!(runtime.enabled, "M1");
        assert_eq!(
            runtime.permission,
            ToolPermissionRequirement::AlwaysAsk,
            "M1"
        );
    }

    #[tokio::test]
    async fn managed_agent_archive_retains_immutable_publication() {
        // Cause/effect graph:
        // C1 Published + archive -> E1 current execution becomes unavailable while
        // the immutable publication remains addressable; C2 Archived + archive ->
        // E2 idempotent; C3 archive CAS committed but withdrawal did not run ->
        // E3 retry replays the same revision-fenced withdrawal. Disable remains a native configuration-plane lifecycle
        // operation and is deliberately absent from the Managed SDK repository.
        //
        // Decision table:
        // | rule | current   | command | current executable | exact snapshot | result |
        // | L1   | Published | archive | no                 | yes            | archived |
        // | L2   | Archived  | archive | no                 | yes            | no new revision |
        // | L3   | Archived  | archive | stale current      | yes            | withdraw repaired, no new revision |
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

        let split = repository
            .create("workspace-a", create_params("split-archive"))
            .await
            .unwrap();
        let split_before = plane
            .get_versioned(&ScopeId::from("workspace-a"), &split.id)
            .await
            .unwrap()
            .unwrap();
        let mut split_archived = split_before.config;
        split_archived.archived_at = Some("2026-08-30T00:00:00Z".into());
        assert!(matches!(
            plane
                .archive_if_revision(
                    &ScopeId::from("workspace-a"),
                    &split_archived,
                    split_before.revision,
                )
                .await
                .unwrap(),
            ConfigWrite::Applied { revision: 2 }
        ));
        assert!(
            catalog.current("workspace-a", &split.id).is_some(),
            "L3 split failure leaves the prior registration until retry"
        );
        let repaired = repository
            .archive("workspace-a", &split.id)
            .await
            .expect("L3 retry");
        assert_eq!(repaired.version, 2, "L3 no authoring revision");
        assert!(
            catalog.current("workspace-a", &split.id).is_none(),
            "L3/E3 retry repairs withdrawal"
        );
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
    async fn managed_reads_never_promote_or_hide_an_unpublished_config_draft() {
        // Cause/effect matrix for the shared config/publication aggregate:
        //
        // | current config | immutable publication | Managed current/list | versions | exact draft |
        // | r1             | r1                    | r1                   | r1       | n/a         |
        // | r2 draft       | r1                    | r1                   | r1       | 404         |
        // | r2             | r1,r2                 | r2                   | r1,r2    | r2          |
        //
        // A config save and a publication are deliberately separate commands.
        // The publication index, not the newest authoring row, therefore owns
        // every Managed Agent read. This closes current, list, versions, exact
        // version, and restart through one selection rule.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.sqlite");
        let (plane, _catalog) = plane_with_catalog(path.to_str().unwrap());
        let repository = ConfigPlaneManagedAgentRepository::new(plane.clone(), "workspace-a");
        let created = repository
            .create("workspace-a", create_params("published name"))
            .await
            .expect("published r1");
        let scope = ScopeId::from("workspace-a");
        let current = plane
            .get_versioned(&scope, &created.id)
            .await
            .unwrap()
            .expect("r1 config");
        let mut draft = current.config;
        draft.name = Some("unpublished draft".into());
        assert!(matches!(
            plane
                .put_if_revision(&scope, &draft, current.revision)
                .await
                .unwrap(),
            ConfigWrite::Applied { revision: 2 }
        ));

        let read = repository
            .retrieve("workspace-a", &created.id, None)
            .await
            .expect("latest published revision remains readable");
        assert_eq!((read.version, read.name.as_str()), (1, "published name"));
        let listed = repository
            .list("workspace-a", &AgentListParams::default())
            .await
            .expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(
            (listed[0].version, listed[0].name.as_str()),
            (1, "published name")
        );
        let versions = repository
            .versions("workspace-a", &created.id)
            .await
            .expect("versions");
        assert_eq!(
            versions
                .iter()
                .map(|agent| agent.version)
                .collect::<Vec<_>>(),
            vec![1]
        );
        assert!(matches!(
            repository
                .retrieve("workspace-a", &created.id, Some(2))
                .await,
            Err(ManagedAgentError::NotFound)
        ));

        plane
            .publish(&scope, &created.id)
            .await
            .expect("publish r2");
        let read = repository
            .retrieve("workspace-a", &created.id, None)
            .await
            .expect("published r2");
        assert_eq!((read.version, read.name.as_str()), (2, "unpublished draft"));
        let versions = repository
            .versions("workspace-a", &created.id)
            .await
            .expect("published versions");
        assert_eq!(
            versions
                .iter()
                .map(|agent| agent.version)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(
            repository
                .retrieve("workspace-a", &created.id, Some(2))
                .await
                .expect("exact published r2")
                .name,
            "unpublished draft"
        );
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
        // Constraints/invariants: each exact published revision supplies its
        // frozen geography and mismatch admission has no Agent write side effect.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.sqlite");
        let plane = plane_with_delegation(path.to_str().unwrap());
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
        // Constraint/invariant: the wire enum is closed to the two official
        // entry tags; decoding cannot manufacture a repository lookup path.
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

    #[test]
    fn managed_multiagent_wire_projection_stably_places_the_advisor_last() {
        // Cause/effect graph: C1 ConfigPlane has no advisor; C2 it has an advisor
        // after ordinary entries; C3 it has an advisor before ordinary/self
        // entries. E1 preserves the authored ordinary/self relative order; E2
        // emits the advisor last; E3 the no-advisor roster is unchanged. This
        // single projection owner serves create/retrieve/update responses.
        //
        // | Rule | Advisor | Authored position | Effects |
        // |---|---|---|---|
        // | R1 | no | n/a | E1,E3 |
        // | R2 | yes | last | E1,E2 |
        // | R3 | yes | first | E1,E2 |
        // Constraints/invariants: projection never mutates ConfigPlane roster
        // truth and has exactly one owner across all Managed read/write surfaces.
        let project_roster = |agents| {
            let mut config =
                config_from_create("coordinator".into(), create_params("coordinator")).unwrap();
            config.multiagent = Some(MultiagentConfig { agents });
            let projected = project(AgentConfigRevision {
                revision: 7,
                config,
                created_at_unix_ms: Some(1_000),
                updated_at_unix_ms: Some(2_000),
            });
            let Some(WireMultiagent::Coordinator { agents }) = projected.multiagent else {
                panic!("projected coordinator roster")
            };
            agents
        };
        let ordinary = || MultiagentTarget::Agent {
            id: "researcher".into(),
            version: Some(3),
        };
        let advisor = || MultiagentTarget::Advisor {
            model: "claude-opus-5".into(),
        };

        let no_advisor = project_roster(vec![ordinary(), MultiagentTarget::SelfReference]);
        assert!(
            matches!(no_advisor.as_slice(), [MultiagentRosterEntry::Reference(first), MultiagentRosterEntry::Reference(second)] if first.id == "researcher" && second.id == "coordinator"),
            "R1/E1/E3"
        );
        for (rule, roster) in [
            (
                "R2",
                project_roster(vec![ordinary(), MultiagentTarget::SelfReference, advisor()]),
            ),
            (
                "R3",
                project_roster(vec![advisor(), ordinary(), MultiagentTarget::SelfReference]),
            ),
        ] {
            assert!(
                matches!(roster.as_slice(), [MultiagentRosterEntry::Reference(first), MultiagentRosterEntry::Reference(second), MultiagentRosterEntry::Advisor(_)] if first.id == "researcher" && second.id == "coordinator"),
                "{rule}/E1/E2"
            );
        }
    }

    #[tokio::test]
    async fn advisor_first_authoring_projects_last_across_agent_crud_and_restart() {
        // Cause/effect graph: C1 the authoritative ConfigPlane roster is authored
        // advisor-first; C2 it also contains an ordinary child then `self`; C3
        // create/update/retrieve/list read the current revision; C4 retrieve/list
        // run after process-local repository loss; C5 an officially invalid pair
        // reaches save admission. Effects: E1 ConfigPlane preserves C1/C2 exactly;
        // E2 every Managed Agent response keeps the two ordinary references in
        // order and moves the advisor last; E3 versions advance without creating
        // a second projection path; E4 C5 is rejected before an Agent write.
        //
        // | Rule | Source order | Surface | Restart | Effect |
        // |---|---|---|---|---|
        // | C1 | advisor,child,self | create | no | E1,E2 |
        // | C2 | same | retrieve/list | no | E2 |
        // | C3 | same | update | no | E1,E2,E3 |
        // | C4 | same | retrieve/list/version | yes | E2,E3 |
        // | C5 | invalid pair | create | no | E4 |
        // Constraints/invariants: ConfigPlane retains authored order, every wire
        // surface uses one projection, and restart cannot introduce cached truth.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.sqlite");
        let plane = plane_with_delegation(path.to_str().unwrap());
        let repository = ConfigPlaneManagedAgentRepository::new(plane.clone(), "workspace-a");

        let mut worker_params = create_params("researcher");
        worker_params.model = ModelInput::Id("claude-sonnet-4-6".into());
        let worker = repository
            .create("workspace-a", worker_params)
            .await
            .expect("ordinary child");

        let mut coordinator_params = create_params("coordinator");
        coordinator_params.model = ModelInput::Id("claude-sonnet-4-6".into());
        coordinator_params.multiagent = Some(
            serde_json::from_value(json!({
                "type": "coordinator",
                "agents": [
                    {"type":"advisor", "model":"claude-opus-5"},
                    {"type":"agent", "id":worker.id},
                    {"type":"self"}
                ]
            }))
            .unwrap(),
        );
        let created = repository
            .create("workspace-a", coordinator_params)
            .await
            .expect("C1");

        let assert_wire_order = |rule: &str, agent: &Agent| {
            let Some(WireMultiagent::Coordinator { agents }) = agent.multiagent.as_ref() else {
                panic!("{rule}: coordinator projection")
            };
            assert!(
                matches!(
                    agents.as_slice(),
                    [
                        MultiagentRosterEntry::Reference(child),
                        MultiagentRosterEntry::Reference(self_reference),
                        MultiagentRosterEntry::Advisor(advisor),
                    ] if child.id == worker.id
                        && self_reference.id == created.id
                        && advisor.model == "claude-opus-5"
                ),
                "{rule}/E2: ordinary references retain order and advisor is last"
            );
        };
        assert_wire_order("C1", &created);

        let authored = plane
            .get_versioned(&ScopeId::from("workspace-a"), &created.id)
            .await
            .unwrap()
            .expect("C1 authoritative config");
        assert!(
            matches!(
                authored.config.multiagent.as_ref().map(|value| value.agents.as_slice()),
                Some([
                    MultiagentTarget::Advisor { .. },
                    MultiagentTarget::Agent { id, .. },
                    MultiagentTarget::SelfReference,
                ]) if id == &worker.id
            ),
            "C1/E1: wire ordering does not mutate ConfigPlane truth"
        );
        assert_wire_order(
            "C2 retrieve",
            &repository
                .retrieve("workspace-a", &created.id, None)
                .await
                .unwrap(),
        );
        let listed = repository
            .list("workspace-a", &AgentListParams::default())
            .await
            .unwrap();
        assert_wire_order(
            "C2 list",
            listed
                .iter()
                .find(|agent| agent.id == created.id)
                .expect("listed coordinator"),
        );

        let updated = repository
            .update("workspace-a", &created.id, update_params(created.version))
            .await
            .expect("C3 update");
        assert_eq!(updated.version, 2, "C3/E3");
        assert_wire_order("C3 update", &updated);
        let authored = plane
            .get_versioned(&ScopeId::from("workspace-a"), &created.id)
            .await
            .unwrap()
            .expect("C3 authoritative config");
        assert!(
            matches!(
                authored
                    .config
                    .multiagent
                    .as_ref()
                    .map(|value| value.agents.first()),
                Some(Some(MultiagentTarget::Advisor { .. }))
            ),
            "C3/E1"
        );

        drop(repository);
        drop(plane);
        let cold = ConfigPlaneManagedAgentRepository::new(
            plane_with_delegation(path.to_str().unwrap()),
            "workspace-a",
        );
        assert_wire_order(
            "C4 cold retrieve",
            &cold
                .retrieve("workspace-a", &created.id, None)
                .await
                .unwrap(),
        );
        let cold_list = cold
            .list("workspace-a", &AgentListParams::default())
            .await
            .unwrap();
        assert_wire_order(
            "C4 cold list",
            cold_list
                .iter()
                .find(|agent| agent.id == created.id)
                .expect("cold listed coordinator"),
        );
        for version in cold.versions("workspace-a", &created.id).await.unwrap() {
            assert_wire_order("C4 cold version", &version);
        }

        let mut invalid = create_params("invalid-pair");
        invalid.model = ModelInput::Id("claude-sonnet-5".into());
        invalid.multiagent = Some(
            serde_json::from_value(json!({
                "type":"coordinator",
                "agents":[{"type":"advisor", "model":"claude-sonnet-5"}]
            }))
            .unwrap(),
        );
        assert!(
            matches!(
                cold.create("workspace-a", invalid).await,
                Err(ManagedAgentError::Invalid(ref message))
                    if message.contains("unsupported advisor model pairing")
            ),
            "C5/E4"
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
