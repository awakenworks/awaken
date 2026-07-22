//! ACL between the Managed Agent wire API and the durable configuration plane.
//!
//! This adapter owns only representation mapping and lifecycle orchestration. The
//! ConfigPlane remains the single authoring source of truth; IAM policy stays in
//! the HTTP edge and never enters either repository.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use awaken_config_service::{ConfigPlane, RESERVED_ADMIN_SCOPE};
use awaken_config_store::{AgentConfig, AgentConfigRevision, ConfigWrite, ModelSelection};
use awaken_protocol_managed::types::ModelConfig;
use awaken_protocol_managed::types::agent::{Agent, AgentCreateParams, AgentUpdateParams};
use awaken_protocol_managed::{ManagedAgentError, ManagedAgentRepository};
use awaken_tenancy::ScopeId;
use serde_json::{Value, json};
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
            let binding = snapshot.resolved_spec.model_binding;
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
}

fn tool_id(value: &Value) -> Option<String> {
    match value {
        Value::String(id) => Some(id.clone()),
        Value::Object(object) => object
            .get("id")
            .or_else(|| object.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

fn config_from_create(id: String, params: AgentCreateParams) -> AgentConfig {
    let model = params.model.into_config();
    AgentConfig {
        id,
        instructions: params.system.unwrap_or_default(),
        max_steps: 8,
        delegation_limits: Default::default(),
        model_binding: ModelSelection::pinned("", model.id, ""),
        tool_ids: params.tools.iter().filter_map(tool_id).collect(),
        plugin_ids: Vec::new(),
        plugin_config: BTreeMap::new(),
        context_policy: Default::default(),
        tool_patterns: Vec::new(),
        model_candidates: Vec::new(),
        name: Some(params.name),
        description: params.description,
        metadata: params.metadata,
        mcp_servers: params.mcp_servers,
        skills: params.skills,
        multiagent: params.multiagent.filter(|value| !value.is_null()),
        archived_at: None,
        tool_overrides: Vec::new(),
        recovery_policies: BTreeMap::new(),
        compaction: None,
    }
}

fn wire_tools(ids: &[String]) -> Vec<Value> {
    ids.iter()
        .map(|id| json!({ "type": "custom", "name": id }))
        .collect()
}

fn project(revision: AgentConfigRevision) -> Agent {
    let config = revision.config;
    let id = config.id.clone();
    let model = config
        .model_binding
        .resolved()
        .map(|binding| binding.model_ref.clone())
        .unwrap_or_default();
    Agent {
        id: id.clone(),
        object_type: "agent",
        archived_at: config.archived_at,
        created_at: OBJECT_AT.to_string(),
        updated_at: OBJECT_AT.to_string(),
        name: config.name.unwrap_or(id),
        description: config.description,
        model: ModelConfig::new(model),
        system: (!config.instructions.is_empty()).then_some(config.instructions),
        metadata: config.metadata,
        mcp_servers: config.mcp_servers,
        skills: config.skills,
        tools: wire_tools(&config.tool_ids),
        multiagent: config.multiagent,
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
        let config = config_from_create(id.clone(), params);
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

    async fn retrieve(&self, workspace_id: &str, id: &str) -> Result<Agent, ManagedAgentError> {
        self.versioned_for_read(workspace_id, id)
            .await?
            .map(|revision| self.project_current(workspace_id, revision))
            .ok_or(ManagedAgentError::NotFound)
    }

    async fn list(&self, workspace_id: &str) -> Result<Vec<Agent>, ManagedAgentError> {
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
            agents.push(self.project_current(workspace_id, versioned));
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
        if current.revision != params.version {
            return Err(ManagedAgentError::Conflict(format!(
                "version mismatch: expected {}, got {}",
                current.revision, params.version
            )));
        }
        if current.config.archived_at.is_some() {
            return Err(ManagedAgentError::Invalid(
                "archived Agent cannot be updated".into(),
            ));
        }
        let mut config = current.config;
        if let Some(name) = params.name {
            config.name = Some(name);
        }
        if let Some(model) = params.model {
            config.model_binding = ModelSelection::pinned("", model.into_config().id, "");
        }
        if let Some(description) = params.description {
            config.description = Some(description);
        }
        if let Some(system) = params.system {
            config.instructions = system;
        }
        if let Some(metadata) = params.metadata {
            config.metadata = metadata;
        }
        if let Some(mcp_servers) = params.mcp_servers {
            config.mcp_servers = mcp_servers;
        }
        if let Some(skills) = params.skills {
            config.skills = skills;
        }
        if let Some(tools) = params.tools {
            config.tool_ids = tools.iter().filter_map(tool_id).collect();
        }
        if let Some(multiagent) = params.multiagent {
            config.multiagent = Some(multiagent).filter(|value| !value.is_null());
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
        if current.config.archived_at.is_some() {
            return Ok(project(current));
        }
        let mut config = current.config;
        let milliseconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or_default();
        config.archived_at = Some(awaken_protocol_managed::cron::to_rfc3339(milliseconds));
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

    use awaken_config_service::{ConfigService, StaticToolCatalog};
    use awaken_config_store::SqliteConfigStore;
    use awaken_protocol_managed::types::agent::{AgentCreateParams, AgentUpdateParams, ModelInput};

    use super::*;

    fn create_params(name: &str) -> AgentCreateParams {
        AgentCreateParams {
            name: name.into(),
            model: ModelInput::Id("model-a".into()),
            description: None,
            system: Some("be helpful".into()),
            metadata: BTreeMap::new(),
            mcp_servers: vec![json!({
                "type": "url",
                "name": "docs",
                "url": "https://mcp.example.test"
            })],
            skills: vec![json!({ "id": "skill-docs" })],
            tools: Vec::new(),
            multiagent: None,
        }
    }

    fn update_params(version: u64) -> AgentUpdateParams {
        AgentUpdateParams {
            version,
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
            Arc::new(ConfigService::new()),
            Arc::new(SqliteConfigStore::open(path).expect("config store")),
            Arc::new(StaticToolCatalog(Vec::new())),
        )
    }

    #[tokio::test]
    async fn managed_agent_is_executable_and_archive_uninstalls_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.sqlite");
        let plane = plane(path.to_str().unwrap());
        let repository = ConfigPlaneManagedAgentRepository::new(plane.clone(), "workspace-a");

        let created = repository
            .create("workspace-a", create_params("assistant"))
            .await
            .unwrap();
        assert_eq!(created.version, 1);
        assert_eq!(created.mcp_servers[0]["name"], "docs");
        assert_eq!(created.skills[0]["id"], "skill-docs");
        assert!(
            plane
                .service()
                .installed_in("workspace-a", &created.id)
                .is_some()
        );

        let archived = repository
            .archive("workspace-a", &created.id)
            .await
            .unwrap();
        assert_eq!(archived.version, 2);
        assert!(archived.archived_at.is_some());
        assert!(
            plane
                .service()
                .installed_in("workspace-a", &created.id)
                .is_none()
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
        let current = repository.retrieve("workspace-a", &id).await.unwrap();
        assert_eq!(current.name, "renamed");
        assert_eq!(current.version, 2);
        let versions = repository.versions("workspace-a", &id).await.unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].name, "assistant");
        assert_eq!(versions[1].name, "renamed");
        assert!(matches!(
            repository.retrieve("workspace-b", &id).await,
            Err(ManagedAgentError::NotFound)
        ));
        assert!(matches!(
            repository.versions("workspace-b", &id).await,
            Err(ManagedAgentError::NotFound)
        ));
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
                    awaken_admin_assistant::ADMIN_ASSISTANT_AGENT_ID
                )
                .await,
            Err(ManagedAgentError::NotFound)
        ));
    }
}
