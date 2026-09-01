//! Protocol-neutral Session command/query surface.
//!
//! The application owns access to Runtime and the aggregate repository. Public
//! adapters invoke these use-case methods and never reach through to either
//! collaborator, preventing a second orchestration boundary in protocol state.

use std::sync::Arc;

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::stream::sink::Sink;
use awaken_agent_contract::thread::read::lifecycle::{RunLifecycleCursor, RunLifecyclePage};
use awaken_session_contract::{
    AgentCapabilities, CommittedOutcomeProjection, DelegatedRun, OutcomeDrive, Pending,
    PersistedSession, ResolvedSkillBinding, RunError, SessionRepositoryError, SessionUsage,
    StepOutcome, ToolPermissionDecision,
};

use crate::{
    CredentialMaterialIngressCommand, CredentialMaterialIngressReceipt, CredentialMaterialInput,
    SessionParticipantProvenance,
};

const SESSION_REPOSITORY_OWNER_KIND: &str = "awaken.session_repository.owner_kind";
const SESSION_REPOSITORY_OWNER_SESSION: &str = "awaken.session_repository.session_id";

/// Closed owner namespace for a Repository definition created exclusively for
/// one Session. Platform and Agent-default Repository definitions never carry
/// this marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionRepositoryOwner {
    Managed { session_id: String },
    Profiled { session_id: String },
}

impl SessionRepositoryOwner {
    #[must_use]
    pub fn from_repository_id(repository_id: &str) -> Option<Self> {
        let (owner, suffix) = repository_id.rsplit_once(":repository:")?;
        if suffix.is_empty() {
            return None;
        }
        if let Some(session_id) = owner.strip_prefix("managed:")
            && !session_id.is_empty()
        {
            return Some(Self::managed(session_id));
        }
        owner
            .strip_prefix("profiled:")
            .filter(|session_id| !session_id.is_empty())
            .map(Self::profiled)
    }

    #[must_use]
    pub fn managed(session_id: impl Into<String>) -> Self {
        Self::Managed {
            session_id: session_id.into(),
        }
    }

    #[must_use]
    pub fn profiled(session_id: impl Into<String>) -> Self {
        Self::Profiled {
            session_id: session_id.into(),
        }
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        match self {
            Self::Managed { session_id } | Self::Profiled { session_id } => session_id,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Managed { .. } => "managed",
            Self::Profiled { .. } => "profiled",
        }
    }

    #[must_use]
    pub fn owns_repository_id(&self, repository_id: &str) -> bool {
        repository_id
            .strip_prefix(&format!(
                "{}:{}:repository:",
                self.kind(),
                self.session_id()
            ))
            .is_some_and(|suffix| !suffix.is_empty())
    }

    #[must_use]
    pub(crate) fn marker(&self) -> std::collections::BTreeMap<String, String> {
        std::collections::BTreeMap::from([
            (SESSION_REPOSITORY_OWNER_KIND.into(), self.kind().into()),
            (
                SESSION_REPOSITORY_OWNER_SESSION.into(),
                self.session_id().into(),
            ),
        ])
    }

    #[must_use]
    pub(crate) fn matches_definition(
        &self,
        definition: &awaken_resource_contract::RepositoryDefinition,
    ) -> bool {
        self.owns_repository_id(definition.id.as_str())
            && definition
                .metadata
                .get(SESSION_REPOSITORY_OWNER_KIND)
                .map(String::as_str)
                == Some(self.kind())
            && definition
                .metadata
                .get(SESSION_REPOSITORY_OWNER_SESSION)
                .map(String::as_str)
                == Some(self.session_id())
    }
}

/// Application command payload for a Repository attached as a Session input.
pub struct SessionRepositoryResourceInput {
    pub id: String,
    pub workspace_id: String,
    pub name: String,
    pub description: String,
    pub remote_url: String,
    pub credential_material: Option<CredentialMaterialInput>,
    pub credential: Option<awaken_credential_contract::CredentialRef>,
    pub mount_path: String,
    pub initial_branch: Option<String>,
    pub initial_commit: Option<String>,
}

/// Transient participant receipt carried only until a Session root adopts the
/// configured Repository. Registry and Vault provenance remain independent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfiguredSessionRepository {
    pub owner: SessionRepositoryOwner,
    pub workspace_id: String,
    pub repository_id: awaken_resource_contract::RepositoryId,
    pub registry_provenance: SessionParticipantProvenance,
    pub credential: Option<CredentialMaterialIngressReceipt>,
}

use crate::SessionApplication;

fn merge_session_usage(total: &mut SessionUsage, child: SessionUsage) {
    total.input_tokens = total.input_tokens.saturating_add(child.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(child.output_tokens);
    total.cache_read_tokens = total
        .cache_read_tokens
        .saturating_add(child.cache_read_tokens);
    total.cache_creation_tokens = total
        .cache_creation_tokens
        .saturating_add(child.cache_creation_tokens);
    total.web_fetch_requests = total
        .web_fetch_requests
        .saturating_add(child.web_fetch_requests);
    total.web_search_requests = total
        .web_search_requests
        .saturating_add(child.web_search_requests);
    for (model, child_model) in child.by_model {
        let model_usage = total.by_model.entry(model).or_default();
        model_usage.input_tokens = model_usage
            .input_tokens
            .saturating_add(child_model.input_tokens);
        model_usage.output_tokens = model_usage
            .output_tokens
            .saturating_add(child_model.output_tokens);
        model_usage.cache_read_tokens = model_usage
            .cache_read_tokens
            .saturating_add(child_model.cache_read_tokens);
        model_usage.cache_creation_tokens = model_usage
            .cache_creation_tokens
            .saturating_add(child_model.cache_creation_tokens);
    }
}

impl SessionApplication {
    /// Validate that one published Agent can start new Sessions in this scope.
    pub fn validate_profiled_agent(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Result<(), RunError> {
        if self.session_profile(workspace_id, agent_id).is_none()
            || self.agent_unavailable(workspace_id, agent_id)
        {
            return Err(RunError::bad_request(format!(
                "agent `{agent_id}` is unavailable"
            )));
        }
        Ok(())
    }

    /// Read the committed transcript after enforcing durable Session ownership.
    /// Protocol projections are deliberately bypassed: the Session aggregate and
    /// Runtime commit log are the only authorities involved.
    pub async fn session_transcript(
        &self,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<Vec<Message>, RunError> {
        self.read_session_projection(session_id, Some(workspace_id))
            .await
            .map_err(|error| match error {
                crate::SessionProjectionRecoveryError::NotFound => {
                    RunError::bad_request("Session was not found")
                }
                crate::SessionProjectionRecoveryError::Rejected(error) => error,
                crate::SessionProjectionRecoveryError::Unavailable(message) => {
                    RunError::unavailable(message)
                }
            })?
            .ok_or_else(|| RunError::bad_request("Session was not found"))?;
        self.committed_messages(session_id).await
    }

    pub async fn session(
        &self,
        session_id: &str,
    ) -> Result<PersistedSession, SessionRepositoryError> {
        self.sessions_repo.get(session_id).await
    }

    pub async fn sessions_by_owner(
        &self,
        owner_scope: &str,
    ) -> Result<Vec<PersistedSession>, SessionRepositoryError> {
        self.sessions_repo.list_by_owner(owner_scope).await
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
    pub fn has_agent_profile_source(&self) -> bool {
        self.config_source.is_some()
    }

    #[must_use]
    pub fn session_profile_at_revision(
        &self,
        workspace_id: &str,
        agent_id: &str,
        source_revision: u64,
    ) -> Option<awaken_executable_agent_contract::ExecutableAgentSessionProfile> {
        self.config_source.as_ref().and_then(|source| {
            source.session_profile_at_revision_in(workspace_id, agent_id, source_revision)
        })
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
            .flat_map(|view| view.delegates.iter().cloned())
            .collect::<std::collections::VecDeque<_>>();
        while let Some(delegate) = pending.pop_front() {
            let agent_id = delegate.agent_id;
            if !seen.insert(agent_id.clone()) {
                continue;
            }
            let view = delegate
                .source_revision
                .and_then(|revision| {
                    self.session_profile_at_revision(workspace_id, &agent_id, revision)
                })
                .or_else(|| self.session_profile(workspace_id, &agent_id));
            let Some(view) = view else {
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
            pending.extend(view.delegates);
        }
        Ok(())
    }

    pub async fn missing_vault(
        &self,
        workspace_id: &str,
        vault_ids: &[String],
    ) -> Result<Option<String>, RunError> {
        let Some(source) = &self.credential_source else {
            return Ok(None);
        };
        for vault_id in vault_ids {
            let exists = source
                .has_vault(workspace_id, vault_id)
                .await
                .map_err(|error| {
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
            self.resource_registry
                .as_deref()
                .map(|catalog| catalog as &dyn awaken_resource_contract::ExecutionResourceResolver),
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
        self.resource_registry
            .as_ref()
            .ok_or_else(|| {
                RunError::bad_request("memory resources require a configured Resource Registry")
            })?
            .find_memory_store(workspace_id, memory_store_id)
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
    ) -> Result<ConfiguredSessionRepository, RunError> {
        let SessionRepositoryResourceInput {
            id,
            workspace_id,
            name,
            description,
            remote_url,
            credential_material,
            credential,
            mount_path: _,
            initial_branch,
            initial_commit,
        } = input;
        let owner = SessionRepositoryOwner::from_repository_id(&id).ok_or_else(|| {
            RunError::bad_request(
                "Session-owned Repository identity does not match its Managed/Profiled owner",
            )
        })?;
        if !owner.owns_repository_id(&id) {
            return Err(RunError::bad_request(
                "Session-owned Repository identity does not match its Managed/Profiled owner",
            ));
        }
        if credential_material.is_some() && credential.is_some() {
            return Err(RunError::bad_request(
                "repository cannot carry both inline credential material and a credential reference",
            ));
        }
        let repository_id = awaken_resource_contract::RepositoryId::from(id);
        let material_ingress = if let Some(material) = credential_material {
            let credential_target =
                awaken_session_contract::repository_transport_credential_target(&remote_url)
                    .map_err(|error| RunError::bad_request(error.to_string()))?;
            let ingress = self.credential_material_ingress.as_deref().ok_or_else(|| {
                RunError::bad_request(
                    "repository authorization requires a configured credential Vault",
                )
            })?;
            let source_id = awaken_credential_contract::CredentialSourceId(format!(
                "{}:credential",
                repository_id.as_str()
            ));
            Some((ingress, source_id, credential_target, material))
        } else {
            None
        };
        let credential_binding = if let Some((_, source_id, _, _)) = &material_ingress {
            Some(source_id.0.clone())
        } else {
            match credential {
                Some(credential) if !credential.id.trim().is_empty() && credential.revision > 0 => {
                    Some(credential.id)
                }
                Some(_) => {
                    return Err(RunError::bad_request(
                        "repository credential reference is invalid",
                    ));
                }
                None => None,
            }
        };
        let catalog = self.resource_registry.as_ref().ok_or_else(|| {
            RunError::bad_request("repository resources require a configured Resource Registry")
        })?;
        let initial_state = if material_ingress.is_some() {
            awaken_resource_contract::ResourceState::Suspended
        } else {
            awaken_resource_contract::ResourceState::Active
        };
        let definition = awaken_resource_contract::RepositoryDefinition {
            id: repository_id.clone(),
            workspace_id,
            name,
            description,
            metadata: owner.marker(),
            state: initial_state,
            current_config_version: awaken_resource_contract::ConfigVersion::INITIAL,
            timestamps: Default::default(),
        };
        let initial_config = awaken_resource_contract::RepositoryConfigVersion {
            repository_id: repository_id.clone(),
            version: awaken_resource_contract::ConfigVersion::INITIAL,
            remote_url,
            credential_binding,
            initial_branch,
            initial_commit,
            clone_policy: awaken_resource_contract::ClonePolicy::default(),
        };
        let map_registry_error = |error| {
            RunError::bad_request(format!(
                "repository resource could not be configured: {error}"
            ))
        };
        // The aggregate is the canonical validation owner. Run that pure
        // admission before either durable participant so invalid Repository
        // configuration cannot create a Vault row or a Registry aggregate.
        awaken_resource_contract::RepositoryAggregate::register(
            definition.clone(),
            initial_config.clone(),
        )
        .map_err(&map_registry_error)?;
        let (registered_state, registry_provenance) = match catalog.register_repository(
            awaken_resource_contract::RegisterRepository {
                definition: definition.clone(),
                initial_config: initial_config.clone(),
            },
        ) {
            Ok(()) => (initial_state, SessionParticipantProvenance::Applied),
            Err(awaken_resource_contract::ResourceRegistryError::AlreadyRegistered(_)) => {
                let stored_definition = catalog
                    .find_repository(&definition.workspace_id, definition.id.as_str())
                    .map_err(&map_registry_error)?;
                let stored_config = catalog
                    .find_repository_config(
                        &definition.workspace_id,
                        definition.id.as_str(),
                        awaken_resource_contract::ConfigVersion::INITIAL,
                    )
                    .map_err(&map_registry_error)?;
                let Some(stored_definition) = stored_definition else {
                    return Err(RunError::bad_request(
                        "repository resource could not be configured: registered Repository is unavailable in this Workspace",
                    ));
                };
                let lifecycle_is_exact = if material_ingress.is_some() {
                    matches!(
                        stored_definition.state,
                        awaken_resource_contract::ResourceState::Suspended
                            | awaken_resource_contract::ResourceState::Active
                    )
                } else {
                    stored_definition.state == awaken_resource_contract::ResourceState::Active
                };
                // State and timestamps are lifecycle progress owned by this
                // Registry saga, not request identity. Normalize only those
                // fields, then retain whole-definition equality for replay.
                let mut stored_registration = stored_definition.clone();
                stored_registration.state = definition.state;
                stored_registration.timestamps = definition.timestamps;
                if !lifecycle_is_exact
                    || stored_registration != definition
                    || stored_config.as_ref() != Some(&initial_config)
                {
                    return Err(RunError::bad_request(
                        "repository resource could not be configured: existing Repository does not exactly match the requested definition and initial config",
                    ));
                }
                (
                    stored_definition.state,
                    SessionParticipantProvenance::Replayed,
                )
            }
            Err(error) => return Err(map_registry_error(error)),
        };
        let Some((ingress, source_id, credential_target, material)) = material_ingress else {
            return Ok(ConfiguredSessionRepository {
                owner,
                workspace_id: definition.workspace_id,
                repository_id,
                registry_provenance,
                credential: None,
            });
        };
        let credential_entry = match ingress
            .enter_material(CredentialMaterialIngressCommand {
                source_id: source_id.clone(),
                workspace_id: definition.workspace_id.clone(),
                target: credential_target,
                usage: awaken_session_contract::repository_transport_credential_usage(),
                material,
            })
            .await
        {
            Ok(entry) => entry,
            Err(error) => {
                let first = RunError::bad_request(format!(
                    "repository authorization could not be sealed: {error}"
                ));
                if registry_provenance == SessionParticipantProvenance::Applied
                    && !self
                        .retire_owned_repository(
                            &definition.workspace_id,
                            &owner,
                            repository_id.as_str(),
                        )
                        .await
                {
                    tracing::warn!(
                        repository = %repository_id,
                        "Session Repository compensation remains pending after credential ingress failure"
                    );
                }
                return Err(first);
            }
        };
        if credential_entry.credential.id != source_id.0
            || credential_entry.credential.revision == 0
        {
            let first = RunError::bad_request(
                "repository authorization returned another credential binding",
            );
            if registry_provenance == SessionParticipantProvenance::Applied
                && !self
                    .retire_owned_repository(
                        &definition.workspace_id,
                        &owner,
                        repository_id.as_str(),
                    )
                    .await
            {
                tracing::warn!(
                    repository = %repository_id,
                    "Session Repository compensation remains pending after invalid credential ingress"
                );
            }
            return Err(first);
        }
        if registered_state == awaken_resource_contract::ResourceState::Active {
            return Ok(ConfiguredSessionRepository {
                owner,
                workspace_id: definition.workspace_id,
                repository_id,
                registry_provenance,
                credential: Some(credential_entry),
            });
        }
        if let Err(error) =
            catalog.change_repository_state(awaken_resource_contract::ChangeRepositoryState {
                workspace_id: definition.workspace_id.clone(),
                id: repository_id.clone(),
                state: awaken_resource_contract::ResourceState::Active,
            })
        {
            let first = map_registry_error(error);
            let configured = ConfiguredSessionRepository {
                owner,
                workspace_id: definition.workspace_id,
                repository_id,
                registry_provenance,
                credential: Some(credential_entry),
            };
            if !self
                .abort_unadopted_session_repositories(std::slice::from_ref(&configured))
                .await
            {
                tracing::warn!(
                    repository = %configured.repository_id,
                    "Session Repository compensation remains pending after activation failure"
                );
            }
            return Err(first);
        }
        Ok(ConfiguredSessionRepository {
            owner,
            workspace_id: definition.workspace_id,
            repository_id,
            registry_provenance,
            credential: Some(credential_entry),
        })
    }

    pub fn notify_lifecycle_fact(&self) {
        if let Some(notifier) = self.lifecycle_notifier.get() {
            notifier.notify();
        }
    }

    pub async fn runtime_owns_thread(&self, thread: &str) -> Result<bool, RunError> {
        self.runtime.owns_thread(thread).await
    }

    pub async fn committed_messages(&self, thread: &str) -> Result<Vec<Message>, RunError> {
        self.runtime.committed_messages(thread).await
    }

    /// Derived Managed child links; the Runtime reconstructs these from the
    /// parent tool facts and ordinary child Thread/dispatch facts.
    pub async fn coordinated_threads(
        &self,
        session_id: &str,
    ) -> Result<Vec<awaken_session_contract::CoordinatedThreadLink>, RunError> {
        self.runtime.coordinated_threads(session_id).await
    }

    /// Read cumulative accounting for one logical Thread through its parent
    /// Session partition. This is the same neutral Runtime authority used by
    /// Session totals; the application does not cache or re-aggregate it.
    pub async fn session_thread_usage(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<SessionUsage, RunError> {
        self.runtime
            .session_thread_usage(session_id, thread_id)
            .await
    }

    /// Open the Runtime's existing Thread-scoped live observer. The application
    /// adds no buffer or subscription registry; it only preserves the
    /// server/runtime boundary for protocol adapters.
    pub async fn subscribe_session_thread_live(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<Option<Box<dyn awaken_session_contract::SessionThreadLiveSubscription>>, RunError>
    {
        self.runtime
            .subscribe_session_thread_live(session_id, thread_id)
            .await
    }

    /// Read the Runtime's one consistent recovery prefix for a logical child
    /// through the parent Session partition. This is a direct query wrapper;
    /// the application owns no snapshot cache or reconstructed projection.
    pub async fn session_thread_recovery_snapshot(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<Option<awaken_agent_contract::thread::read::recovery::RunRecoverySnapshot>, RunError>
    {
        self.runtime
            .session_thread_recovery_snapshot(session_id, thread_id)
            .await
    }

    pub async fn session_thread_disposition(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<awaken_agent_contract::ThreadDisposition, RunError> {
        self.runtime
            .session_thread_disposition(session_id, thread_id)
            .await
    }

    pub async fn archive_session_thread(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<(), RunError> {
        let child = awaken_agent_contract::agent::thread::Id(thread_id.to_string());
        let links = self.runtime.coordinated_threads(session_id).await?;
        if !links
            .iter()
            .any(|link| link.session_id == session_id && link.thread_id == child)
        {
            return Err(RunError::bad_request(
                "Agent Thread was not found in this Session",
            ));
        }
        self.runtime
            .archive_session_thread(session_id, thread_id)
            .await
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

    pub async fn interrupt(&self, thread: &str) -> Result<(), RunError> {
        self.runtime.interrupt(thread).await
    }

    pub async fn define_outcome(
        &self,
        thread: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<OutcomeDrive, RunError> {
        self.runtime
            .define_outcome(thread, description, rubric, max_iterations)
            .await
    }

    pub async fn prepare_outcome(
        &self,
        thread: &str,
        outcome_id: &str,
        description: &str,
        rubric: &str,
        max_iterations: u32,
    ) -> Result<u64, RunError> {
        self.runtime
            .prepare_outcome(thread, outcome_id, description, rubric, max_iterations)
            .await
    }

    pub async fn continue_outcome(&self, thread: &str) -> Result<Option<OutcomeDrive>, RunError> {
        self.runtime.continue_outcome(thread).await
    }

    /// Project an exact terminal Outcome through the sole Runtime query port.
    /// The application owns no Outcome cache or continuation state.
    pub async fn committed_outcome_projection(
        &self,
        thread: &str,
        outcome_id: &str,
    ) -> Result<Option<CommittedOutcomeProjection>, RunError> {
        self.runtime
            .committed_outcome_projection(thread, outcome_id)
            .await
    }

    pub async fn supports_mid_conversation_system(&self, thread: &str) -> bool {
        self.runtime.supports_mid_conversation_system(thread).await
    }

    pub async fn session_usage(&self, thread: &str) -> Result<SessionUsage, RunError> {
        self.session_usage_with_additional_thread(thread, None)
            .await
    }

    /// Include the currently executing logical Thread exactly once even if its
    /// deterministic coordination receipt is still racing an already-admitted
    /// dispatch. The committed Thread usage and ordinary link projection remain
    /// the only authorities; this method creates no relationship cache.
    pub(crate) async fn session_usage_for_model_request(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> Result<SessionUsage, RunError> {
        self.session_usage_with_additional_thread(session_id, Some(thread_id))
            .await
    }

    async fn session_usage_with_additional_thread(
        &self,
        thread: &str,
        additional_thread: Option<&str>,
    ) -> Result<SessionUsage, RunError> {
        let mut usage = self.runtime.session_usage(thread).await?;
        let mut included = std::collections::BTreeSet::from([thread.to_string()]);
        for link in self.runtime.coordinated_threads(thread).await? {
            included.insert(link.thread_id.0.clone());
            let child = self
                .runtime
                .session_thread_usage(thread, &link.thread_id.0)
                .await?;
            merge_session_usage(&mut usage, child);
        }
        if let Some(additional_thread) = additional_thread
            && included.insert(additional_thread.to_string())
        {
            let current = self
                .runtime
                .session_thread_usage(thread, additional_thread)
                .await?;
            merge_session_usage(&mut usage, current);
        }
        let session = self
            .session_repository()
            .get(thread)
            .await
            .map_err(|error| RunError::unavailable(error.to_string()))?;
        usage.active_seconds =
            session.effective_runtime_active_millis(super::activity::now_unix_ms()) / 1_000;
        Ok(usage)
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
