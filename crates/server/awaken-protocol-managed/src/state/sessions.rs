//! Session lifecycle for [`ManagedState`]: create, rehydrate, get/list,
//! update, delete, and archive.

use super::application::initial_mcp_candidates;
use super::*;

use super::session_mcp_projection::typed_mcp_servers;
use crate::types::AgentRef;

impl ManagedState {
    pub(super) const fn wire_session_status(execution: SessionExecutionState) -> SessionStatus {
        match execution {
            SessionExecutionState::Preparing | SessionExecutionState::Activating => {
                SessionStatus::Rescheduling
            }
            SessionExecutionState::ActivationFailed => SessionStatus::Terminated,
            SessionExecutionState::Running => SessionStatus::Running,
            SessionExecutionState::Rescheduling => SessionStatus::Rescheduling,
            SessionExecutionState::Idle => SessionStatus::Idle,
            SessionExecutionState::Terminated => SessionStatus::Terminated,
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
        record.session.status = Self::wire_session_status(persisted.execution);
        record.session.title = persisted.title.clone();
        record.session.metadata = persisted.metadata.clone();
        record.session.deployment_id = persisted.metadata.get("awaken.deployment_id").cloned();
        record.session.archived_at = persisted.archived_at().map(str::to_owned);
        record.session.agent.tools = crate::project::managed_tools(&persisted.tools);
        record.session.agent.mcp_servers = mcp_servers;
        record.session.budget = persisted
            .budget
            .max_list_cost_minor()
            .map(crate::types::BudgetLimit::from_minor);
        record.resource_state = persisted.resources.clone();
        Ok(())
    }

    /// Wire-cache adapter around the Session application's sole root CAS. Every
    /// specialized command crosses the application boundary first; only its
    /// committed result is projected into the disposable Managed cache.
    #[cfg(test)]
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

    pub(super) fn map_application_mutation_error(
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

    fn map_creation_error(error: awaken_session_application::SessionCreationError) -> StateError {
        match error {
            awaken_session_application::SessionCreationError::Conflict => StateError::Conflict,
            awaken_session_application::SessionCreationError::IdempotencyMismatch => {
                StateError::IdempotencyMismatch
            }
            awaken_session_application::SessionCreationError::Rejected(error) => {
                StateError::Run(error)
            }
            awaken_session_application::SessionCreationError::Unavailable(message) => {
                StateError::Run(RunError::unavailable(message))
            }
        }
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
    pub async fn check_bind(
        &self,
        workspace_id: &str,
        req: &SessionCreateParams,
    ) -> Result<(), StateError> {
        if let Some(vault_id) = self
            .application
            .missing_vault(workspace_id, &req.vault_ids)
            .await
            .map_err(StateError::Run)?
        {
            return Err(StateError::VaultNotFound(vault_id));
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
            session.status = SessionStatus::Running;
        }
        Ok(session)
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
        let owner_scope = workspace_id
            .clone()
            .unwrap_or_else(|| DEFAULT_SCOPE.to_string());
        self.check_bind(&owner_scope, &req).await?;
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
                let repository_absent = match self.application.session(&candidate).await {
                    Err(awaken_session_contract::SessionRepositoryError::NotFound) => true,
                    Ok(_) => false,
                    Err(error) => return Err(StateError::from(error)),
                };
                if !self
                    .application
                    .runtime_owns_thread(&candidate)
                    .await
                    .map_err(StateError::Run)?
                    && repository_absent
                {
                    break candidate;
                }
            },
        };
        let agent_id = req.agent.id().to_string();
        let config_view = self.application.session_profile(&owner_scope, &agent_id);
        let is_built_in_dream_agent = agent_id == awaken_dream_application::BUILT_IN_DREAM_AGENT_ID
            && req
                .metadata
                .get("awaken.session.origin")
                .is_some_and(|origin| origin == "dream");
        if config_view.is_none()
            && self.application.agent_unavailable(&owner_scope, &agent_id)
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
            ModelOverride::Absent => config_view.as_ref().and_then(|view| {
                view.model
                    .clone()
                    .map(|id| ModelConfig::from_inference(id, view.inference.clone()))
            }),
        };
        // Echo the agent version the client pinned (or overrode over), defaulting to 1.
        let agent_version = req.agent.version().unwrap_or(1);
        let delegate_ids = config_view
            .as_ref()
            .map(|view| view.delegate_ids.clone())
            .unwrap_or_default();
        let selected_geo = selected_model
            .as_ref()
            .and_then(|model| model.inference_geo.as_deref());
        for delegate_id in &delegate_ids {
            let delegate = self
                .application
                .session_profile(&owner_scope, delegate_id)
                .ok_or_else(|| {
                    StateError::Run(RunError::bad_request(format!(
                        "multiagent_unavailable: Agent `{delegate_id}` has no executable profile"
                    )))
                })?;
            if delegate
                .inference
                .inference_geo
                .map(|geography| geography.as_str())
                != selected_geo
            {
                return Err(StateError::Run(RunError::bad_request(format!(
                    "multiagent_inference_geo_mismatch: coordinator is {:?}, Agent `{delegate_id}` is {:?}",
                    selected_geo, delegate.inference.inference_geo
                ))));
            }
        }
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
                &owner_scope,
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
        // Sole protocol-neutral Session input resolution point. Environment
        // selection has already produced one exact snapshot above; the final
        // SessionCreationIntent is the only value that combines and freezes both
        // families. Runtime never re-opens Agent or Resource stores.
        let mut resolved_resources = self
            .application
            .resolve_session_inputs(&owner_scope, agent_defaults, &attachments)
            .map_err(StateError::Run)?;
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
        self.application
            .validate_session_skill_total(
                &owner_scope,
                &agent_id,
                config_view.as_ref(),
                effective_skills.as_deref().unwrap_or_default(),
            )
            .map_err(StateError::Run)?;
        if let Some(skills) = &effective_skills {
            resolved_resources.skills = Some(
                self.application
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
        let caps = self.application.capabilities_for(&id);
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
            .unwrap_or_else(|| ModelConfig::new(self.application.model()));
        let execution_model_ref = config_view
            .as_ref()
            .and_then(|view| view.execution_model_ref.clone())
            .unwrap_or_else(|| resolved_model.id.clone());
        let budget_state = match &req.budget {
            Some(budget) => {
                let max_list_cost_minor = budget
                    .max_list_cost_minor()
                    .map_err(|message| StateError::Run(RunError::bad_request(message)))?;
                let occurred_at_unix_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let model_refs = self
                    .application
                    .managed_session_model_refs(&owner_scope, &agent_id, &execution_model_ref)
                    .map_err(StateError::Run)?;
                let snapshot = self
                    .application
                    .resolve_managed_list_price_snapshot(
                        awaken_session_contract::ManagedListPriceRequest {
                            occurred_at_unix_ms,
                            model_refs,
                        },
                    )
                    .await
                    .map_err(|error| {
                        let run = match error {
                            awaken_session_contract::ManagedListPriceError::Unavailable(_) => {
                                RunError::unavailable(error.to_string())
                            }
                            _ => RunError::bad_request(error.to_string()),
                        };
                        StateError::Run(run)
                    })?;
                awaken_session_contract::SessionBudgetState::active(max_list_cost_minor, snapshot)
            }
            None => awaken_session_contract::SessionBudgetState::Absent,
        };
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
        };
        // The adapter has finished lowering wire policy. The Session application
        // now exclusively orders every durable mutation and external projection.
        let persisted = self
            .application
            .create_session(awaken_session_application::CreateSessionCommand {
                owner_scope: owner_scope.clone(),
                session_id: id.clone(),
                intent: creation_intent,
                title: req.title.clone(),
                metadata: req.metadata.clone(),
                tools: effective_tools.clone(),
                budget: budget_state,
            })
            .await
            .map_err(Self::map_creation_error)?;
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
                    |view| {
                        project::agent_multiagent_roster(
                            &view.delegate_ids,
                            view.advisor_model.as_deref(),
                        )
                    },
                ),
            },
            budget: persisted
                .budget
                .max_list_cost_minor()
                .map(crate::types::BudgetLimit::from_minor),
            environment_id: environment_id.clone(),
            created_at: PROCESSED_AT.to_string(),
            updated_at: PROCESSED_AT.to_string(),
            archived_at: None,
            title: req.title,
            metadata: req.metadata,
            // Resource wire values are projected from `resource_state` on response.
            resources: Vec::new(),
            outcome_evaluations: Vec::new(),
            // The durable application aggregate is the sole lifecycle truth.
            // A registered-Worker placement remains preparing until its claimed
            // realization acknowledges the exact frozen projection; hardcoding
            // non-Application creation to idle created a second, unsafe status.
            status: Self::wire_session_status(persisted.execution),
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
        self.owners.lock().unwrap().insert(id.clone(), owner_scope);
        let record = SessionRecord::new(
            agent_id,
            session,
            persisted.resources,
            Vec::new(),
            Default::default(),
        );
        let session = record.session_projection();
        self.sessions.lock().unwrap().insert(id.clone(), record);
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
    pub async fn resolve_owner(&self, session_id: &str) -> Result<Option<String>, StateError> {
        if let Some(scope) = self.owner_scope(session_id) {
            return Ok(Some(scope));
        }
        match self.application.owner(session_id).await {
            Ok(owner) => Ok(Some(owner)),
            Err(awaken_session_application::SessionMutationError::NotFound) => Ok(None),
            Err(error) => Err(Self::map_application_mutation_error(error)),
        }
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
        let caps = self.application.capabilities_for(id);
        let projected_budget = persisted
            .as_ref()
            .and_then(|session| session.budget.max_list_cost_minor())
            .map(crate::types::BudgetLimit::from_minor);
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
                            self.application.model(),
                            p.environment_id().to_string(),
                        )
                    });
                let mcp_servers = typed_mcp_servers(p.visible_mcp_servers());
                let archived_at = p.archived_at().map(str::to_owned);
                (
                    agent_id,
                    model,
                    environment_id,
                    p.title,
                    p.metadata,
                    project::managed_tools(&p.tools),
                    mcp_servers,
                    Self::wire_session_status(p.execution),
                    archived_at,
                )
            }
            None => (
                "assistant".to_string(),
                self.application.model(),
                "env_local".to_string(),
                None,
                Default::default(),
                default_tools,
                Vec::new(),
                SessionStatus::Idle,
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
            budget: projected_budget,
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
            .session(id)
            .await
            .map_err(StateError::from)?;
        if persisted.is_hidden() {
            return Err(StateError::NotFound);
        }
        let owner_scope = self
            .application
            .owner(id)
            .await
            .map_err(Self::map_application_mutation_error)?;
        let delegated_runs = self
            .application
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
            .session(id)
            .await
            .map(|session| session.revision)
            .map_err(StateError::from)
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
        // A terminal command must work after a process restart without realizing
        // the soon-to-be-released Environment or MCP attachments.
        self.ensure_session_for_terminal_cleanup(id).await?;
        // Do not remove the visible record until the repository has atomically
        // stored the terminal fence and outbox fact. Child cleanup targets are
        // frozen later from durable Runtime delegation authority.
        let owner = self.resolve_owner(id).await?;
        let deleted_fact = lifecycle_fact(
            format!("session:{id}:deleted"),
            id,
            owner.clone(),
            lifecycle_event::SESSION_DELETED,
        );
        let transition = self
            .application
            .begin_delete(id, deleted_fact.clone())
            .await
            .map_err(Self::map_preparation_error)?;
        self.refresh_cached_projection(&transition.session)?;

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
        if let Err(error) = self
            .application
            .release_terminal_resources(&transition.owner_scope, id)
            .await
        {
            tracing::warn!(
                session = id,
                error = ?error,
                "Session delete cleanup remains pending for application recovery"
            );
        }
        // Project the deletion as a lifecycle fact so a webhook subscriber is
        // notified, mirroring create's `session.status_idled` and archive's
        // `session.status_terminated`. The owner is resolved from the persisted
        // owner (the delete edge carries only the id).
        self.application.notify_lifecycle_fact();
        Ok(())
    }

    /// `POST /v1/sessions/{id}/archive` — terminate the session: stamp
    /// `archived_at`, move `status` to `terminated`, and commit a
    /// `session.status_terminated` event so a streaming/listing client observes the
    /// terminal transition (not just the mutated status field). Idempotent: a
    /// re-archive returns the same terminal record without a second event.
    pub async fn archive_session(&self, id: &str) -> Result<Session, StateError> {
        self.ensure_session_for_terminal_cleanup(id).await?;
        let owner = self.resolve_owner(id).await?;
        let terminated_fact = lifecycle_fact(
            format!("session:{id}:terminated"),
            id,
            owner.clone(),
            lifecycle_event::SESSION_TERMINATED,
        );
        let transition = self
            .application
            .terminate_session(id, PROCESSED_AT, terminated_fact.clone())
            .await
            .map_err(Self::map_preparation_error)?;
        self.refresh_cached_projection(&transition.session)?;
        let newly_terminated = transition.transitioned;
        let session = {
            let terminated_id = self.next_event_id();
            let mut sessions = self.sessions.lock().unwrap();
            let record = sessions.get_mut(id).ok_or(StateError::NotFound)?;
            if newly_terminated {
                record.events.push(Event {
                    id: terminated_id,
                    kind: OutboundKind::SessionStatusTerminated {},
                    processed_at: Some(PROCESSED_AT.to_string()),
                });
            }
            record.session_projection()
        };
        // Project the terminal transition as a lifecycle fact, mirroring create's
        // `session.status_idled`. The owning workspace is resolved from the session's
        // persisted owner (the archive edge carries only the id) so a subscription in
        // that workspace is matched even after a restart lost the in-memory index.
        Ok(session)
    }
}
