//! Protocol-neutral Session command/query surface.
//!
//! The application owns access to Runtime and the aggregate repository. Public
//! adapters invoke these use-case methods and never reach through to either
//! collaborator, preventing a second orchestration boundary in protocol state.

use std::sync::Arc;

use awaken_agent_contract::RedactedString;
use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::stream::sink::Sink;
use awaken_agent_contract::thread::read::lifecycle::{RunLifecycleCursor, RunLifecyclePage};
use awaken_session_contract::{
    AgentCapabilities, DelegatedRun, OutcomeReport, Pending, PersistedSession,
    ResolvedSkillBinding, RunError, SessionRepositoryError, SessionUsage, StepOutcome,
    ToolPermissionDecision,
};

/// Application command payload for a Repository attached as a Session input.
pub struct SessionRepositoryResourceInput {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    pub description: String,
    pub remote_url: String,
    pub authorization_token: Option<RedactedString>,
    pub initial_branch: Option<String>,
    pub initial_commit: Option<String>,
}

use crate::SessionApplication;

impl SessionApplication {
    pub async fn session(
        &self,
        session_id: &str,
    ) -> Result<PersistedSession, SessionRepositoryError> {
        self.sessions_repo.get(session_id).await
    }

    #[must_use]
    pub fn session_profile(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<awaken_executable_agent_contract::ExecutableAgentSessionProfile> {
        self.config_source
            .as_ref()
            .and_then(|source| source.session_profile_in(workspace_id, agent_id))
    }

    #[must_use]
    pub fn agent_unavailable(&self, workspace_id: &str, agent_id: &str) -> bool {
        self.config_source
            .as_ref()
            .is_some_and(|source| source.agent_unavailable_in(workspace_id, agent_id))
    }

    pub fn validate_session_skill_total(
        &self,
        workspace_id: &str,
        root_agent_id: &str,
        root_view: Option<&awaken_executable_agent_contract::ExecutableAgentSessionProfile>,
        root_skills: &[awaken_agent_contract::AgentSkillBinding],
    ) -> Result<(), RunError> {
        const MAX_SESSION_SKILLS: usize = 500;
        let mut total = root_skills.len();
        if total > MAX_SESSION_SKILLS {
            return Err(RunError::bad_request(
                "a session supports at most 500 skills across all agents",
            ));
        }
        let mut seen = std::collections::BTreeSet::from([root_agent_id.to_string()]);
        let mut pending = root_view
            .into_iter()
            .flat_map(|view| view.delegate_ids.iter().cloned())
            .collect::<std::collections::VecDeque<_>>();
        while let Some(agent_id) = pending.pop_front() {
            if !seen.insert(agent_id.clone()) {
                continue;
            }
            let Some(view) = self.session_profile(workspace_id, &agent_id) else {
                continue;
            };
            total = total.checked_add(view.skills.len()).ok_or_else(|| {
                RunError::bad_request("a session supports at most 500 skills across all agents")
            })?;
            if total > MAX_SESSION_SKILLS {
                return Err(RunError::bad_request(
                    "a session supports at most 500 skills across all agents",
                ));
            }
            pending.extend(view.delegate_ids);
        }
        Ok(())
    }

    pub async fn missing_vault(&self, vault_ids: &[String]) -> Result<Option<String>, RunError> {
        let Some(source) = &self.credential_source else {
            return Ok(None);
        };
        for vault_id in vault_ids {
            let exists = source.has_vault(vault_id).await.map_err(|error| {
                RunError::unavailable(format!("credential authority unavailable: {error}"))
            })?;
            if !exists {
                return Ok(Some(vault_id.clone()));
            }
        }
        Ok(None)
    }

    pub fn resolve_session_inputs(
        &self,
        workspace_id: &str,
        agent_defaults: &[awaken_resource_contract::InputBinding],
        session_inputs: &[awaken_session_contract::SessionInputAttachment],
    ) -> Result<awaken_session_contract::ResolvedSessionResources, RunError> {
        awaken_session_contract::SessionInputResolver::resolve_inputs(
            workspace_id,
            self.resource_catalog
                .as_deref()
                .map(|catalog| catalog as &dyn awaken_resource_contract::ResourceConfigSource),
            agent_defaults,
            session_inputs,
        )
        .map_err(|error| RunError::bad_request(error.to_string()))
    }

    pub fn session_memory_store(
        &self,
        workspace_id: &str,
        memory_store_id: &str,
    ) -> Result<awaken_resource_contract::MemoryStoreDefinition, RunError> {
        self.resource_catalog
            .as_ref()
            .ok_or_else(|| {
                RunError::bad_request("memory resources require a configured Resource Catalog")
            })?
            .memory_store(workspace_id, memory_store_id)
            .map_err(|error| RunError::bad_request(error.to_string()))?
            .ok_or_else(|| {
                RunError::bad_request(format!(
                    "MemoryStore `{memory_store_id}` not found in this workspace"
                ))
            })
    }

    pub async fn configure_session_repository(
        &self,
        input: SessionRepositoryResourceInput,
    ) -> Result<awaken_resource_contract::RepositoryId, RunError> {
        let catalog = self.resource_catalog.as_ref().ok_or_else(|| {
            RunError::bad_request("repository resources require a configured Resource Catalog")
        })?;
        let credential_binding = match input.authorization_token {
            Some(token) => {
                let ingress = self.repository_credential_ingress.as_ref().ok_or_else(|| {
                    RunError::bad_request(
                        "repository authorization requires a configured credential Vault",
                    )
                })?;
                Some(
                    ingress
                        .enter_repository_token(
                            awaken_credential_contract::CredentialSourceId(format!(
                                "{}:credential",
                                input.id
                            )),
                            &input.workspace_id,
                            token,
                        )
                        .await
                        .map_err(|error| {
                            RunError::bad_request(format!(
                                "repository authorization could not be sealed: {error}"
                            ))
                        })?
                        .0,
                )
            }
            None => None,
        };
        catalog
            .create_repository(
                awaken_resource_contract::RepositoryDefinition {
                    id: input.id.clone().into(),
                    workspace_id: input.workspace_id,
                    name: input.name,
                    description: input.description,
                    metadata: Default::default(),
                    state: awaken_resource_contract::ResourceState::Active,
                    current_config_version: awaken_resource_contract::ConfigVersion::INITIAL,
                    timestamps: Default::default(),
                },
                awaken_resource_contract::RepositoryConfigVersion {
                    repository_id: input.id.clone().into(),
                    version: awaken_resource_contract::ConfigVersion::INITIAL,
                    remote_url: input.remote_url,
                    credential_binding,
                    initial_branch: input.initial_branch,
                    initial_commit: input.initial_commit,
                    clone_policy: awaken_resource_contract::ClonePolicy::default(),
                },
            )
            .map_err(|error| {
                RunError::bad_request(format!(
                    "repository resource could not be configured: {error}"
                ))
            })?;
        Ok(awaken_resource_contract::RepositoryId::from(input.id))
    }

    pub async fn emit_lifecycle_fact(
        &self,
        fact_id: &str,
        session_id: &str,
        workspace_id: Option<&str>,
        event_type: &str,
    ) {
        if let Some(sink) = &self.lifecycle_sink {
            sink.emit_fact(fact_id, session_id, workspace_id, event_type)
                .await;
        }
    }

    pub async fn runtime_owns_thread(&self, thread: &str) -> Result<bool, RunError> {
        self.runtime.owns_thread(thread).await
    }

    pub async fn committed_messages(&self, thread: &str) -> Result<Vec<Message>, RunError> {
        self.runtime.committed_messages(thread).await
    }

    pub async fn committed_run_lifecycle(
        &self,
        thread: &str,
        cursor: RunLifecycleCursor,
        limit: usize,
    ) -> Result<RunLifecyclePage, RunError> {
        self.runtime
            .committed_run_lifecycle(thread, cursor, limit)
            .await
    }

    pub async fn pending_tool(&self, thread: &str) -> Result<Option<Pending>, RunError> {
        self.runtime.pending_tool(thread).await
    }

    pub async fn delegated_runs(&self, thread: &str) -> Result<Vec<DelegatedRun>, RunError> {
        self.runtime.delegated_runs(thread).await
    }

    pub async fn resolve_session_skills(
        &self,
        workspace_id: &str,
        skills: &[awaken_agent_contract::AgentSkillBinding],
    ) -> Result<Vec<ResolvedSkillBinding>, RunError> {
        self.runtime
            .resolve_session_skills(workspace_id, skills)
            .await
    }

    pub async fn rebind_model(&self, thread: &str, model: &str) -> Result<(), RunError> {
        self.runtime.rebind_model(thread, model).await
    }

    pub async fn run_streaming_attributed(
        &self,
        agent: &str,
        thread: &str,
        content: Vec<ContentBlock>,
        data_subject_id: Option<String>,
        sink: Arc<dyn Sink>,
    ) -> Result<StepOutcome, RunError> {
        self.runtime
            .run_streaming_attributed(agent, thread, content, data_subject_id, sink)
            .await
    }

    pub async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        decision: ToolPermissionDecision,
    ) -> Result<StepOutcome, RunError> {
        self.runtime.resume(thread, tool_use_id, decision).await
    }

    pub async fn resume_custom(
        &self,
        thread: &str,
        tool_use_id: &str,
        content: Vec<ContentBlock>,
        is_error: bool,
    ) -> Result<StepOutcome, RunError> {
        self.runtime
            .resume_custom(thread, tool_use_id, content, is_error)
            .await
    }

    pub async fn add_system(&self, thread: &str, text: &str) -> Result<(), RunError> {
        self.runtime.add_system(thread, text).await
    }

    pub async fn interrupt(&self, thread: &str) -> Result<(), RunError> {
        self.runtime.interrupt(thread).await
    }

    pub async fn define_outcome(
        &self,
        thread: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<OutcomeReport, RunError> {
        self.runtime
            .define_outcome(thread, description, rubric, max_iterations)
            .await
    }

    pub async fn supports_mid_conversation_system(&self, thread: &str) -> bool {
        self.runtime.supports_mid_conversation_system(thread).await
    }

    pub async fn session_usage(&self, thread: &str) -> Result<SessionUsage, RunError> {
        self.runtime.session_usage(thread).await
    }

    pub async fn end_runtime_session(&self, thread: &str) -> Result<(), RunError> {
        self.runtime.end_session(thread).await
    }

    #[must_use]
    pub fn model(&self) -> String {
        self.runtime.model()
    }

    #[must_use]
    pub fn capabilities_for(&self, thread: &str) -> AgentCapabilities {
        self.runtime.capabilities_for(thread)
    }
}
