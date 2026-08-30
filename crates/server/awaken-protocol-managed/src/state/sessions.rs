//! Session lifecycle for [`ManagedState`]: create, rehydrate, get/list,
//! update, delete, and archive.

use super::application::initial_mcp_candidates;
use super::*;

use super::session_mcp_projection::typed_mcp_servers;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RehydrationPurpose {
    Interactive,
    CollectionRead,
    FrozenControl,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RehydrationPublicationDecision {
    Available,
    Unpinned,
    NonInteractiveFrozenProjection,
    RejectMissingExact,
}

/// Decide whether projection recovery may proceed without a catalog profile.
/// Interactive recovery remains fail-closed. Collection reads and controls over
/// already-frozen work may project only the Session's durable baseline; neither
/// path restores a Runtime or makes the missing publication executable.
#[must_use]
const fn rehydration_publication_decision(
    purpose: RehydrationPurpose,
    has_frozen_revision: bool,
    profile_available: bool,
) -> RehydrationPublicationDecision {
    if profile_available {
        RehydrationPublicationDecision::Available
    } else {
        match (purpose, has_frozen_revision) {
            (RehydrationPurpose::Interactive, true) => {
                RehydrationPublicationDecision::RejectMissingExact
            }
            (RehydrationPurpose::CollectionRead | RehydrationPurpose::FrozenControl, true) => {
                RehydrationPublicationDecision::NonInteractiveFrozenProjection
            }
            (_, false) => RehydrationPublicationDecision::Unpinned,
        }
    }
}

#[cfg(kani)]
#[kani::proof]
fn missing_agent_publication_is_available_only_to_noninteractive_projection() {
    let purpose_discriminant: u8 = kani::any();
    kani::assume(purpose_discriminant < 3);
    let has_frozen_revision: bool = kani::any();
    let profile_available: bool = kani::any();
    let purpose = match purpose_discriminant {
        0 => RehydrationPurpose::Interactive,
        1 => RehydrationPurpose::CollectionRead,
        _ => RehydrationPurpose::FrozenControl,
    };
    let decision =
        rehydration_publication_decision(purpose, has_frozen_revision, profile_available);

    assert_eq!(
        decision == RehydrationPublicationDecision::NonInteractiveFrozenProjection,
        purpose != RehydrationPurpose::Interactive && has_frozen_revision && !profile_available
    );
    assert_eq!(
        decision == RehydrationPublicationDecision::RejectMissingExact,
        purpose == RehydrationPurpose::Interactive && has_frozen_revision && !profile_available
    );
}

impl ManagedState {
    fn resolved_session_multiagent(
        &self,
        workspace_id: &str,
        profile: Option<&awaken_executable_agent_contract::ExecutableAgentSessionProfile>,
        caps: &AgentCapabilities,
    ) -> Result<Option<crate::types::SessionMultiagentCoordinator>, StateError> {
        let delegates = profile.map_or_else(
            || {
                caps.delegates
                    .iter()
                    .cloned()
                    .map(
                        |agent_id| awaken_executable_agent_contract::ExecutableAgentDelegate {
                            agent_id,
                            source_revision: None,
                        },
                    )
                    .collect()
            },
            |profile| profile.delegates.clone(),
        );
        let advisor_model = profile.and_then(|profile| profile.advisor_model.clone());
        if delegates.is_empty() && advisor_model.is_none() {
            return Ok(None);
        }
        let mut agents = Vec::with_capacity(delegates.len() + usize::from(advisor_model.is_some()));
        for delegate in delegates {
            let resolved = delegate
                .source_revision
                .and_then(|revision| {
                    self.application.session_profile_at_revision(
                        workspace_id,
                        &delegate.agent_id,
                        revision,
                    )
                })
                .or_else(|| {
                    self.application
                        .session_profile(workspace_id, &delegate.agent_id)
                });
            let child = match resolved {
                Some(profile) => Self::thread_agent_from_profile(&delegate.agent_id, profile),
                None if profile.is_none() => {
                    Self::thread_agent_from_profile(&delegate.agent_id, Default::default())
                }
                None => {
                    return Err(StateError::Run(RunError::bad_request(format!(
                        "multiagent_unavailable: Agent `{}` publication revision {:?} is unavailable",
                        delegate.agent_id, delegate.source_revision
                    ))));
                }
            };
            agents.push(crate::types::SessionMultiagentRosterEntry::Agent(child));
        }
        if let Some(model) = advisor_model {
            agents.push(crate::types::SessionMultiagentRosterEntry::Advisor(
                crate::types::agent::AdvisorRosterEntry {
                    model,
                    kind: crate::types::agent::AdvisorRosterEntryKind::Advisor,
                },
            ));
        }
        Ok(Some(crate::types::SessionMultiagentCoordinator {
            kind: "coordinator",
            agents,
        }))
    }

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

    /// Apply durable Session-root fields to a private projection candidate.
    /// Publication belongs exclusively to `publish_projection_candidate`, so
    /// callers cannot partially mutate the live cache before Thread reads pass.
    pub(super) fn apply_persisted_session(
        record: &mut SessionRecord,
        persisted: &PersistedSession,
    ) {
        let mcp_servers = typed_mcp_servers(persisted.configured_mcp_servers());
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
        record.checkpoint.source.session_revision = persisted.revision;
    }

    /// Publish only Session-root fields after its durable CAS. This is a
    /// candidate builder, not another writer: full Thread/lifecycle reduction
    /// and root-only visibility both converge on the sole CAS publisher.
    pub(super) fn publish_persisted_session(
        &self,
        persisted: &PersistedSession,
    ) -> Result<(), StateError> {
        let result = self.publish_projection_update(&persisted.session_id, |candidate| {
            if persisted.revision >= candidate.checkpoint.source.session_revision {
                Self::apply_persisted_session(candidate, persisted);
            }
            Ok(())
        });
        match result {
            // A cold or control-only adapter has no disposable wire row to
            // update. Durable truth remains committed; rehydration constructs
            // the initial candidate later.
            Err(StateError::NotFound) => Ok(()),
            other => other,
        }
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
        self.publish_persisted_session(&session)?;
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

    pub(crate) fn map_creation_error(
        error: awaken_session_application::SessionCreationError,
    ) -> StateError {
        match error {
            awaken_session_application::SessionCreationError::Conflict => StateError::Conflict,
            awaken_session_application::SessionCreationError::Tombstoned => {
                StateError::TerminalCreateConflict
            }
            awaken_session_application::SessionCreationError::IdempotencyMismatch => {
                StateError::IdempotencyMismatch
            }
            awaken_session_application::SessionCreationError::Rejected(error) => {
                StateError::Run(error)
            }
            awaken_session_application::SessionCreationError::Unavailable(message) => {
                StateError::Run(RunError::unavailable(message))
            }
            awaken_session_application::SessionCreationError::Internal(message) => {
                StateError::Run(RunError::internal(message))
            }
        }
    }

    pub(super) fn map_create_replay_error(
        error: awaken_session_application::SessionCreationError,
    ) -> StateError {
        // An occupied identity without this exact profiled receipt and a foreign
        // owner are both deterministic-request mismatches at preflight. Fresh
        // create races retain the ordinary Conflict classification; terminal,
        // outage, and corrupt states keep the canonical creation mapping.
        match error {
            awaken_session_application::SessionCreationError::Conflict => {
                StateError::IdempotencyMismatch
            }
            error => Self::map_creation_error(error),
        }
    }

    /// `POST /v1/sessions`.
    ///
    /// MCP binding (ADR-0043 Phase 3): each requested server is bound to a vault
    /// credential by exact `mcp_server_url` match across the request's
    /// `vault_ids`. The preparation intent, frozen generation-1 state, and exact
    /// realization claim all commit before the complete frozen projection port
    /// performs external I/O. A failed realization leaves recoverable failed
    /// state and fails the create (the router maps the `RunError` to the error
    /// envelope). A `vault_ids` entry that names no
    /// existing vault fails the create closed too:
    /// a 404 naming the vault id, BEFORE anything is provisioned — never a
    /// silent no-binding whose 401 only surfaces at the first Run. (Without a
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

    pub async fn accept_session(
        &self,
        req: SessionCreateParams,
        workspace_id: Option<String>,
    ) -> Result<Session, StateError> {
        self.accept_session_with_identity(req, workspace_id, None)
            .await
    }

    pub(super) async fn create_session_with_identity(
        &self,
        req: SessionCreateParams,
        workspace_id: Option<String>,
        explicit_id: Option<String>,
    ) -> Result<Session, StateError> {
        self.create_session_with_identity_from(req, workspace_id, explicit_id, None, false)
            .await
    }

    pub(super) async fn accept_session_with_identity(
        &self,
        req: SessionCreateParams,
        workspace_id: Option<String>,
        explicit_id: Option<String>,
    ) -> Result<Session, StateError> {
        self.create_session_with_identity_from(req, workspace_id, explicit_id, None, true)
            .await
    }

    /// One create owner for Session and Deployment wire unions. `Some` carries
    /// the already-lowered Deployment union; ordinary Session create uses the
    /// request's narrower `initial_events` field.
    pub(super) async fn create_session_with_identity_from(
        &self,
        mut req: SessionCreateParams,
        workspace_id: Option<String>,
        explicit_id: Option<String>,
        deployment_initial_events: Option<Vec<InboundEvent>>,
        accept_durable_root: bool,
    ) -> Result<Session, StateError> {
        req.validate_common()
            .map_err(|message| StateError::Run(RunError::bad_request(message)))?;
        // Initial-event admission is atomic with Session creation: validate the
        // complete batch before bind checks, identity allocation, persistence, or
        // Runtime preparation. The shared validator is also used by Deployments.
        let initial_events = if let Some(events) = deployment_initial_events {
            if !req.initial_events.is_empty() {
                return Err(StateError::Run(RunError::internal(
                    "Deployment initial Events must have one lowering owner",
                )));
            }
            crate::types::initial_event::validate_deployment_inbound_initial_events(&events)
                .map_err(|message| StateError::Run(RunError::bad_request(message)))?;
            events
        } else {
            req.validate_initial_events()
                .map_err(|message| StateError::Run(RunError::bad_request(message)))?;
            req.initial_events.clone()
        };
        let owner_scope = workspace_id
            .clone()
            .unwrap_or_else(|| DEFAULT_SCOPE.to_string());
        // Resource-path admission is the first domain operation after the edge
        // authorization check. Parse and validate the complete explicit
        // Repository set before refreshing catalogs or invoking model,
        // Environment, MCP, Skill, File, Vault, Registry, Runtime, or Git ports.
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
        super::resource::preflight_repository_mount_paths(&resources)?;
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
        self.application
            .refresh_executable_projections()
            .await
            .map_err(|error| {
                StateError::Run(RunError::unavailable_classified(
                    "executable_projection_refresh_failed",
                    format!("Executable projections could not be refreshed: {error}"),
                ))
            })?;
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
        let initial_events =
            crate::types::initial_event::compile_session_initial_event_plan(&id, &initial_events)
                .map_err(|message| StateError::Run(RunError::bad_request(message)))?;
        let requested_agent_version = req.agent.version();
        let config_view = requested_agent_version
            .and_then(|version| {
                self.application
                    .session_profile_at_revision(&owner_scope, &agent_id, version)
            })
            .or_else(|| {
                requested_agent_version
                    .is_none()
                    .then(|| self.application.session_profile(&owner_scope, &agent_id))
                    .flatten()
            });
        if let Some(version) = requested_agent_version
            && config_view.is_none()
            && self.application.has_agent_profile_source()
        {
            return Err(StateError::Run(RunError::bad_request(format!(
                "agent_version_unavailable: Agent `{agent_id}` version {version} is unavailable"
            ))));
        }
        if let Some(requested_agent_version) = requested_agent_version
            && config_view.is_none()
        {
            return Err(StateError::Run(RunError::bad_request(format!(
                "agent_version_unavailable: agent `{agent_id}` has no executable publication at version {requested_agent_version}"
            ))));
        }
        if config_view.is_none()
            && requested_agent_version.is_none()
            && self.application.agent_unavailable(&owner_scope, &agent_id)
        {
            return Err(StateError::Run(RunError::bad_request(format!(
                "agent_unavailable: agent `{agent_id}` cannot start a new session"
            ))));
        }
        let agent_defaults = config_view
            .as_ref()
            .map(|view| view.resources.as_slice())
            .unwrap_or_default();
        let prepared_resources =
            self.prepare_session_input_attachments(&id, &owner_scope, &resources, agent_defaults)?;
        // Cause/effect decision table for Session model authority: R1 absent ->
        // inherit the Agent publication; R2 equal override -> reuse its route and
        // replace inference controls;
        // R3 different override + resolver -> freeze the complete newly resolved
        // publication; R4 different override + unavailable/invalid resolver ->
        // fail before Session persistence. A public id is never stitched onto the
        // Agent's old backend, endpoint, or credential provisioning.
        let published_model = config_view
            .as_ref()
            .and_then(|view| view.model.clone())
            .unwrap_or_else(|| self.application.model());
        let (selected_model, model_override): (
            Option<ModelConfig>,
            Option<awaken_session_contract::SessionModelOverride>,
        ) = match req.agent.model_override() {
            ModelOverride::Set(cfg) => {
                let inference = cfg.inference_options();
                let model_override = self
                    .application
                    .resolve_session_model_override(
                        &owner_scope,
                        &cfg.id,
                        &published_model,
                        inference,
                    )
                    .await
                    .map_err(StateError::Run)?;
                (Some(cfg), Some(model_override))
            }
            ModelOverride::Absent => (
                config_view.as_ref().and_then(|view| {
                    view.model
                        .clone()
                        .map(|id| ModelConfig::from_inference(id, view.inference.clone()))
                }),
                None,
            ),
        };
        self.authorize_inference_geo(
            &owner_scope,
            selected_model
                .as_ref()
                .and_then(|model| model.inference_geo),
            crate::InferenceGeoCheckpoint::SessionCreate,
        )
        .await?;
        // Echo the agent version the client pinned (or overrode over), defaulting to 1.
        let agent_version = requested_agent_version
            .or_else(|| config_view.as_ref().map(|profile| profile.source_revision))
            .unwrap_or(1)
            .max(1);
        let delegate_ids: Vec<String> = config_view
            .as_ref()
            .map(|view| {
                view.delegates
                    .iter()
                    .map(|delegate| delegate.agent_id.clone())
                    .collect()
            })
            .unwrap_or_default();
        let selected_geo = selected_model
            .as_ref()
            .and_then(|model| model.inference_options().inference_geo);
        for delegate_ref in config_view
            .as_ref()
            .into_iter()
            .flat_map(|profile| profile.delegates.iter())
        {
            let delegate_id = &delegate_ref.agent_id;
            let delegate = delegate_ref
                .source_revision
                .and_then(|revision| {
                    self.application.session_profile_at_revision(
                        &owner_scope,
                        delegate_id,
                        revision,
                    )
                })
                .or_else(|| self.application.session_profile(&owner_scope, delegate_id))
                .ok_or_else(|| {
                    StateError::Run(RunError::bad_request(format!(
                        "multiagent_unavailable: Agent `{delegate_id}` has no executable profile"
                    )))
                })?;
            if delegate.inference.inference_geo != selected_geo {
                return Err(StateError::Run(RunError::bad_request(format!(
                    "multiagent_inference_geo_mismatch: coordinator is {:?}, Agent `{delegate_id}` is {:?}",
                    selected_geo, delegate.inference.inference_geo
                ))));
            }
        }
        // Freeze the root execution route before any Resource or Session write.
        // For budgeted Sessions, the application's single roster compiler also
        // validates that the root, exact ordinary roster revisions, and the
        // Advisor route all execute inside the Native per-request gate.
        let resolved_model = selected_model
            .clone()
            .unwrap_or_else(|| ModelConfig::new(self.application.model()));
        let execution_model_ref = model_override
            .as_ref()
            .and_then(|model_override| model_override.publication.as_ref())
            .map(|publication| publication.primary.binding().model_ref.clone())
            .or_else(|| {
                config_view
                    .as_ref()
                    .and_then(|view| view.execution_model_ref.clone())
            })
            .unwrap_or_else(|| resolved_model.id.clone());
        let published_backend_ref = model_override
            .as_ref()
            .and_then(|model_override| model_override.publication.as_ref())
            .map(|publication| publication.primary.binding().backend_ref.clone())
            .or_else(|| {
                config_view
                    .as_ref()
                    .map(|view| view.backend_ref.clone())
                    .filter(|backend_ref| !backend_ref.trim().is_empty())
            });
        let budget_model_refs = req
            .budget
            .as_ref()
            .map(|_| {
                self.application.managed_session_model_refs(
                    &owner_scope,
                    &agent_id,
                    config_view.as_ref(),
                    &execution_model_ref,
                    published_backend_ref.as_deref(),
                    model_override
                        .as_ref()
                        .and_then(|model_override| model_override.publication.as_deref()),
                )
            })
            .transpose()
            .map_err(StateError::Run)?;
        let agent_mcp_override = req.agent.mcp_servers_override();
        let (mcp_candidates, mcp_targets) =
            awaken_session_application::SessionApplication::normalize_mcp_candidate_targets(
                initial_mcp_candidates(config_view.as_ref(), agent_mcp_override),
            )?;
        // Resolve the SDK-required Environment and networking policy once, for both
        // SessionInit and the echoed Session object. Managed never invokes the
        // native application's optional local-environment fallback.
        let agent_environment = config_view
            .as_ref()
            .and_then(|view| view.environment.as_ref());
        let (environment_id, environment) = self
            .resolve_session_environment(
                &req.environment_id,
                agent_environment,
                published_backend_ref.as_deref(),
                &mcp_targets,
            )
            .await?;
        // Cause/effect layout gate: C1 the complete effective typed bindings,
        // frozen Environment, and exact publication-selected provider produce a
        // non-overlapping final Sandbox layout -> continue; C2 any projected
        // File/Memory/Repository, baseline, output, HOME/XDG, or provider mount
        // overlaps -> reject before Vault checks, Skill/File materialization,
        // Repository configuration, root CAS, cache prewarm, or provider I/O.
        self.application
            .validate_session_sandbox_layout(
                &id,
                &awaken_session_contract::SessionSandboxLayout {
                    workspace_id: owner_scope.clone(),
                    agent_id: agent_id.clone(),
                    agent_revision: config_view.as_ref().and_then(|profile| {
                        (profile.source_revision > 0).then_some(profile.source_revision)
                    }),
                    runtime_placement: self.application.runtime_placement(),
                    model_override: model_override.clone(),
                    runtime: published_backend_ref.clone(),
                    mounts: Vec::new(),
                    env: Vec::new(),
                    environment: environment.clone(),
                    resources: prepared_resources.effective_bindings().to_vec(),
                },
            )
            .map_err(StateError::Run)?;
        self.check_bind(&owner_scope, &req).await?;
        let mcp_drafts = self
            .application
            .normalize_mcp_drafts(
                &owner_scope,
                mcp_candidates,
                &req.vault_ids,
                &environment.credential_realization.mcp_holder,
            )
            .await?;
        let effective_skills = req
            .agent
            .skills_override()
            .map(|skills| {
                skills
                    .iter()
                    .cloned()
                    .map(crate::types::agent::AgentSkill::into_binding)
                    .collect::<Vec<_>>()
            })
            .or_else(|| config_view.as_ref().map(|view| view.skills.clone()));
        self.application
            .validate_session_skill_total(
                &owner_scope,
                &agent_id,
                config_view.as_ref(),
                effective_skills.as_deref().unwrap_or_default(),
            )
            .map_err(StateError::Run)?;
        let resolved_skills = match &effective_skills {
            Some(skills) => Some(
                self.application
                    .resolve_session_skills(&owner_scope, skills)
                    .await
                    .map_err(StateError::Run)?,
            ),
            None => None,
        };
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
        let effective_tools = req
            .agent
            .tools_override()
            .map(project::session_tool_configuration)
            .unwrap_or(inherited_tools);
        // Validate after replacement/inheritance has produced the effective
        // Session configuration. Checking only when both override arrays were
        // present admitted a partial override whose inherited half named a
        // different server.
        let effective_server_names = mcp_drafts
            .iter()
            .map(|draft| draft.name.clone())
            .collect::<Vec<_>>();
        let effective_toolset_names = effective_tools
            .toolsets
            .iter()
            .filter_map(|toolset| match &toolset.source {
                awaken_runtime_contract::agent_bindings::ToolsetSource::Mcp { server_name } => {
                    Some(server_name.clone())
                }
                awaken_runtime_contract::agent_bindings::ToolsetSource::Agent => None,
            })
            .collect::<Vec<_>>();
        awaken_session_contract::validate_mcp_toolset_pairing(
            &effective_server_names,
            &effective_toolset_names,
        )
        .map_err(|message| StateError::Run(RunError::bad_request(message)))?;
        let budget_state = match &req.budget {
            Some(budget) => {
                let max_list_cost_minor = budget
                    .max_list_cost_minor()
                    .map_err(|message| StateError::Run(RunError::bad_request(message)))?;
                let occurred_at_unix_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let model_refs =
                    budget_model_refs.expect("budgeted Session compiled its exact model roster");
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
        // Cause/effect rules for Session-owned Repository participants:
        // R1 all pure Environment/Skill/tool/budget validation succeeds ->
        // enter Registry/Vault; R2 lowering fails -> compensate only prior
        // Applied participants; R3 later resolve/pin fails -> compensate every
        // Applied participant; R4 root Applied/Replayed -> creation owns the
        // participants and terminal reconciliation becomes the only retire path.
        let (attachments, repository_configurations) = self
            .lower_prepared_session_input_attachments(&id, &owner_scope, &prepared_resources)
            .await?;
        // Sole protocol-neutral Session input resolution point. Environment
        // selection has already produced one exact snapshot above; the final
        // SessionCreationIntent is the only value that combines and freezes both
        // families. Runtime never re-opens Agent or Resource stores.
        let mut resolved_resources = match self.application.resolve_session_inputs(
            &owner_scope,
            agent_defaults,
            &attachments,
        ) {
            Ok(resources) => resources,
            Err(first) => {
                if !self
                    .application
                    .abort_unadopted_session_repositories(&repository_configurations)
                    .await
                {
                    tracing::warn!(
                        session = %id,
                        "Managed Session Repository compensation remains pending after input resolution"
                    );
                }
                return Err(StateError::Run(first));
            }
        };
        if let Some(resolved_skills) = resolved_skills {
            resolved_resources = match resolved_resources.with_skills(resolved_skills) {
                Ok(resources) => resources,
                Err(error) => {
                    let first = StateError::Run(RunError::bad_request(error.to_string()));
                    if !self
                        .application
                        .abort_unadopted_session_repositories(&repository_configurations)
                        .await
                    {
                        tracing::warn!(
                            session = %id,
                            "Managed Session Repository compensation remains pending after Skill binding"
                        );
                    }
                    return Err(first);
                }
            };
        }
        if let Err(error) = self
            .application
            .pin_repository_credentials(
                &owner_scope,
                &environment.credential_realization.resource_holder,
                &mut resolved_resources,
            )
            .await
        {
            let first = Self::map_preparation_error(error);
            if !self
                .application
                .abort_unadopted_session_repositories(&repository_configurations)
                .await
            {
                tracing::warn!(
                    session = %id,
                    "Managed Session Repository compensation remains pending after credential pinning"
                );
            }
            return Err(first);
        }
        let creation_intent = awaken_session_contract::SessionCreationIntent {
            control: awaken_session_contract::ControlSessionCreationInputs {
                mutation_policy: awaken_session_contract::SessionMutationPolicy::Managed,
                environment,
                runtime_placement: self.application.runtime_placement(),
                mcp_authoring: awaken_session_contract::SessionMcpAuthoringContext {
                    ordered_vault_ids: req.vault_ids.clone(),
                },
                agent_id: agent_id.clone(),
                agent_revision: config_view.as_ref().and_then(|profile| {
                    (profile.source_revision > 0).then_some(profile.source_revision)
                }),
                model: resolved_model.id.clone(),
                execution_model_ref,
                model_override,
                system_prompt: match req.agent.system_override() {
                    None => awaken_session_contract::SessionSystemPromptSelection::Inherit,
                    Some(None) => awaken_session_contract::SessionSystemPromptSelection::Clear,
                    Some(Some(value)) => {
                        awaken_session_contract::SessionSystemPromptSelection::Replace(
                            value.to_owned(),
                        )
                    }
                },
                runtime: published_backend_ref,
                delegate_ids: delegate_ids.clone(),
                toolsets: effective_tools.toolsets.clone(),
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: Vec::new(),
                transcript_prefix: None,
                resources: resolved_resources,
                initial_mcp: mcp_drafts,
            },
        };
        // The adapter has finished lowering wire policy. The Session application
        // now exclusively orders every durable mutation and external projection.
        let creation = awaken_session_application::CreateSessionCommand {
            owner_scope: owner_scope.clone(),
            session_id: id.clone(),
            intent: creation_intent,
            title: req.title.clone(),
            metadata: req.metadata.clone(),
            tools: effective_tools.clone(),
            budget: budget_state,
            repository_configurations,
            idempotency: None,
            initial_events,
        };
        let persisted = if accept_durable_root {
            Box::pin(self.application.accept_session(creation)).await
        } else {
            Box::pin(self.application.create_session(creation)).await
        }
        .map_err(Self::map_creation_error)?;
        let deployment_id = req.metadata.get("awaken.deployment_id").cloned();
        let session_tools = project::managed_tools(&effective_tools);
        let session_multiagent =
            self.resolved_session_multiagent(&owner_scope, config_view.as_ref(), &caps)?;
        let mut session = Session {
            id: id.clone(),
            kind: "session",
            agent: SessionAgent {
                id: agent_id.clone(),
                kind: "agent",
                version: agent_version,
                // Echo the accepted official override or the published Agent model.
                model: resolved_model,
                name: config_view
                    .as_ref()
                    .and_then(|profile| profile.name.clone())
                    .unwrap_or_else(|| agent_id.clone()),
                description: config_view
                    .as_ref()
                    .and_then(|profile| profile.description.clone()),
                system: config_view.as_ref().and_then(|view| view.system.clone()),
                tools: session_tools,
                // Echo accepted Agent configuration in the SDK shape. Runtime
                // visibility is a separate state and can remain Requested while
                // this immutable Session snapshot is already readable.
                mcp_servers: typed_mcp_servers(persisted.configured_mcp_servers()),
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
                multiagent: session_multiagent,
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
        // clears only `system`; an empty array clears a list field.
        if let Some(system) = req.agent.system_override() {
            session.agent.system = system.map(str::to_owned);
        }
        if let Some(tools) = req.agent.tools_override() {
            if tools.is_empty() && !session.agent.skills.is_empty() {
                return Err(StateError::Run(RunError::bad_request(
                    "cannot clear tools while skills are configured",
                )));
            }
            session.agent.tools = project::resolved_tools(tools);
        }
        self.owners.lock().unwrap().insert(id.clone(), owner_scope);
        let record = SessionRecord::new(
            agent_id,
            session,
            persisted.revision,
            persisted.resources,
            Vec::new(),
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

    /// A disposable wire object reconstructed from one durable Session root.
    /// Omitting that root can never manufacture placeholder identity,
    /// ownership, configuration, or capabilities from Runtime observations.
    pub(crate) fn rehydrated_session(
        &self,
        id: &str,
        owner_scope: &str,
        persisted: PersistedSession,
    ) -> Result<Session, StateError> {
        self.rehydrated_session_for(id, owner_scope, persisted, RehydrationPurpose::Interactive)
    }

    fn rehydrated_session_for(
        &self,
        id: &str,
        owner_scope: &str,
        persisted: PersistedSession,
        purpose: RehydrationPurpose,
    ) -> Result<Session, StateError> {
        let caps = self.application.capabilities_for(id);
        let projected_budget = persisted
            .budget
            .max_list_cost_minor()
            .map(crate::types::BudgetLimit::from_minor);
        let (
            agent_id,
            agent_revision,
            model,
            environment_id,
            title,
            metadata,
            agent_tools,
            mcp_servers,
            model_inference,
            system_prompt,
            agent_skills,
            vault_ids,
            status,
            archived_at,
        ) = {
            let p = persisted;
            let baseline = p.frozen_baseline().ok_or_else(|| {
                StateError::Run(RunError::unavailable(
                    "Session root is not finalized and cannot be rehydrated",
                ))
            })?;
            let (agent_id, agent_revision, model, environment_id) = (
                baseline.agent_id.clone(),
                baseline.agent_revision,
                baseline.model.clone(),
                baseline.environment.environment_id.clone(),
            );
            let model_inference = baseline
                .model_override
                .as_ref()
                .map(|model_override| model_override.inference.clone());
            let system_prompt = baseline.system_prompt.as_ref().clone();
            let agent_skills = p
                .resources
                .desired()
                .skills()
                .iter()
                .map(crate::types::agent::AgentSkill::from_resolved_binding)
                .collect();
            let vault_ids = baseline.mcp_authoring.ordered_vault_ids.clone();
            let mcp_servers = typed_mcp_servers(p.configured_mcp_servers());
            let archived_at = p.archived_at().map(str::to_owned);
            (
                agent_id,
                agent_revision,
                model,
                environment_id,
                p.title,
                p.metadata,
                project::managed_tools(&p.tools),
                mcp_servers,
                model_inference,
                system_prompt,
                agent_skills,
                vault_ids,
                Self::wire_session_status(p.execution),
                archived_at,
            )
        };
        let deployment_id = metadata.get("awaken.deployment_id").cloned();
        let profile = agent_revision
            .and_then(|revision| {
                self.application
                    .session_profile_at_revision(owner_scope, &agent_id, revision)
            })
            .or_else(|| {
                agent_revision
                    .is_none()
                    .then(|| self.application.session_profile(owner_scope, &agent_id))
                    .flatten()
            });
        let publication_decision =
            rehydration_publication_decision(purpose, agent_revision.is_some(), profile.is_some());
        if publication_decision == RehydrationPublicationDecision::RejectMissingExact {
            let revision = agent_revision.expect("missing exact decision requires a revision");
            return Err(StateError::Run(RunError::unavailable(format!(
                "Agent `{agent_id}` publication revision {revision} is unavailable during Session recovery"
            ))));
        }
        let multiagent = match publication_decision {
            RehydrationPublicationDecision::NonInteractiveFrozenProjection => None,
            RehydrationPublicationDecision::Available
            | RehydrationPublicationDecision::Unpinned => {
                self.resolved_session_multiagent(owner_scope, profile.as_ref(), &caps)?
            }
            RehydrationPublicationDecision::RejectMissingExact => {
                unreachable!("missing exact publication was rejected before projection")
            }
        };
        Ok(Session {
            id: id.to_string(),
            kind: "session",
            agent: SessionAgent {
                id: agent_id.clone(),
                kind: "agent",
                version: profile
                    .as_ref()
                    .map(|profile| profile.source_revision)
                    .or(agent_revision)
                    .unwrap_or(1)
                    .max(1),
                model: ModelConfig::from_inference(
                    model,
                    model_inference
                        .or_else(|| profile.as_ref().map(|profile| profile.inference.clone()))
                        .unwrap_or_default(),
                ),
                name: profile
                    .as_ref()
                    .and_then(|profile| profile.name.clone())
                    .unwrap_or_else(|| agent_id.clone()),
                description: profile
                    .as_ref()
                    .and_then(|profile| profile.description.clone()),
                system: system_prompt
                    .resolve(profile.as_ref().and_then(|profile| profile.system.clone())),
                tools: agent_tools,
                mcp_servers,
                skills: agent_skills,
                multiagent,
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
            vault_ids,
            deployment_id,
        })
    }

    /// Rebuild only the disposable projection needed to control already-frozen
    /// work. Terminal cleanup and interruption must not prepare a runtime or
    /// realize MCP again: those effects may carry expired, Run-scoped credentials
    /// and are being stopped or released rather than used.
    pub(super) async fn ensure_session_for_frozen_control(
        &self,
        id: &str,
    ) -> Result<(), StateError> {
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
        let record = SessionRecord::new(
            persisted.agent_id().unwrap_or("assistant").to_string(),
            self.rehydrated_session_for(
                id,
                &owner_scope,
                persisted.clone(),
                RehydrationPurpose::FrozenControl,
            )?,
            persisted.revision,
            persisted.resources,
            Vec::new(),
        );
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

    /// Build the collection projection from durable Workspace truth, overlaying
    /// the richer same-process cache when present. A cold process must not
    /// return an empty Session list merely because no caller retrieved every
    /// durable id first.
    pub async fn list_sessions_scoped_durable(
        &self,
        scope: &str,
    ) -> Result<Vec<Session>, StateError> {
        let persisted = self
            .application
            .sessions_by_owner(scope)
            .await
            .map_err(StateError::from)?;
        let cached = self.sessions.lock().unwrap();
        let mut out = Vec::with_capacity(persisted.len());
        for session in persisted {
            let session_id = session.session_id.clone();
            if session.is_hidden() {
                continue;
            }
            if let Some(record) = cached.get(&session_id) {
                out.push(record.session_projection());
            } else {
                out.push(self.rehydrated_session_for(
                    &session_id,
                    scope,
                    session,
                    RehydrationPurpose::CollectionRead,
                )?);
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
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
        self.ensure_session_for_frozen_control(id).await?;
        // Do not remove the visible record until the repository has atomically
        // stored the terminal fence and outbox fact. Child cleanup targets are
        // frozen later from durable Runtime delegation authority.
        let _transition = self
            .application
            .delete_session(awaken_session_application::SessionDeleteCommand::new(id))
            .await
            .map_err(Self::map_preparation_error)?;
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
        Ok(())
    }

    /// `POST /v1/sessions/{id}/archive` — terminate the durable Session, then let
    /// the sole warm/cold projector derive the aggregate and child terminal wire
    /// events. Idempotent: a re-archive observes the same fact and emits no
    /// duplicate event.
    pub async fn archive_session(&self, id: &str) -> Result<Session, StateError> {
        self.ensure_session_for_frozen_control(id).await?;
        let owner = self.resolve_owner(id).await?;
        let terminated_fact = lifecycle_fact(
            format!("session:{id}:terminated"),
            id,
            owner.clone(),
            lifecycle_event::SESSION_TERMINATED,
        );
        self.application
            .terminate_session(id, PROCESSED_AT, terminated_fact)
            .await
            .map_err(Self::map_preparation_error)?;
        // No direct cache mutation or bespoke terminal append belongs here. The
        // canonical refresh rereads PersistedSession and projects root + every
        // derived child terminal with identical warm/restart behavior.
        self.refresh_committed_projection(id).await?;
        self.get_session(id)
    }
}

#[cfg(test)]
mod rehydration_publication_policy_tests {
    use super::*;

    #[test]
    fn missing_exact_publication_is_readable_but_not_interactively_recoverable() {
        // Cause/effect table: C1=exact publication available; C2=operation may
        // start/resume execution or projects/controls only frozen truth.
        // E1=available publication is used; E2=interactive recovery without it
        // rejects; E3=noninteractive projection rebuilds no executable state.
        // Rules P1 C1=>E1; P2 !C1+interactive=>E2;
        // P3 !C1+collection-or-frozen-control=>E3.
        assert_eq!(
            rehydration_publication_decision(RehydrationPurpose::Interactive, true, false),
            RehydrationPublicationDecision::RejectMissingExact
        );
        assert_eq!(
            rehydration_publication_decision(RehydrationPurpose::CollectionRead, true, false),
            RehydrationPublicationDecision::NonInteractiveFrozenProjection
        );
        assert_eq!(
            rehydration_publication_decision(RehydrationPurpose::FrozenControl, true, false),
            RehydrationPublicationDecision::NonInteractiveFrozenProjection
        );
        for purpose in [
            RehydrationPurpose::Interactive,
            RehydrationPurpose::CollectionRead,
            RehydrationPurpose::FrozenControl,
        ] {
            assert_eq!(
                rehydration_publication_decision(purpose, true, true),
                RehydrationPublicationDecision::Available
            );
        }
    }
}
