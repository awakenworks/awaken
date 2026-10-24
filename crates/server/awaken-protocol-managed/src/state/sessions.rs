// @formal entrypoint webhook.outbox_fact_identity_is_stable
//! Session lifecycle for [`ManagedState`]: create, rehydrate, get/list,
//! update, delete, and archive.

use super::*;
use awaken_session_application::SessionRecoveryPurpose;

use super::session_mcp_projection::typed_mcp_servers;

fn invalid_tool_configuration(message: String) -> StateError {
    StateError::Run(RunError::bad_request(message))
}
fn damaged_tool_projection(message: String) -> StateError {
    StateError::Run(RunError::internal(message))
}

fn managed_tools_override(
    tools: &[crate::tools::AgentTool],
) -> Result<awaken_session_application::SessionToolsOverride, StateError> {
    Ok(awaken_session_application::SessionToolsOverride {
        configuration: project::session_tool_configuration(tools)
            .map_err(invalid_tool_configuration)?,
        clears_all: tools.is_empty(),
    })
}

impl ManagedState {
    /// Map the one neutral public status law onto the Managed wire vocabulary.
    pub(super) fn wire_session_status(persisted: &PersistedSession) -> SessionStatus {
        match persisted.public_status() {
            awaken_session_state_contract::SessionPublicStatus::Idle => SessionStatus::Idle,
            awaken_session_state_contract::SessionPublicStatus::Running => SessionStatus::Running,
            awaken_session_state_contract::SessionPublicStatus::Rescheduling => {
                SessionStatus::Rescheduling
            }
            awaken_session_state_contract::SessionPublicStatus::Terminated => {
                SessionStatus::Terminated
            }
        }
    }

    /// Apply durable Session-root fields to a private projection candidate.
    /// Publication belongs exclusively to `publish_projection_candidate`, so
    /// callers cannot partially mutate the live cache before Thread reads pass.
    pub(super) fn apply_persisted_session(
        record: &mut SessionRecord,
        persisted: &PersistedSession,
    ) -> Result<(), StateError> {
        let tools =
            crate::project::managed_tools(&persisted.tools).map_err(damaged_tool_projection)?;
        let mcp_servers = typed_mcp_servers(persisted.configured_mcp_servers());
        record.session.status = Self::wire_session_status(persisted);
        record.session.created_at = super::constants::persisted_time(persisted.created_at_unix_ms);
        record.session.updated_at = super::constants::persisted_time(persisted.updated_at_unix_ms);
        record.root_time = RootTimeEvidence::from(persisted);
        record.session.title = persisted.title.clone();
        record.session.metadata = persisted.metadata.clone();
        record.session.deployment_id = persisted.metadata.get("awaken.deployment_id").cloned();
        record.session.archived_at = persisted.archived_at().map(str::to_owned);
        record.session.agent.tools = tools;
        record.session.agent.mcp_servers = mcp_servers;
        record.session.budget = persisted
            .budget
            .max_list_cost_minor()
            .map(crate::types::BudgetLimit::from_minor);
        record.resource_state = persisted.resources.clone();
        record.checkpoint.source.session_revision = persisted.revision;
        Ok(())
    }

    /// Publish only Session-root fields after its durable CAS. This is a
    /// candidate builder, not another writer: full Thread/lifecycle reduction
    /// and root-only visibility both converge on the sole CAS publisher. The
    /// Coordinator's Session adoption of credential rollouts publishes the
    /// roots it committed through this.
    pub fn publish_persisted_session(
        &self,
        persisted: &PersistedSession,
    ) -> Result<(), StateError> {
        let result = self.publish_projection_update(persisted.session_id.as_str(), |candidate| {
            if persisted.revision >= candidate.checkpoint.source.session_revision {
                Self::apply_persisted_session(candidate, persisted)?;
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
            .mutations()
            .commit_session_snapshot_with_outcome(
                owner_scope,
                awaken_session_state_contract::SessionAggregate::try_from(session)
                    .expect("valid test snapshot"),
                operation,
                lifecycle_facts,
            )
            .await
            .map_err(Self::map_application_mutation_error)?
            .into_session();
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
            .roles
            .credentials
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
        // It is required — an unstamped request is refused at the edge extractor,
        // so this owner is never invented here.
        owner_scope: &str,
    ) -> Result<Session, StateError> {
        self.create_session_with_identity(req, owner_scope, None, None)
            .await
    }

    pub(super) async fn create_session_with_identity(
        &self,
        req: SessionCreateParams,
        owner_scope: &str,
        explicit_id: Option<String>,
        idempotency: Option<awaken_session_state_contract::IdempotencyRecord>,
    ) -> Result<Session, StateError> {
        self.create_session_with_identity_from(
            req,
            owner_scope,
            explicit_id,
            idempotency,
            None,
            None,
        )
        .await
    }

    /// One create owner for Session and Deployment wire unions. `Some` carries
    /// the already-lowered Deployment union; ordinary Session create uses the
    /// request's narrower `initial_events` field. The adapter validates and
    /// lowers the wire request; the Session application orders every check,
    /// effect and the durable insert.
    pub(super) async fn create_session_with_identity_from(
        &self,
        mut req: SessionCreateParams,
        owner_scope: &str,
        explicit_id: Option<String>,
        idempotency: Option<awaken_session_state_contract::IdempotencyRecord>,
        deployment_initial_events: Option<Vec<InboundEvent>>,
        context_seed: Option<awaken_session_configuration_contract::SessionContextSeed>,
    ) -> Result<Session, StateError> {
        req.validate_common().map_err(invalid_tool_configuration)?;
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
                .map_err(invalid_tool_configuration)?;
            events
        } else {
            req.validate_initial_events()
                .map_err(invalid_tool_configuration)?;
            req.initial_events.clone()
        };
        let model_override = match req.agent.model_override() {
            ModelOverride::Set(cfg) => Some(cfg),
            ModelOverride::Absent => None,
        };
        let request = awaken_session_application::SessionCreateRequest {
            owner_scope: owner_scope.to_owned(),
            explicit_id,
            idempotency,
            agent_id: req.agent.id().to_string(),
            agent_version: req.agent.version(),
            model_override: model_override.as_ref().map(|cfg| {
                awaken_session_application::SessionModelRequest {
                    model_id: cfg.id.clone(),
                    inference: cfg.inference_options(),
                }
            }),
            system_override: req
                .agent
                .system_override()
                .map(|system| system.map(str::to_owned)),
            tools_override: req
                .agent
                .tools_override()
                .map(managed_tools_override)
                .transpose()?,
            skills_override: req.agent.skills_override().map(|skills| {
                skills
                    .iter()
                    .cloned()
                    .map(crate::types::agent::AgentSkill::into_binding)
                    .collect()
            }),
            mcp_override: req.agent.mcp_servers_override().map(|servers| {
                servers
                    .iter()
                    .cloned()
                    .map(|server| {
                        super::application::agent_mcp_candidate(
                            server,
                            crate::session_mcp::McpAttachmentOrigin::Agent,
                        )
                    })
                    .collect()
            }),
            environment_id: req.environment_id.clone(),
            inputs: std::mem::take(&mut req.resources)
                .iter()
                .map(crate::types::resource::ResourceInput::to_input_request)
                .collect(),
            vault_ids: req.vault_ids.clone(),
            title: req.title.clone(),
            metadata: req.metadata.clone(),
            context_seed,
            budget: req
                .budget
                .as_ref()
                .map(|budget| match budget.max_list_cost_minor() {
                    Ok(minor) => {
                        awaken_session_application::SessionBudgetRequest::MaxListCostMinor(minor)
                    }
                    Err(message) => {
                        awaken_session_application::SessionBudgetRequest::Invalid(message)
                    }
                }),
        };
        let protocol = ManagedCreateProtocol {
            state: self,
            model_override: model_override.as_ref(),
            initial_events: &initial_events,
        };
        let created = Box::pin(
            self.roles
                .creation
                .create_requested_session(request, &protocol),
        )
        .await
        .map_err(Self::map_create_request_error)?;
        let persisted = created;
        let id = persisted.session_id.to_string();
        let baseline = persisted.frozen_baseline().ok_or_else(|| {
            StateError::Run(RunError::internal(
                "created Session root has no frozen baseline",
            ))
        })?;
        let agent_id = baseline.agent_id.clone();
        let configuration =
            awaken_session_state_contract::SessionUpdateHistory::agent_configuration(&persisted)
                .ok_or_else(|| {
                    StateError::Run(RunError::internal(
                        "fresh Session has no accepted Agent configuration",
                    ))
                })?;
        let session = session_from_persisted_root(&persisted, baseline, &configuration)?;
        self.owners
            .lock()
            .unwrap()
            .insert(id.clone(), owner_scope.to_owned());
        let root_time = RootTimeEvidence::from(&persisted);
        let record = SessionRecord::new(
            agent_id,
            session,
            persisted.revision,
            persisted.resources,
            root_time,
            Vec::new(),
        );
        let session = record.session_projection();
        self.sessions.lock().unwrap().insert(id.clone(), record);
        Ok(session)
    }

    fn map_create_request_error(
        error: awaken_session_application::SessionCreateRequestError,
    ) -> StateError {
        match error {
            awaken_session_application::SessionCreateRequestError::Rejected(error) => {
                StateError::Run(error)
            }
            awaken_session_application::SessionCreateRequestError::VaultNotFound(vault_id) => {
                StateError::VaultNotFound(vault_id)
            }
            awaken_session_application::SessionCreateRequestError::Repository(error) => {
                StateError::from(error)
            }
            awaken_session_application::SessionCreateRequestError::Preparation(error) => {
                Self::map_preparation_error(error)
            }
            awaken_session_application::SessionCreateRequestError::Creation(error) => {
                Self::map_creation_error(error)
            }
        }
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
        match self
            .roles
            .roots
            .owner(&awaken_session_types::SessionId::from(session_id))
            .await
        {
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
        self.rehydrated_session_for(
            id,
            owner_scope,
            persisted,
            SessionRecoveryPurpose::Interactive,
        )
    }

    pub(super) fn rehydrated_session_for(
        &self,
        id: &str,
        owner_scope: &str,
        persisted: PersistedSession,
        purpose: SessionRecoveryPurpose,
    ) -> Result<Session, StateError> {
        let (baseline, configuration) = self
            .roles
            .projection
            .recover_agent_configuration(id, owner_scope, &persisted, purpose)
            .map_err(StateError::Run)?;
        session_from_persisted_root(&persisted, baseline, &configuration)
    }

    /// Rebuild only the disposable projection needed to control already-frozen
    /// work. Terminal cleanup and interruption must not prepare a runtime or
    /// realize MCP again: those effects may carry expired, Run-scoped credentials
    /// and are being stopped or released rather than used.
    pub(super) async fn ensure_session_for_frozen_control(
        &self,
        id: &str,
    ) -> Result<(), StateError> {
        self.hydrate_session_record(id, SessionRecoveryPurpose::FrozenControl)
            .await
    }

    /// `GET /v1/sessions/{id}`.
    pub fn get_session(&self, id: &str) -> Result<Session, StateError> {
        self.get_session_with_revision(id)
            .map(|(session, _)| session)
    }

    /// Read the wire projection and its durable Session revision from the same
    /// cache snapshot. The checkpoint is already the root revision used to
    /// render this DTO; a second repository read could pair a newer ETag with
    /// older content or turn a committed create into an unavailable response.
    pub fn get_session_with_revision(
        &self,
        id: &str,
    ) -> Result<(Session, awaken_session_types::SessionRevision), StateError> {
        self.get_session_with_revision_and_time(id)
            .map(|(session, revision, _)| (session, revision))
    }

    pub(crate) fn get_session_with_revision_and_time(
        &self,
        id: &str,
    ) -> Result<
        (
            Session,
            awaken_session_types::SessionRevision,
            RootTimeEvidence,
        ),
        StateError,
    > {
        let sessions = self.sessions.lock().unwrap();
        sessions
            .get(id)
            .map(|record| {
                (
                    record.session_projection(),
                    record.checkpoint.source.session_revision,
                    record.root_time,
                )
            })
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
    /// workspace's `GET /v1/sessions` never sees another's. Creation records an
    /// owner for every session, so a cached session with no owner index entry is
    /// an internal inconsistency and belongs to no scope's list rather than to
    /// the seeded one. Mirrors the per-id ownership guard, which the collection
    /// route does not pass through.
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
                    .is_some_and(|owner| owner == scope)
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
        Ok(self
            .list_sessions_scoped_durable_with_time(scope)
            .await?
            .into_iter()
            .map(|(session, _)| session)
            .collect())
    }

    pub(crate) async fn list_sessions_scoped_durable_with_time(
        &self,
        scope: &str,
    ) -> Result<Vec<(Session, RootTimeEvidence)>, StateError> {
        let persisted = self
            .roles
            .roots
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
            if let Some(record) = cached.get(session_id.as_str()) {
                out.push((record.session_projection(), record.root_time));
            } else {
                let evidence = RootTimeEvidence::from(&session);
                out.push((
                    self.rehydrated_session_for(
                        session_id.as_str(),
                        scope,
                        session,
                        SessionRecoveryPurpose::CollectionRead,
                    )?,
                    evidence,
                ));
            }
        }
        out.sort_by(|a, b| a.0.id.cmp(&b.0.id));
        Ok(out)
    }

    /// `DELETE /v1/sessions/{id}` — commit a terminal `session.deleted` event,
    /// push it to any open SSE stream, then drop the in-memory record. The
    /// broadcast happens before cache removal. An already-open stream can also
    /// observe the same logical deletion from its exact hidden durable root or
    /// owner-scoped tombstone; a 404 alone is never a deletion receipt. Public
    /// `events.list`/`retrieve` still return 404 for this hidden Session.
    pub async fn delete_session(&self, id: &str) -> Result<(), StateError> {
        // A terminal command must work after a process restart without realizing
        // the soon-to-be-released Environment or MCP attachments.
        self.ensure_session_for_frozen_control(id).await?;
        // Do not remove the visible record until the repository has atomically
        // stored the terminal fence and outbox fact. Child cleanup targets are
        // frozen later from durable Runtime delegation authority.
        let transition = self
            .roles
            .termination
            .clone()
            .delete_session(awaken_session_application::SessionDeleteCommand::new(id))
            .await
            .map_err(Self::map_preparation_error)?;
        {
            let mut sessions = self.sessions.lock().unwrap();
            let record = sessions.get_mut(id).ok_or(StateError::NotFound)?;
            let mut event = super::events::session_deletion_event(&transition.owner_scope, id);
            let delta = super::events::committed_delivery_delta(
                &record.events,
                std::slice::from_mut(&mut event),
            )?;
            self.broadcast_committed_delta(id, &delta);
            sessions.remove(id);
        }
        Ok(())
    }

    /// `POST /v1/sessions/{id}/archive` — terminate the durable Session, then let
    /// the sole warm/cold projector derive the aggregate and child terminal wire
    /// events. Idempotent: a re-archive observes the same fact and emits no
    /// duplicate event.
    pub async fn archive_session(&self, id: &str) -> Result<Session, StateError> {
        self.archive_session_with_cleanup(id, false).await
    }

    /// Profiled release joins verified cleanup through the existing Session
    /// archive owner, including exact failed Frozen WorkUnit admission.
    /// Public archive keeps its strict idle-only admission.
    pub async fn release_profiled_session_without_publication(
        &self,
        id: &str,
    ) -> Result<Session, StateError> {
        self.archive_session_with_cleanup(id, true).await
    }

    async fn archive_session_with_cleanup(
        &self,
        id: &str,
        verified_cleanup: bool,
    ) -> Result<Session, StateError> {
        self.ensure_session_for_frozen_control(id).await?;
        let termination = &self.roles.termination;
        let release = if verified_cleanup {
            termination.release_profiled_session(id).await
        } else {
            termination.archive_session(id).await
        };
        release.map_err(Self::map_preparation_error)?;
        // No direct cache mutation or bespoke terminal append belongs here. The
        // canonical refresh rereads PersistedSession and projects root + every
        // derived child terminal with identical warm/restart behavior.
        self.refresh_committed_projection(id).await?;
        self.get_session(id)
    }
}

/// The Managed rules of Session creation: the `sesn_` id spelling, initial
/// Event lowering, the inference-geography policy, custom tool rules and the
/// SDK projection finished before the durable insert.
struct ManagedCreateProtocol<'a> {
    state: &'a ManagedState,
    model_override: Option<&'a ModelConfig>,
    initial_events: &'a [InboundEvent],
}

fn run_error(error: StateError) -> RunError {
    match error {
        StateError::Run(error) => error,
        other => RunError::internal(other.to_string()),
    }
}

#[async_trait::async_trait]
impl awaken_session_application::SessionCreateProtocol for ManagedCreateProtocol<'_> {
    // Mint from the process-incarnation namespace so active-active peers and
    // restarted processes cannot choose the same Session id.
    fn session_id_candidate(&self) -> String {
        let sequence = self.state.session_seq.fetch_add(1, Ordering::SeqCst);
        format!(
            "sesn_{}",
            awaken_agent_contract::stable_fingerprint(&(
                "managed-session",
                self.state.roles.creation.runtime_incarnation(),
                sequence,
            ))
        )
    }

    fn initial_event_plan(
        &self,
        session_id: &str,
    ) -> Result<Option<awaken_session_event_contract::SessionInitialEventPlan>, RunError> {
        crate::types::initial_event::compile_session_initial_event_plan(
            session_id,
            self.initial_events,
        )
        .map_err(RunError::bad_request)
    }

    async fn admit_model(
        &self,
        owner_scope: &str,
        model: Option<&awaken_session_configuration_contract::SessionModelChoice>,
    ) -> Result<(), RunError> {
        let inference_geo = match model {
            Some(choice)
                if choice.source
                    == awaken_session_configuration_contract::SessionModelSource::Override =>
            {
                self.model_override.and_then(|model| model.inference_geo)
            }
            Some(choice) => {
                ModelConfig::from_inference(choice.model_id.clone(), choice.inference.clone())
                    .inference_geo
            }
            None => None,
        };
        self.state
            .authorize_inference_geo(
                owner_scope,
                inference_geo,
                crate::InferenceGeoCheckpoint::SessionCreate,
            )
            .await
            .map_err(run_error)
    }

    fn capability_tools(
        &self,
        capabilities: &AgentCapabilities,
    ) -> Result<awaken_session_configuration_contract::SessionToolConfiguration, RunError> {
        for tool in &capabilities.custom_tools {
            project::validate_custom_tool(tool).map_err(RunError::bad_request)?;
        }
        let tools = project::agent_tools(capabilities).map_err(RunError::bad_request)?;
        project::session_tool_configuration(&tools).map_err(RunError::bad_request)
    }

    fn validate_before_insert(
        &self,
        configuration: &awaken_session_configuration_contract::SessionAgentConfiguration,
    ) -> Result<(), RunError> {
        session_agent_from_configuration(configuration).map_err(run_error)?;
        Ok(())
    }
}

/// The sole Session DTO constructor borrows its original authoritative Root.
/// Creation and recovery choose configuration at their original boundaries;
/// this pure projector never reselects catalog data or publishes a cache.
fn session_from_persisted_root(
    persisted: &PersistedSession,
    baseline: &awaken_session_configuration_contract::SessionBaseline,
    configuration: &awaken_session_configuration_contract::SessionAgentConfiguration,
) -> Result<Session, StateError> {
    Ok(Session {
        id: persisted.session_id.to_string(),
        kind: "session",
        agent: session_agent_from_configuration(configuration)?,
        budget: persisted
            .budget
            .max_list_cost_minor()
            .map(crate::types::BudgetLimit::from_minor),
        environment_id: baseline.environment.environment_id.clone(),
        created_at: super::constants::persisted_time(persisted.created_at_unix_ms),
        updated_at: super::constants::persisted_time(persisted.updated_at_unix_ms),
        archived_at: persisted.archived_at().map(str::to_owned),
        title: persisted.title.clone(),
        metadata: persisted.metadata.clone(),
        resources: Vec::new(),
        outcome_evaluations: Vec::new(),
        status: ManagedState::wire_session_status(persisted),
        stats: SessionStats::default(),
        usage: Usage::default(),
        vault_ids: baseline.mcp_authoring.ordered_vault_ids.clone(),
        deployment_id: persisted.metadata.get("awaken.deployment_id").cloned(),
    })
}

pub(crate) fn session_agent_from_configuration(
    configuration: &awaken_session_configuration_contract::SessionAgentConfiguration,
) -> Result<SessionAgent, StateError> {
    let multiagent = configuration
        .multiagent
        .as_ref()
        .map(|roster| -> Result<_, StateError> {
            let agents = roster
                .members
                .iter()
                .map(|member| match member {
                    awaken_session_configuration_contract::SessionAgentRosterMember::Agent(
                        value,
                    ) => Ok(crate::types::SessionMultiagentRosterEntry::Agent(
                        crate::types::SessionThreadAgent::from(&session_agent_from_configuration(
                            value,
                        )?),
                    )),
                    awaken_session_configuration_contract::SessionAgentRosterMember::Advisor(
                        model,
                    ) => Ok(crate::types::SessionMultiagentRosterEntry::Advisor(
                        crate::types::agent::AdvisorRosterEntry {
                            model: model.clone(),
                            kind: crate::types::agent::AdvisorRosterEntryKind::Advisor,
                        },
                    )),
                })
                .collect::<Result<_, StateError>>()?;
            Ok(crate::types::SessionMultiagentCoordinator {
                kind: "coordinator",
                agents,
            })
        })
        .transpose()?;
    Ok(SessionAgent {
        id: configuration.agent_id.clone(),
        kind: "agent",
        version: configuration.version,
        model: ModelConfig::from_inference(
            configuration.model.model_id.clone(),
            configuration.model.inference.clone(),
        ),
        name: configuration.name.clone(),
        description: configuration.description.clone(),
        system: configuration.system.clone(),
        tools: project::managed_tools(&configuration.tools).map_err(damaged_tool_projection)?,
        mcp_servers: typed_mcp_servers(configuration.mcp_servers.clone()),
        skills: configuration
            .skills
            .iter()
            .cloned()
            .map(crate::types::agent::AgentSkill::from_binding)
            .collect(),
        multiagent,
    })
}

/// The one ownership verdict an Event command carries, with its Workspace:
/// the owner index names the Workspace and the durable root decides.
pub(crate) async fn owned_session_access(
    state: &ManagedState,
    session_id: &str,
) -> Result<(awaken_session_application::SessionAccess, String), StateError> {
    let owner = state.owner_scope(session_id).ok_or(StateError::NotFound)?;
    let access = state
        .roles
        .roots
        .authorize(&owner, &awaken_session_types::SessionId::from(session_id))
        .await
        .map_err(|error| StateError::Run(error.into()))?;
    Ok((access, owner))
}
