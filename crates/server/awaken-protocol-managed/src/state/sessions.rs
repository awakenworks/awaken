// @formal entrypoint session.missing_publication_is_available_only_to_noninteractive_frozen_projection
// @formal entrypoint webhook.outbox_fact_identity_is_stable
//! Session lifecycle for [`ManagedState`]: create, rehydrate, get/list,
//! update, delete, and archive.

use super::*;
use awaken_time::WallClockExt as _;

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

fn managed_archive_command(
    id: &str,
    owner: Option<String>,
    observed_at: u64,
) -> (String, ManagedLifecycleFact) {
    (
        awaken_session_application_contract::epoch_millis_to_rfc3339(observed_at),
        lifecycle_fact(
            awaken_session_event_contract::session_terminated_fact_id(id),
            id,
            owner,
            awaken_session_event_contract::SESSION_TERMINATED,
            observed_at,
        ),
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RehydrationPurpose {
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
    match (profile_available, has_frozen_revision, purpose) {
        (true, _, _) => RehydrationPublicationDecision::Available,
        (false, false, _) => RehydrationPublicationDecision::Unpinned,
        (false, true, RehydrationPurpose::Interactive) => {
            RehydrationPublicationDecision::RejectMissingExact
        }
        (false, true, _) => RehydrationPublicationDecision::NonInteractiveFrozenProjection,
    }
}

#[cfg(test)]
#[test]
fn rehydration_publication_decision_table_is_exact() {
    // The three causes form all twelve rows: profile availability dominates;
    // otherwise only a pinned interactive recovery rejects. This executes the
    // existing Kani decision surface on native platforms as well.
    for purpose in [
        RehydrationPurpose::Interactive,
        RehydrationPurpose::CollectionRead,
        RehydrationPurpose::FrozenControl,
    ] {
        for pinned in [false, true] {
            for available in [false, true] {
                let expected = if available {
                    RehydrationPublicationDecision::Available
                } else if !pinned {
                    RehydrationPublicationDecision::Unpinned
                } else if purpose == RehydrationPurpose::Interactive {
                    RehydrationPublicationDecision::RejectMissingExact
                } else {
                    RehydrationPublicationDecision::NonInteractiveFrozenProjection
                };
                assert_eq!(
                    rehydration_publication_decision(purpose, pinned, available),
                    expected
                );
            }
        }
    }
}

#[cfg(kani)]
/// @obligation session.missing_publication_is_available_only_to_noninteractive_frozen_projection
/// @kani profile=solver-kissat-v1
/// @proof-input-debt free_symbol rehydration_publication_decision has_frozen_revision
/// @proof-input-debt free_symbol rehydration_publication_decision profile_available
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
        captured: Option<
            &[(
                String,
                awaken_executable_agent_contract::ExecutableAgentSessionProfile,
            )],
        >,
    ) -> Result<Option<awaken_session_configuration_contract::SessionAgentRoster>, StateError> {
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
            let resolved = if let Some(captured) = captured {
                captured
                    .iter()
                    .find(|(id, value)| {
                        id == &delegate.agent_id
                            && delegate
                                .source_revision
                                .is_none_or(|revision| value.source_revision == revision)
                    })
                    .map(|(_, value)| value.clone())
            } else {
                match (profile.is_some(), delegate.source_revision) {
                    (true, Some(revision)) => self.roles.projection.session_profile_at_revision(
                        workspace_id,
                        &delegate.agent_id,
                        revision,
                    ),
                    _ => self
                        .roles
                        .projection
                        .session_profile(workspace_id, &delegate.agent_id),
                }
            };
            let child = match resolved {
                Some(profile) => agent_configuration_from_profile(&delegate.agent_id, profile),
                None if profile.is_none() => {
                    agent_configuration_from_profile(&delegate.agent_id, Default::default())
                }
                None => {
                    return Err(StateError::Run(RunError::bad_request(format!(
                        "multiagent_unavailable: Agent `{}` publication revision {:?} is unavailable",
                        delegate.agent_id, delegate.source_revision
                    ))));
                }
            };
            agents.push(
                awaken_session_configuration_contract::SessionAgentRosterMember::Agent(Box::new(
                    child,
                )),
            );
        }
        if let Some(model) = advisor_model {
            agents.push(
                awaken_session_configuration_contract::SessionAgentRosterMember::Advisor(model),
            );
        }
        Ok(Some(
            awaken_session_configuration_contract::SessionAgentRoster { members: agents },
        ))
    }

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
            .creation
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
        self.rehydrated_session_for(id, owner_scope, persisted, RehydrationPurpose::Interactive)
    }

    pub(super) fn rehydrated_session_for(
        &self,
        id: &str,
        owner_scope: &str,
        persisted: PersistedSession,
        purpose: RehydrationPurpose,
    ) -> Result<Session, StateError> {
        let baseline = persisted.frozen_baseline().ok_or_else(|| {
            StateError::Run(RunError::unavailable(
                "Session root is not finalized and cannot be rehydrated",
            ))
        })?;
        let configuration =
            rehydrated_agent_configuration(self, id, owner_scope, &persisted, baseline, purpose)?;
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
        self.hydrate_session_record(id, RehydrationPurpose::FrozenControl)
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
                        RehydrationPurpose::CollectionRead,
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
        let owner = self.resolve_owner(id).await?;
        let (archived_at, terminated_fact) =
            managed_archive_command(id, owner, self.roles.clock.now_ms());
        let termination = &self.roles.termination;
        let release = if verified_cleanup {
            // This read selects a safe command, not a second status authority.
            // Application commands reload and recheck the durable root.
            let execution = self
                .roles
                .roots
                .session(&awaken_session_types::SessionId::from(id))
                .await?
                .execution;
            if !execution.is_terminal() {
                let initial_fact = terminated_fact.clone();
                let initial_release = match execution {
                    SessionExecutionState::Preparing | SessionExecutionState::Running => {
                        termination.release_profiled_session(id, &archived_at, initial_fact)
                    }
                    _ => termination.terminate_session(id, &archived_at, initial_fact),
                };
                initial_release.await.map_err(Self::map_preparation_error)?;
            }
            termination
                .release_terminal_session(id, &archived_at, terminated_fact)
                .await
        } else {
            termination
                .terminate_session(id, &archived_at, terminated_fact)
                .await
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

    fn project_before_insert(
        &self,
        owner_scope: &str,
        profile: Option<&awaken_executable_agent_contract::ExecutableAgentSessionProfile>,
        capabilities: &AgentCapabilities,
        mut configuration: awaken_session_configuration_contract::SessionAgentConfiguration,
        delegates: &[(
            String,
            awaken_executable_agent_contract::ExecutableAgentSessionProfile,
        )],
    ) -> Result<awaken_session_configuration_contract::SessionAgentConfiguration, RunError> {
        configuration.multiagent = self
            .state
            .resolved_session_multiagent(owner_scope, profile, capabilities, Some(delegates))
            .map_err(run_error)?;
        session_agent_from_configuration(&configuration).map_err(run_error)?;
        Ok(configuration)
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

/// The complete Agent input phase of cold projection. The outer Session
/// constructor is shared; only legacy input recovery consults the old catalog.
fn rehydrated_agent_configuration(
    state: &ManagedState,
    id: &str,
    owner_scope: &str,
    persisted: &PersistedSession,
    baseline: &awaken_session_configuration_contract::SessionBaseline,
    purpose: RehydrationPurpose,
) -> Result<awaken_session_configuration_contract::SessionAgentConfiguration, StateError> {
    if let Some(configuration) =
        awaken_session_state_contract::SessionUpdateHistory::agent_configuration(persisted)
    {
        return Ok(configuration);
    }
    let caps = state.roles.projection.capabilities_for(id);
    let profile = match baseline.agent_revision {
        Some(revision) => state.roles.projection.session_profile_at_revision(
            owner_scope,
            &baseline.agent_id,
            revision,
        ),
        None => state
            .roles
            .projection
            .session_profile(owner_scope, &baseline.agent_id),
    };
    let decision = rehydration_publication_decision(
        purpose,
        baseline.agent_revision.is_some(),
        profile.is_some(),
    );
    if decision == RehydrationPublicationDecision::RejectMissingExact {
        let revision = baseline
            .agent_revision
            .expect("missing exact decision requires a revision");
        return Err(StateError::Run(RunError::unavailable(format!(
            "Agent `{}` publication revision {revision} is unavailable during Session recovery",
            baseline.agent_id
        ))));
    }
    let multiagent = match decision {
        RehydrationPublicationDecision::NonInteractiveFrozenProjection => None,
        RehydrationPublicationDecision::Available | RehydrationPublicationDecision::Unpinned => {
            state.resolved_session_multiagent(owner_scope, profile.as_ref(), &caps, None)?
        }
        RehydrationPublicationDecision::RejectMissingExact => {
            unreachable!("missing exact publication was rejected before projection")
        }
    };
    let mut configuration =
        agent_configuration_from_profile(&baseline.agent_id, profile.clone().unwrap_or_default());
    configuration.version = profile
        .as_ref()
        .map(|value| value.source_revision)
        .or(baseline.agent_revision)
        .unwrap_or(1)
        .max(1);
    configuration.model = awaken_session_configuration_contract::SessionModelChoice {
        model_id: baseline.model.clone(),
        inference: baseline
            .model_override
            .as_ref()
            .map(|value| value.inference.clone())
            .or_else(|| profile.as_ref().map(|value| value.inference.clone()))
            .unwrap_or_default(),
        source: awaken_session_configuration_contract::SessionModelSource::Publication,
    };
    configuration.system = baseline
        .system_prompt
        .resolve(profile.as_ref().and_then(|value| value.system.clone()));
    configuration.tools = persisted.tools.clone();
    configuration.mcp_servers = persisted.configured_mcp_servers();
    configuration.skills = persisted
        .resources
        .desired()
        .skills()
        .iter()
        .map(|value| awaken_agent_contract::AgentSkillBinding {
            kind: value.kind,
            skill_id: value.skill_id.clone(),
            version: value.version.to_string(),
        })
        .collect();
    configuration.multiagent = multiagent;
    Ok(configuration)
}

pub(crate) fn agent_configuration_from_profile(
    agent_id: &str,
    profile: awaken_executable_agent_contract::ExecutableAgentSessionProfile,
) -> awaken_session_configuration_contract::SessionAgentConfiguration {
    awaken_session_configuration_contract::SessionAgentConfiguration {
        agent_id: agent_id.to_owned(),
        version: profile.source_revision.max(1),
        model: awaken_session_configuration_contract::SessionModelChoice {
            model_id: profile
                .model
                .or(profile.execution_model_ref)
                .unwrap_or_default(),
            inference: profile.inference,
            source: awaken_session_configuration_contract::SessionModelSource::Publication,
        },
        name: profile.name.unwrap_or_else(|| agent_id.to_owned()),
        description: profile.description,
        system: profile.system,
        tools: awaken_session_configuration_contract::SessionToolConfiguration {
            toolsets: profile.toolsets,
            client_tools: profile.client_tools,
        },
        mcp_servers: profile
            .mcp_servers
            .into_iter()
            .map(
                |server| awaken_session_configuration_contract::VisibleMcpServer {
                    name: server.name,
                    target: server.target,
                    prompts_as_skills: server.prompts_as_skills,
                },
            )
            .collect(),
        skills: profile.skills,
        multiagent: None,
    }
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

#[cfg(test)]
mod rehydration_publication_policy_tests {
    use super::*;

    #[test]
    fn independent_managed_archives_share_intent_across_route_clock_samples() {
        // Causes: M1 two independent Managed archive requests sample 1s/2s
        // for the same owned Session. Effects: M1 display and fact timestamp
        // differ, while Session/Workspace/fact id/type and interval semantics
        // remain identical. The Application lost-CAS table tests one receipt.
        let (first_time, first) =
            managed_archive_command("session", Some("workspace".into()), 1_000);
        let (second_time, second) =
            managed_archive_command("session", Some("workspace".into()), 2_000);
        assert_ne!(first_time, second_time, "M1");
        assert_ne!(first.timestamp, second.timestamp, "M1");
        assert_eq!(first.id, second.id, "M1");
        assert_eq!(first.object_id, second.object_id, "M1");
        assert_eq!(first.workspace_id, second.workspace_id, "M1");
        assert_eq!(first.event_type, second.event_type, "M1");
        assert_eq!(first.runtime_interval, second.runtime_interval, "M1");
    }

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
