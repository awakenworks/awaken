//! Session lifecycle for [`ManagedState`]: create, rehydrate, get/list,
//! update, delete, and archive.

use super::application::initial_mcp_candidates;
use super::*;

use super::session_mcp_projection::typed_mcp_servers;
use crate::types::AgentRef;
use awaken_session_contract::ApplicationSessionContributionFailure;

fn validate_session_skill_total(
    source: Option<&dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>,
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

    // Agent identity, rather than roster-edge count, owns one mounted Skill set.
    // This also closes cycles in malformed/legacy published topologies.
    let mut seen = std::collections::BTreeSet::from([root_agent_id.to_string()]);
    let mut pending = root_view
        .into_iter()
        .flat_map(|view| view.delegate_ids.iter().cloned())
        .collect::<std::collections::VecDeque<_>>();
    while let Some(agent_id) = pending.pop_front() {
        if !seen.insert(agent_id.clone()) {
            continue;
        }
        let Some(view) =
            source.and_then(|source| source.session_profile_in(workspace_id, &agent_id))
        else {
            continue;
        };
        total = total.saturating_add(view.skills.len());
        if total > MAX_SESSION_SKILLS {
            return Err(RunError::bad_request(
                "a session supports at most 500 skills across all agents",
            ));
        }
        pending.extend(view.delegate_ids);
    }
    Ok(())
}

impl ManagedState {
    pub fn validate_dream_agent(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Result<(), StateError> {
        if agent_id == awaken_dream_application::BUILT_IN_DREAM_AGENT_ID {
            return Ok(());
        }
        let Some(source) = &self.application.config_source() else {
            return Err(StateError::Run(RunError::bad_request(format!(
                "dream agent Agent `{agent_id}` is unavailable"
            ))));
        };
        if source.session_profile_in(workspace_id, agent_id).is_none()
            || source.agent_unavailable_in(workspace_id, agent_id)
        {
            return Err(StateError::Run(RunError::bad_request(format!(
                "dream agent Agent `{agent_id}` is unavailable"
            ))));
        }
        Ok(())
    }

    /// Frozen committed Session input used by the dream
    /// application. Workspace ownership is checked before the Runtime transcript
    /// is read; the returned messages preserve tool calls and tool results exactly
    /// as committed by the ordinary Session authority.
    pub async fn dream_transcript(
        &self,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<Vec<awaken_agent_contract::agent::message::Message>, StateError> {
        self.ensure_session(session_id).await?;
        let owner = self
            .resolve_owner(session_id)
            .await
            .ok_or(StateError::NotFound)?;
        if owner != workspace_id {
            return Err(StateError::NotFound);
        }
        Ok(self
            .application
            .runtime()
            .committed_messages(session_id)
            .await)
    }

    pub async fn prepare_protocol_session(
        &self,
        workspace_id: &str,
        thread_id: &str,
        agent_id: &str,
    ) -> Result<(), StateError> {
        if self
            .application
            .session_repository()
            .get(thread_id)
            .await
            .is_some()
        {
            // A durable row does not imply that this process has reconstructed
            // the runtime projection.  Reuse the authoritative restart path so
            // resources and the exact Environment snapshot are restored before
            // a non-Managed adapter is allowed to execute the next turn.
            return self.ensure_session(thread_id).await;
        }
        let request = serde_json::from_value(serde_json::json!({"agent": agent_id}))
            .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?;
        match self
            .create_session_with_identity(
                request,
                Some(workspace_id.to_string()),
                Some(thread_id.to_string()),
            )
            .await
        {
            Ok(_) => Ok(()),
            // Concurrent first turns share the explicit protocol thread id. The
            // durable create fence chooses one winner; every loser adopts that
            // exact committed Session through the normal recovery seam.
            Err(StateError::Conflict)
                if self
                    .application
                    .session_repository()
                    .get(thread_id)
                    .await
                    .is_some() =>
            {
                self.ensure_session(thread_id).await
            }
            Err(error) => Err(error),
        }
    }

    fn wire_session_status(status: &str) -> &'static str {
        match status {
            "preparing" => "preparing",
            "activating" => "activating",
            "activation_failed" => "failed",
            "terminated" => "terminated",
            _ => "idle",
        }
    }

    /// Refresh the disposable HTTP projection after the one durable root CAS.
    /// Every mutation crosses this seam, so realization, update, archive, and
    /// recovery cannot each invent a second cache-synchronization path.
    pub(super) fn refresh_cached_projection(
        &self,
        persisted: &PersistedSession,
    ) -> Result<(), StateError> {
        let mcp_servers = typed_mcp_servers(persisted.visible_mcp_servers());
        let mut sessions = self.sessions.lock().unwrap();
        let Some(record) = sessions.get_mut(&persisted.session_id) else {
            return Ok(());
        };
        record.session.status = Self::wire_session_status(&persisted.status);
        record.session.title = persisted.title.clone();
        record.session.metadata = persisted.metadata.clone();
        record.session.deployment_id = persisted.metadata.get("awaken.deployment_id").cloned();
        record.session.archived_at = persisted.archived_at.clone();
        record.session.agent.tools = crate::project::managed_tools(&persisted.tools);
        record.session.agent.mcp_servers = mcp_servers;
        record.resource_state = persisted.resources.clone();
        Ok(())
    }

    /// Wire-cache adapter around the Session application's sole root CAS. Every
    /// specialized command crosses the application boundary first; only its
    /// committed result is projected into the disposable Managed cache.
    pub(crate) async fn commit_session_snapshot(
        &self,
        owner_scope: &str,
        session: PersistedSession,
        operation: &str,
        lifecycle_facts: Vec<ManagedLifecycleFact>,
    ) -> Result<PersistedSession, StateError> {
        let session = self
            .application
            .commit_session_snapshot(owner_scope, session, operation, lifecycle_facts)
            .await
            .map_err(Self::map_application_mutation_error)?;
        self.refresh_cached_projection(&session)?;
        Ok(session)
    }

    fn map_application_mutation_error(
        error: awaken_session_application::SessionMutationError,
    ) -> StateError {
        match error {
            awaken_session_application::SessionMutationError::NotFound => StateError::NotFound,
            awaken_session_application::SessionMutationError::Conflict => StateError::Conflict,
            awaken_session_application::SessionMutationError::IdempotencyMismatch => {
                StateError::IdempotencyMismatch
            }
            awaken_session_application::SessionMutationError::Unavailable(message) => {
                StateError::Run(RunError::internal(message))
            }
        }
    }

    async fn create_session_snapshot(
        &self,
        owner_scope: &str,
        mut session: PersistedSession,
    ) -> Result<PersistedSession, StateError> {
        let payload = awaken_session_contract::SessionMutationPayload::Replace(session.clone());
        let payload_hash = payload.stable_hash();
        let revision = self
            .application
            .session_repository()
            .create(
                owner_scope,
                session.clone(),
                awaken_session_contract::IdempotencyRecord {
                    key: format!("managed:create:{}:{payload_hash}", session.session_id),
                    payload_hash,
                },
                Vec::new(),
            )
            .await
            .map_err(|error| match error {
                awaken_session_contract::SessionRepositoryError::IdempotencyMismatch => {
                    StateError::IdempotencyMismatch
                }
                awaken_session_contract::SessionRepositoryError::AlreadyExists => {
                    StateError::Conflict
                }
                error => StateError::Run(RunError::internal(error.to_string())),
            })?;
        session.revision = revision;
        Ok(session)
    }

    /// `POST /v1/sessions`.
    ///
    /// MCP binding (ADR-0043 Phase 3): each requested server is bound to a vault
    /// credential by exact `mcp_server_url` match across the request's
    /// `vault_ids`. The preparation intent, frozen generation-1 state, and exact
    /// realization claim all commit before
    /// [`SessionRuntime::prepare_session`](awaken_session_contract::SessionRuntime::prepare_session)
    /// performs external I/O. A failed realization leaves recoverable failed
    /// state and fails the create (the router maps the `RunError` to the error
    /// envelope). A `vault_ids` entry that names no
    /// existing vault fails the create closed too:
    /// a 404 naming the vault id, BEFORE anything is provisioned — never a
    /// silent no-binding whose 401 only surfaces at the first turn. (Without a
    /// wired vault surface there is nothing to validate against and every
    /// binding resolves to no credential, as before.)
    /// Fail-closed bind-time legality check, shared by session creation and any
    /// pre-flight bind check: every vault a session references must exist. This is
    /// the one validation that must hold *before* an id is minted or a thread is
    /// prepared, so it lives in a single method rather than inline — a dry-run
    /// bind check calls exactly this, and gets exactly the error create would.
    pub async fn check_bind(&self, req: &SessionCreateParams) -> Result<(), StateError> {
        if let Some(vaults) = &self.application.credential_source() {
            for vault_id in &req.vault_ids {
                let exists = vaults.has_vault(vault_id).await.map_err(|error| {
                    StateError::Run(RunError::internal(format!(
                        "credential authority unavailable: {error}"
                    )))
                })?;
                if !exists {
                    return Err(StateError::VaultNotFound(vault_id.clone()));
                }
            }
        }
        Ok(())
    }

    pub async fn create_session(
        &self,
        req: SessionCreateParams,
        // The edge-resolved owning workspace (aspect): handed to the lifecycle
        // sink for webhook/usage stamping, but NEVER stored on the core session.
        workspace_id: Option<String>,
    ) -> Result<Session, StateError> {
        self.create_session_with_identity(req, workspace_id, None)
            .await
    }

    /// The canonical public create command: create the Session and enqueue its
    /// admitted initial Events through the ordinary Event command. Protocol
    /// handlers and Deployment launchers share this method so neither can create
    /// an inert Session or invent a follow-up Event path.
    pub async fn create_session_with_initial_events(
        self: &Arc<Self>,
        req: SessionCreateParams,
        workspace_id: Option<String>,
    ) -> Result<Session, StateError> {
        self.create_session_with_initial_events_and_identity(req, workspace_id, None)
            .await
    }

    pub(super) async fn create_session_with_initial_events_and_identity(
        self: &Arc<Self>,
        req: SessionCreateParams,
        workspace_id: Option<String>,
        explicit_id: Option<String>,
    ) -> Result<Session, StateError> {
        let initial_events = req.initial_events.clone();
        let mut session = self
            .create_session_with_identity(req, workspace_id, explicit_id)
            .await?;
        if !initial_events.is_empty() {
            self.start_initial_events(&session.id, initial_events)?;
            session.status = "running";
        }
        Ok(session)
    }

    /// Create the Control-owned half of an externally dispatched application
    /// Session under the dispatcher's exact durable thread identity.
    ///
    /// Only application-contribution Sessions may cross this embedding seam:
    /// their Runtime projection is realized by the claim-owning Worker, while
    /// this aggregate remains the sole author of the frozen baseline and
    /// realization generations. Ordinary public Session creation continues to
    /// mint its own identity through [`Self::create_session`].
    pub async fn create_application_session(
        &self,
        session_id: impl Into<String>,
        req: SessionCreateParams,
        workspace_id: Option<String>,
    ) -> Result<Session, StateError> {
        let session_id = session_id.into();
        if session_id.trim().is_empty() {
            return Err(StateError::Run(RunError::bad_request(
                "application Session id is empty",
            )));
        }
        if !req.application_contribution_required {
            return Err(StateError::Run(RunError::bad_request(
                "externally identified Session requires an application contribution",
            )));
        }
        self.create_session_with_identity(req, workspace_id, Some(session_id))
            .await
    }

    pub(super) async fn create_session_with_identity(
        &self,
        mut req: SessionCreateParams,
        workspace_id: Option<String>,
        explicit_id: Option<String>,
    ) -> Result<Session, StateError> {
        // Initial-event admission is atomic with Session creation: validate the
        // complete batch before bind checks, identity allocation, persistence, or
        // Runtime preparation. The shared validator is also used by Deployments.
        req.validate_initial_events()
            .map_err(|message| StateError::Run(RunError::bad_request(message)))?;
        self.check_bind(&req).await?;
        // Mint from the process-incarnation namespace so active-active peers and
        // restarted processes cannot choose the same Session id. The repository
        // check remains the final collision fence; `ensure_session` is still the
        // only path that may reattach to a caller-supplied existing thread.
        let id = match explicit_id {
            Some(id) => id,
            None => loop {
                let sequence = self.session_seq.fetch_add(1, Ordering::SeqCst);
                let candidate = format!(
                    "sesn_{}",
                    awaken_session_contract::stable_fingerprint(&(
                        "managed-session",
                        &self.application.runtime_incarnation(),
                        sequence,
                    ))
                );
                if !self.application.runtime().owns_thread(&candidate).await
                    && self
                        .application
                        .session_repository()
                        .get(&candidate)
                        .await
                        .is_none()
                {
                    break candidate;
                }
            },
        };
        let agent_id = req.agent.id().to_string();
        let owner_scope = workspace_id
            .clone()
            .unwrap_or_else(|| DEFAULT_SCOPE.to_string());
        let config_view = self
            .application
            .config_source()
            .and_then(|source| source.session_profile_in(&owner_scope, &agent_id));
        let is_built_in_dream_agent = agent_id == awaken_dream_application::BUILT_IN_DREAM_AGENT_ID
            && req
                .metadata
                .get("awaken.session.origin")
                .is_some_and(|origin| origin == "dream");
        if config_view.is_none()
            && self
                .application
                .config_source()
                .is_some_and(|source| source.agent_unavailable_in(&owner_scope, &agent_id))
            && !is_built_in_dream_agent
        {
            return Err(StateError::Run(RunError::bad_request(format!(
                "agent_unavailable: agent `{agent_id}` cannot start a new session"
            ))));
        }
        // Cause/effect decision table for Session model authority:
        // R1 no override + published Agent -> inherit its complete publication;
        // R2 equal official override -> accept without changing the route;
        // R3 different official override + published Agent -> reject before state;
        // R4 cleared override -> reject. Metadata never selects execution.
        // This prevents a model string from being stitched to the Agent's old
        // backend/credential pins. Selecting another route requires publishing an
        // Agent for that Managed model id first.
        let selected_model: Option<ModelConfig> = match req.agent.model_override() {
            ModelOverride::Set(cfg) => {
                if config_view
                    .as_ref()
                    .and_then(|view| view.model.as_deref())
                    .is_some_and(|published| published != cfg.id)
                {
                    return Err(StateError::Run(RunError::bad_request(
                        "agent_model_override_unpublished: publish or update an Agent with this model id before creating the Session",
                    )));
                }
                Some(cfg)
            }
            ModelOverride::Cleared => {
                return Err(StateError::Run(RunError::bad_request(
                    "agent_model_required: a session override cannot clear `model`",
                )));
            }
            ModelOverride::Absent => config_view
                .as_ref()
                .and_then(|view| view.model.clone())
                .map(ModelConfig::new),
        };
        // Echo the agent version the client pinned (or overrode over), defaulting to 1.
        let agent_version = req.agent.version().unwrap_or(1);
        let delegate_ids = config_view
            .as_ref()
            .map(|view| view.delegate_ids.clone())
            .unwrap_or_default();
        // Anthropic requires MCP declarations and toolsets to be a bijective
        // reference: every declared server has a toolset and every toolset names
        // a declared server. Validate create-time overrides before provisioning.
        if let AgentRef::Object(override_ref) = &req.agent
            && let (Some(Some(mcp_servers)), Some(Some(tools))) =
                (&override_ref.mcp_servers, &override_ref.tools)
        {
            let declared = mcp_servers
                .iter()
                .map(crate::types::agent::AgentMcpServer::name)
                .collect::<std::collections::BTreeSet<_>>();
            let toolset_names = tools
                .iter()
                .filter_map(|tool| match tool {
                    awaken_session_contract::AgentTool::McpToolset {
                        mcp_server_name, ..
                    } => Some(mcp_server_name.as_str()),
                    _ => None,
                })
                .collect::<std::collections::BTreeSet<_>>();
            if declared != toolset_names {
                return Err(StateError::Run(RunError::bad_request(
                    "each mcp_server must be referenced by exactly one mcp_toolset",
                )));
            }
        }
        let agent_mcp_override = match &req.agent {
            AgentRef::Object(override_ref) => override_ref
                .mcp_servers
                .as_ref()
                .map(|servers| servers.as_deref().unwrap_or_default().to_vec()),
            AgentRef::Id(_) => None,
        };
        let mcp_drafts = self
            .application
            .normalize_mcp_drafts(
                initial_mcp_candidates(
                    &req.mcp_servers,
                    config_view.as_ref(),
                    agent_mcp_override.as_deref(),
                ),
                &req.vault_ids,
            )
            .await?;
        // Parse the wire `resources[]` (ADR-0038) into staged mounts, and project each
        // into a DTO entry so the created session echoes its create-time resources —
        // list/get/delete then address these and any later-attached ones uniformly.
        const MAX_SESSION_MEMORY_STORES: usize = 8;
        const MAX_MEMORY_INSTRUCTIONS_CHARS: usize = 4_096;
        let memory_resources = req.resources.iter().filter_map(|resource| match resource {
            crate::types::resource::ResourceInput::MemoryStore { instructions, .. } => {
                Some(instructions)
            }
            _ => None,
        });
        let mut memory_count = 0usize;
        for instructions in memory_resources {
            memory_count += 1;
            if instructions.as_ref().is_some_and(|instructions| {
                instructions.chars().count() > MAX_MEMORY_INSTRUCTIONS_CHARS
            }) {
                return Err(StateError::Run(RunError::bad_request(format!(
                    "memory store instructions support at most {MAX_MEMORY_INSTRUCTIONS_CHARS} characters"
                ))));
            }
        }
        if memory_count > MAX_SESSION_MEMORY_STORES {
            return Err(StateError::Run(RunError::bad_request(format!(
                "a Session supports at most {MAX_SESSION_MEMORY_STORES} memory stores"
            ))));
        }
        let resources = std::mem::take(&mut req.resources)
            .iter()
            .map(crate::types::resource::ResourceInput::to_parsed_input)
            .collect::<Vec<_>>();
        if resources
            .iter()
            .filter(|resource| matches!(resource.target, ParsedInputTarget::File(_)))
            .count()
            > super::resource::MAX_SESSION_FILE_RESOURCES
        {
            return Err(StateError::Run(RunError::bad_request(format!(
                "a Session supports at most {} files",
                super::resource::MAX_SESSION_FILE_RESOURCES
            ))));
        }
        // Lower compatibility Repository URLs/tokens before the neutral resolver:
        // the catalog receives a Session-scoped definition and a Vault reference,
        // never the token. File/Memory already carry platform identities on wire.
        let agent_defaults = config_view
            .as_ref()
            .map(|view| view.resources.as_slice())
            .unwrap_or_default();
        let attachments = self
            .lower_session_input_attachments(&id, &owner_scope, &resources, agent_defaults)
            .await?;
        // Resolve the session's environment (defaulting to the local one) and its
        // networking policy once, for both the SessionInit (staged before the first
        // turn) and the echoed Session object.
        let agent_environment = config_view
            .as_ref()
            .and_then(|view| view.environment.as_ref());
        // The immutable Agent publication is the only backend authority.  The
        // baseline persists this projection so recovery and Worker placement do
        // not need to reopen the publication registry.
        let published_backend_ref = config_view
            .as_ref()
            .map(|view| view.backend_ref.clone())
            .filter(|backend_ref| !backend_ref.trim().is_empty());
        let mcp_targets = mcp_drafts
            .iter()
            .map(|draft| draft.target.clone())
            .collect::<Vec<_>>();
        let (environment_id, environment) = self
            .resolve_session_environment(
                req.environment_id.as_deref(),
                agent_environment,
                published_backend_ref.as_deref(),
                &mcp_targets,
            )
            .await?;
        // Sole protocol-neutral Resource composition/resolution point. Environment
        // selection has already produced one exact snapshot above; the final
        // SessionCreationIntent is the only value that combines and freezes both
        // families. Runtime never re-opens Agent or Resource stores.
        let mut resolved_resources = awaken_session_contract::SessionInputResolver::resolve_inputs(
            &owner_scope,
            self.application
                .resource_catalog()
                .map(|catalog| catalog as &dyn awaken_resource_contract::ResourceConfigSource),
            agent_defaults,
            &attachments,
        )
        .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?;
        let effective_skills = match &req.agent {
            AgentRef::Object(override_ref) => override_ref.skills.as_ref().map(|skills| {
                skills
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .map(crate::types::agent::AgentSkill::into_binding)
                    .collect::<Vec<_>>()
            }),
            AgentRef::Id(_) => None,
        }
        .or_else(|| config_view.as_ref().map(|view| view.skills.clone()));
        validate_session_skill_total(
            self.application.config_source(),
            &owner_scope,
            &agent_id,
            config_view.as_ref(),
            effective_skills.as_deref().unwrap_or_default(),
        )
        .map_err(StateError::Run)?;
        if let Some(skills) = &effective_skills {
            resolved_resources.skills = Some(
                self.application
                    .runtime()
                    .resolve_session_skills(&owner_scope, skills)
                    .await
                    .map_err(StateError::Run)?,
            );
        }
        self.application
            .pin_repository_credentials(
                &owner_scope,
                &environment.credential_realization.resource_holder,
                &mut resolved_resources,
            )
            .await
            .map_err(Self::map_preparation_error)?;
        // Validate the advertised tool surface before persisting an activation or
        // touching a Host. A definition error cannot strand Prepared resources.
        let caps = self.application.runtime().capabilities_for(&id);
        for tool in &caps.custom_tools {
            project::validate_custom_tool(tool)
                .map_err(|msg| StateError::Run(RunError::bad_request(msg)))?;
        }
        let capability_tools = project::session_tool_configuration(&project::agent_tools(&caps));
        let inherited_tools = config_view.as_ref().map_or_else(
            || capability_tools.clone(),
            |profile| awaken_session_contract::SessionToolConfiguration {
                toolsets: if profile.toolsets.is_empty() {
                    capability_tools.toolsets.clone()
                } else {
                    profile.toolsets.clone()
                },
                client_tools: profile.client_tools.clone(),
            },
        );
        let effective_tools = match &req.agent {
            AgentRef::Object(reference) => reference
                .tools
                .as_ref()
                .map(|tools| {
                    project::session_tool_configuration(tools.as_deref().unwrap_or_default())
                })
                .unwrap_or(inherited_tools),
            AgentRef::Id(_) => inherited_tools,
        };
        let resolved_model = selected_model
            .clone()
            .unwrap_or_else(|| ModelConfig::new(self.application.runtime().model()));
        let execution_model_ref = config_view
            .as_ref()
            .and_then(|view| view.execution_model_ref.clone())
            .unwrap_or_else(|| resolved_model.id.clone());
        let application_required = req.application_contribution_required;
        let creation_intent = awaken_session_contract::SessionCreationIntent {
            control: awaken_session_contract::ControlSessionCreationInputs {
                environment,
                runtime_placement: self.application.runtime_placement(),
                mcp_authoring: awaken_session_contract::SessionMcpAuthoringContext {
                    ordered_vault_ids: req.vault_ids.clone(),
                },
                agent_id: agent_id.clone(),
                model: resolved_model.id.clone(),
                execution_model_ref,
                runtime: published_backend_ref,
                delegate_ids: delegate_ids.clone(),
                toolsets: effective_tools.toolsets.clone(),
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: Vec::new(),
                resources: resolved_resources,
                initial_mcp: mcp_drafts,
            },
            application: if application_required {
                awaken_session_contract::ApplicationContributionState::Required
            } else {
                awaken_session_contract::ApplicationContributionState::Absent
            },
        };
        // Compile before insert so malformed no-application input cannot strand a
        // preparation row. The transient result is committed exactly once after
        // the insert; required applications compile only after their contribution.
        let compiled = if application_required {
            None
        } else {
            Some(
                creation_intent
                    .clone()
                    .finalize(Vec::new())
                    .map_err(|error| StateError::Run(RunError::bad_request(error.to_string())))?,
            )
        };
        let mut persisted = PersistedSession {
            session_id: id.clone(),
            revision: Default::default(),
            baseline: awaken_session_contract::SessionBaselineState::Preparing(creation_intent),
            title: req.title.clone(),
            metadata: req.metadata.clone(),
            tools: effective_tools.clone(),
            activity_epoch: 0,
            environment: Default::default(),
            mcp: Default::default(),
            resources: Default::default(),
            realization: None,
            status: "preparing".to_string(),
            archived_at: None,
        };
        // The activation intent and owner fence commit before Host/worker IO.
        persisted = self
            .create_session_snapshot(&owner_scope, persisted)
            .await?;
        if let Some(compiled) = compiled {
            persisted = self
                .application
                .commit_compiled_session_creation(&owner_scope, persisted, compiled)
                .await
                .map_err(|error| match error {
                    ApplicationSessionContributionFailure::Conflict => StateError::Conflict,
                    error => StateError::Run(RunError::internal(error.to_string())),
                })?;
            // Cause/effect decision table (the cross-product is exercised by the
            // placement test below):
            //
            // | Frozen placement | Application | Coordinator effect | Realization owner |
            // |---|---|---|---|
            // | local | absent | canonical local phase driver | local Runtime |
            // | WorkQueue | absent | install dispatch projection only | claiming Worker |
            // | any | required | wait for claimed contribution | claiming Worker |
            //
            // The frozen baseline and Requested generations are durable before
            // either effect. Dispatch preparation deliberately acquires no
            // realization lease and performs no physical Runtime I/O.
            let realization = if self.application.requires_external_realization(&persisted) {
                self.application
                    .install_dispatch_projection(&owner_scope, &persisted)
                    .await
                    .map_err(Self::map_realization_application_error)
                    .map(|()| persisted.clone())
            } else {
                self.realize_session_locally(&id).await
            };
            persisted = match realization {
                Ok(persisted) => persisted,
                Err(error) => {
                    let _ = self
                        .release_terminal_resources(&id, Some(&owner_scope), &[])
                        .await;
                    return Err(error);
                }
            };
        }
        let deployment_id = req.metadata.get("awaken.deployment_id").cloned();
        let session_tools = project::managed_tools(&effective_tools);
        let mut session = Session {
            id: id.clone(),
            kind: "session",
            agent: SessionAgent {
                id: agent_id.clone(),
                kind: "agent",
                version: agent_version,
                // Echo the accepted official override or the published Agent model.
                model: resolved_model,
                name: agent_id.clone(),
                description: None,
                system: config_view.as_ref().and_then(|view| view.system.clone()),
                tools: session_tools,
                // Echo the accepted servers in the SDK's `{name, type:"url", url}` shape.
                mcp_servers: typed_mcp_servers(persisted.visible_mcp_servers()),
                skills: effective_skills.as_ref().map_or_else(
                    || project::agent_skills(&caps),
                    |skills| {
                        skills
                            .iter()
                            .cloned()
                            .map(crate::types::agent::AgentSkill::from_binding)
                            .collect()
                    },
                ),
                multiagent: config_view.as_ref().map_or_else(
                    || project::agent_multiagent(&caps),
                    |view| project::agent_multiagent_ids(&view.delegate_ids),
                ),
            },
            environment_id: environment_id.clone(),
            created_at: PROCESSED_AT.to_string(),
            updated_at: PROCESSED_AT.to_string(),
            archived_at: None,
            title: req.title,
            metadata: req.metadata,
            // Resource wire values are projected from `resource_state` on response.
            resources: Vec::new(),
            outcome_evaluations: Vec::new(),
            status: if application_required {
                "preparing"
            } else {
                "idle"
            },
            stats: SessionStats::default(),
            usage: Usage::default(),
            vault_ids: req.vault_ids.clone(),
            deployment_id,
        };
        // Anthropic's create-time overrides are session-local replacements. Null
        // clears nullable/list fields; an empty list also clears a list field.
        if let AgentRef::Object(override_ref) = &req.agent {
            if let Some(system) = &override_ref.system {
                session.agent.system = system.clone();
            }
            if let Some(tools) = &override_ref.tools {
                if tools.as_ref().is_none_or(|tools| tools.is_empty())
                    && !session.agent.skills.is_empty()
                {
                    return Err(StateError::Run(RunError::bad_request(
                        "cannot clear tools while skills are configured",
                    )));
                }
                session.agent.tools = project::resolved_tools(tools.as_deref().unwrap_or_default());
            }
        }
        persisted.tools = project::session_tool_configuration(&session.agent.tools);
        // Persist the session's config (secret-free) so a restart or a peer process
        // rehydrates its real agent/model/title/metadata/MCP, not a placeholder.
        // The core session record is tenancy-agnostic (authz is an edge aspect) —
        // it never stores a workspace/org.
        let created_fact = if application_required {
            persisted = self
                .commit_session_snapshot(
                    &owner_scope,
                    persisted,
                    "record-preparing-config",
                    Vec::new(),
                )
                .await?;
            None
        } else {
            let fact = lifecycle_fact(
                format!("session:{id}:created"),
                &id,
                workspace_id.clone(),
                lifecycle_event::SESSION_IDLED,
            );
            persisted = self
                .commit_session_snapshot(&owner_scope, persisted, "activate", vec![fact.clone()])
                .await?;
            Some(fact)
        };
        self.owners.lock().unwrap().insert(id.clone(), owner_scope);
        // Dispatch only after the active activation and Session lifecycle fact are
        // durable. A worker can never claim a work item whose resource intent is
        // still merely Prepared. The application command is shared with recovery.
        if let Err(error) = self.application.dispatch_session_work(&persisted).await {
            // The frozen Session baseline is already durable and is the
            // authoritative dispatch intent. Returning an ambiguous create
            // failure here could make a client create a second Session while
            // reconciliation later dispatches this one. Keep the successful
            // create result and let the canonical reconciler retry projection.
            tracing::warn!(
                session = %id,
                environment = %environment_id,
                error = ?error,
                "Session WorkQueue dispatch remains pending after create"
            );
        }
        let record = SessionRecord::new(
            agent_id,
            session,
            persisted.resources,
            Vec::new(),
            Default::default(),
        );
        let session = record.session_projection();
        self.sessions.lock().unwrap().insert(id.clone(), record);
        // Project the committed create as a lifecycle fact: a fresh session is idle,
        // so fan out `session.status_idled` (the webhook catalog name — past-tense
        // fact, distinct from the SSE `session.status_idle` transition) to any
        // workspace-scoped subscribers. The owning workspace comes from the edge (the
        // aspect), passed in — never read back from the core record. Out-of-band.
        if let (Some(sink), Some(created_fact)) =
            (&self.application.lifecycle_sink(), &created_fact)
        {
            sink.emit_fact(
                &created_fact.id,
                &id,
                workspace_id.as_deref(),
                lifecycle_event::SESSION_IDLED,
            )
            .await;
        }
        Ok(session)
    }

    /// The owner scope of `session_id`, if this process created (or has cached) it —
    /// the aspect-layer session→owner lookup the edge ownership guard consults
    /// (ADR-0051). `None` when the id is unknown to this process (e.g. a cross-
    /// process session before rehydration), where the guard falls through and the
    /// persistence layer remains the fence.
    #[must_use]
    pub fn owner_scope(&self, session_id: &str) -> Option<String> {
        self.owners.lock().unwrap().get(session_id).cloned()
    }

    /// Resolve the owner scope of `session_id` for the edge ownership guard,
    /// consulting the in-memory index first (same-process, no I/O) and then the
    /// durable store (cross-process, after a restart lost the index). `None` when
    /// no backend knows the session — a genuinely unknown id, where the guard
    /// falls through and the handler's own `NotFound` answers.
    pub async fn resolve_owner(&self, session_id: &str) -> Option<String> {
        if let Some(scope) = self.owner_scope(session_id) {
            return Some(scope);
        }
        self.application
            .session_repository()
            .owner(session_id)
            .await
    }

    /// A session object reconstructed for a rehydrated (post-restart) session.
    /// When the durable repo holds the session's config it is restored faithfully;
    /// otherwise (a session created before the repo existed, or a purely in-memory
    /// deployment) it falls back to the runtime's advertised surface with
    /// placeholder agent/title/metadata — the pre-repo behavior.
    pub(crate) fn rehydrated_session(
        &self,
        id: &str,
        persisted: Option<PersistedSession>,
    ) -> Result<Session, StateError> {
        let caps = self.application.runtime().capabilities_for(id);
        let default_tools = project::agent_tools(&caps);
        let (
            agent_id,
            model,
            environment_id,
            title,
            metadata,
            agent_tools,
            mcp_servers,
            status,
            archived_at,
        ) = match persisted {
            Some(p) => {
                let (agent_id, model, environment_id) = p
                    .frozen_baseline()
                    .map(|baseline| {
                        (
                            baseline.agent_id.clone(),
                            baseline.model.clone(),
                            baseline.environment.environment_id.clone(),
                        )
                    })
                    .unwrap_or_else(|| {
                        (
                            "assistant".into(),
                            self.application.runtime().model(),
                            p.environment_id().to_string(),
                        )
                    });
                let mcp_servers = typed_mcp_servers(p.visible_mcp_servers());
                (
                    agent_id,
                    model,
                    environment_id,
                    p.title,
                    p.metadata,
                    project::managed_tools(&p.tools),
                    mcp_servers,
                    Self::wire_session_status(&p.status),
                    p.archived_at,
                )
            }
            None => (
                "assistant".to_string(),
                self.application.runtime().model(),
                "env_local".to_string(),
                None,
                Default::default(),
                default_tools,
                Vec::new(),
                "idle",
                None,
            ),
        };
        let deployment_id = metadata.get("awaken.deployment_id").cloned();
        Ok(Session {
            id: id.to_string(),
            kind: "session",
            agent: SessionAgent {
                id: agent_id.clone(),
                kind: "agent",
                version: 1,
                model: ModelConfig::new(model),
                name: agent_id,
                description: None,
                system: None,
                tools: agent_tools,
                mcp_servers,
                skills: project::agent_skills(&caps),
                multiagent: project::agent_multiagent(&caps),
            },
            environment_id,
            created_at: PROCESSED_AT.to_string(),
            updated_at: PROCESSED_AT.to_string(),
            archived_at,
            title,
            metadata,
            resources: Vec::new(),
            outcome_evaluations: Vec::new(),
            status,
            stats: SessionStats::default(),
            usage: Usage::default(),
            vault_ids: Vec::new(),
            deployment_id,
        })
    }

    /// Rebuild only the disposable projection needed by a terminal command.
    /// Terminal recovery must not prepare a runtime or realize MCP again: those
    /// effects may carry expired, Run-scoped credentials and are about to be
    /// released rather than used.
    async fn ensure_session_for_terminal_cleanup(&self, id: &str) -> Result<(), StateError> {
        if self.sessions.lock().unwrap().contains_key(id) {
            return Ok(());
        }
        let persisted = self
            .application
            .session_repository()
            .get(id)
            .await
            .filter(|session| !matches!(session.status.as_str(), "deleted" | "activation_failed"))
            .ok_or(StateError::NotFound)?;
        let owner_scope = self
            .application
            .session_repository()
            .owner(id)
            .await
            .unwrap_or_else(|| DEFAULT_SCOPE.to_string());
        let delegated_runs = self
            .application
            .runtime()
            .delegated_runs(id)
            .await
            .map_err(StateError::Run)?;
        let mut record = SessionRecord::new(
            persisted.agent_id().unwrap_or("assistant").to_string(),
            self.rehydrated_session(id, Some(persisted.clone()))?,
            persisted.resources,
            Vec::new(),
            Default::default(),
        );
        self.append_delegation_projections(&mut record, &delegated_runs);
        self.sessions
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_insert(record);
        self.owners
            .lock()
            .unwrap()
            .entry(id.to_string())
            .or_insert(owner_scope);
        Ok(())
    }

    /// `GET /v1/sessions/{id}`.
    pub fn get_session(&self, id: &str) -> Result<Session, StateError> {
        let sessions = self.sessions.lock().unwrap();
        sessions
            .get(id)
            .map(SessionRecord::session_projection)
            .ok_or(StateError::NotFound)
    }

    pub async fn session_revision(
        &self,
        id: &str,
    ) -> Result<awaken_session_contract::SessionRevision, StateError> {
        self.application
            .session_repository()
            .get(id)
            .await
            .map(|session| session.revision)
            .ok_or(StateError::NotFound)
    }

    /// `GET /v1/sessions` — every session, ascending id (deterministic).
    pub fn list_sessions(&self) -> Vec<Session> {
        let sessions = self.sessions.lock().unwrap();
        let mut out: Vec<Session> = sessions
            .values()
            .map(SessionRecord::session_projection)
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Sessions owned by `scope` — the tenancy-fenced list (ADR-0051), so a
    /// workspace's `GET /v1/sessions` never sees another's. A session with no
    /// recorded owner belongs to the seeded default scope. Mirrors the per-id
    /// ownership guard, which the collection route does not pass through.
    pub fn list_sessions_scoped(&self, scope: &str) -> Vec<Session> {
        // Snapshot owners first (lock, clone, drop) so we never hold two locks at
        // once — create_session takes `owners` on its own path.
        let owners = self.owners.lock().unwrap().clone();
        let sessions = self.sessions.lock().unwrap();
        let mut out: Vec<Session> = sessions
            .values()
            .filter(|r| {
                owners
                    .get(&r.session.id)
                    .map_or(scope == DEFAULT_SCOPE, |owner| owner == scope)
            })
            .map(SessionRecord::session_projection)
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// `DELETE /v1/sessions/{id}` — commit a terminal `session.deleted` event,
    /// push it to any open SSE stream, then drop the in-memory record. The
    /// broadcast happens *before* removal because after the record is gone there
    /// is nothing to backfill from: a live frame is the only way a streaming
    /// client observes the deletion, and a subsequent `events.list`/`retrieve`
    /// is a 404 (delete removes the session; it does not tombstone it as archive
    /// does).
    pub async fn delete_session(&self, id: &str) -> Result<(), StateError> {
        // Snapshot before the durable commit, but do not remove the visible record
        // until the repository has atomically stored the terminal state, cleanup
        // intent, and outbox fact. The row becomes a tombstone only after every
        // external cleanup succeeds; until then it is hidden application state for
        // the ResourceReclaimer.
        let child_threads = {
            let sessions = self.sessions.lock().unwrap();
            sessions
                .get(id)
                .ok_or(StateError::NotFound)?
                .child_threads
                .clone()
        };
        let owner = self.resolve_owner(id).await;
        let deleted_fact = lifecycle_fact(
            format!("session:{id}:deleted"),
            id,
            owner.clone(),
            lifecycle_event::SESSION_DELETED,
        );
        let mut persisted = self
            .application
            .session_repository()
            .get(id)
            .await
            .ok_or(StateError::NotFound)?;
        persisted.status = "deleted".into();
        if persisted.resources.pending.is_none() {
            persisted
                .resources
                .begin_release()
                .map_err(|error| StateError::Run(RunError::internal(error.to_string())))?;
        }
        let owner_scope = owner.as_deref().unwrap_or(DEFAULT_SCOPE);
        self.commit_session_snapshot(
            owner_scope,
            persisted,
            "delete-intent",
            vec![deleted_fact.clone()],
        )
        .await?;

        {
            let deleted_id = self.next_event_id();
            let mut sessions = self.sessions.lock().unwrap();
            let record = sessions.get_mut(id).ok_or(StateError::NotFound)?;
            let from = record.events.len();
            record.events.push(Event {
                id: deleted_id,
                kind: OutboundKind::SessionDeleted {},
                processed_at: Some(PROCESSED_AT.to_string()),
            });
            self.broadcast_committed_from(id, record, from);
            sessions.remove(id);
        }
        // Terminal edge: dispose the session's sandbox(es) at the host — the main
        // thread is the session id, and each spawned child agent thread gets its own.
        // Best-effort teardown: the session IS deleted from the client's view
        // regardless, so a dispose failure must not resurrect a deleted session.
        if self
            .release_terminal_resources(id, owner.as_deref(), &child_threads)
            .await
        {
            let released = self
                .application
                .session_repository()
                .get(id)
                .await
                .ok_or(StateError::NotFound)?;
            self.application
                .tombstone_session_snapshot(owner_scope, &released, deleted_fact.clone())
                .await
                .map_err(Self::map_application_mutation_error)?;
        }
        // Project the deletion as a lifecycle fact so a webhook subscriber is
        // notified, mirroring create's `session.status_idled` and archive's
        // `session.status_terminated`. The owner is resolved from the persisted
        // owner (the delete edge carries only the id).
        if let Some(sink) = &self.application.lifecycle_sink() {
            sink.emit_fact(
                &deleted_fact.id,
                id,
                owner.as_deref(),
                lifecycle_event::SESSION_DELETED,
            )
            .await;
        }
        Ok(())
    }

    /// Dispose the host sandbox(es) for a session being torn down at a terminal
    /// edge: the main thread (the session id) plus each spawned child agent thread
    /// (the Runtime relationship's stable child Run id). Best-effort — the terminal transition has already committed, so
    /// a dispose failure is logged, never propagated (it must not resurrect the
    /// session). `SessionRuntime::end_session` is a no-op for a thread that never
    /// materialized a sandbox, so deriving child ids is safe.
    async fn end_session_sandboxes(&self, id: &str, child_threads: &[SessionThread]) -> bool {
        let mut threads: Vec<String> = vec![id.to_string()];
        for child in child_threads {
            threads.push(child.id.clone());
        }
        let mut released = true;
        for thread in threads {
            if let Err(err) = self.application.runtime().end_session(&thread).await {
                released = false;
                tracing::warn!(
                    session = id,
                    thread = %thread,
                    error = ?err,
                    "session teardown: sandbox dispose failed (best-effort)"
                );
            }
        }
        released
    }

    async fn release_terminal_resources(
        &self,
        id: &str,
        owner_scope: Option<&str>,
        child_threads: &[SessionThread],
    ) -> bool {
        let Some(mut persisted) = self.application.session_repository().get(id).await else {
            return self.end_session_sandboxes(id, child_threads).await;
        };
        let owner_scope = owner_scope.unwrap_or(DEFAULT_SCOPE);
        let needs_release_intent = persisted.resources.pending.is_none()
            && persisted.resources.activations.iter().any(|activation| {
                activation.state == awaken_session_contract::ActivationState::Active
            });
        if needs_release_intent {
            if let Err(error) = persisted.resources.begin_release() {
                tracing::warn!(session = id, error = ?error, "could not persist resource release intent");
                return false;
            }
            persisted = match self
                .commit_session_snapshot(
                    owner_scope,
                    persisted,
                    "terminal-release-intent",
                    Vec::new(),
                )
                .await
            {
                Ok(session) => session,
                Err(error) => {
                    tracing::warn!(session = id, error = ?error, "could not persist resource release intent");
                    return false;
                }
            };
        }
        if self.end_session_sandboxes(id, child_threads).await
            && self
                .application
                .retire_session_repositories(owner_scope, id, &persisted.resources)
                .await
        {
            persisted
                .resources
                .complete_terminal_release("Session terminated");
            match self
                .commit_session_snapshot(
                    owner_scope,
                    persisted,
                    "terminal-release-complete",
                    Vec::new(),
                )
                .await
            {
                Ok(_) => true,
                Err(error) => {
                    tracing::warn!(session = id, error = ?error, "could not persist resource release completion");
                    false
                }
            }
        } else {
            false
        }
    }

    /// `POST /v1/sessions/{id}/archive` — terminate the session: stamp
    /// `archived_at`, move `status` to `terminated`, and commit a
    /// `session.status_terminated` event so a streaming/listing client observes the
    /// terminal transition (not just the mutated status field). Idempotent: a
    /// re-archive returns the same terminal record without a second event.
    pub async fn archive_session(&self, id: &str) -> Result<Session, StateError> {
        self.ensure_session_for_terminal_cleanup(id).await?;
        let (mut newly_terminated, child_threads) = {
            let sessions = self.sessions.lock().unwrap();
            let record = sessions.get(id).ok_or(StateError::NotFound)?;
            (
                record.session.archived_at.is_none(),
                record.child_threads.clone(),
            )
        };
        let owner = self.resolve_owner(id).await;
        let terminated_fact = lifecycle_fact(
            format!("session:{id}:terminated"),
            id,
            owner.clone(),
            lifecycle_event::SESSION_TERMINATED,
        );
        if newly_terminated {
            let mut persisted = self
                .application
                .session_repository()
                .get(id)
                .await
                .ok_or(StateError::NotFound)?;
            if persisted.status == "terminated" {
                newly_terminated = false;
            } else {
                persisted.status = "terminated".into();
                persisted.archived_at = Some(PROCESSED_AT.into());
                self.commit_session_snapshot(
                    owner.as_deref().unwrap_or(DEFAULT_SCOPE),
                    persisted,
                    "archive",
                    vec![terminated_fact.clone()],
                )
                .await?;
            }
        }
        let session = {
            let terminated_id = self.next_event_id();
            let mut sessions = self.sessions.lock().unwrap();
            let record = sessions.get_mut(id).ok_or(StateError::NotFound)?;
            if newly_terminated {
                record.session.archived_at = Some(PROCESSED_AT.to_string());
                record.session.status = "terminated";
                record.events.push(Event {
                    id: terminated_id,
                    kind: OutboundKind::SessionStatusTerminated {},
                    processed_at: Some(PROCESSED_AT.to_string()),
                });
            }
            record.session_projection()
        };
        // Archive is terminal (no further turns run on this session), so reap its
        // sandbox — but only on the transition, so a re-archive (idempotent) does
        // not re-dispose. The record survives as a tombstone; only the sandbox goes.
        let release_required = newly_terminated
            || self
                .application
                .session_repository()
                .get(id)
                .await
                .is_some_and(|persisted| {
                    persisted.resources.pending.is_some()
                        || persisted.resources.activations.iter().any(|activation| {
                            activation.state == awaken_session_contract::ActivationState::Active
                        })
                });
        if release_required
            && !self
                .release_terminal_resources(id, owner.as_deref(), &child_threads)
                .await
        {
            return Err(StateError::Run(RunError::internal(format!(
                "Session `{id}` terminal resources could not be released"
            ))));
        }
        // Project the terminal transition as a lifecycle fact, mirroring create's
        // `session.status_idled`. The owning workspace is resolved from the session's
        // persisted owner (the archive edge carries only the id) so a subscription in
        // that workspace is matched even after a restart lost the in-memory index.
        if newly_terminated && let Some(sink) = &self.application.lifecycle_sink() {
            sink.emit_fact(
                &terminated_fact.id,
                id,
                owner.as_deref(),
                lifecycle_event::SESSION_TERMINATED,
            )
            .await;
        }
        Ok(session)
    }
}
