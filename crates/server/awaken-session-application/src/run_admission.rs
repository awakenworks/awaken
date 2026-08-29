//! Session-owned admission in front of the neutral Run application port.
//!
//! Runtime Host executes an already-admitted Run. Session creation/recovery is
//! application policy, so it is configured here once for every public protocol.

use std::collections::BTreeMap;
use std::sync::Arc;

use awaken_agent_contract::agent::message::Message;
use awaken_session_contract::{
    AdmitSessionRun, AdmittedSessionRun, ControlSessionCreationInputs, McpAttachmentOrigin,
    Pending, RunApplication, RunApplicationError, RunError, RunResume, SessionBaselineState,
    SessionCreationIntent, SessionMcpAuthoringContext, SessionNetworkPolicy,
    SessionRepositoryError, SessionRunDelivery, SessionRunReservation, SessionToolConfiguration,
    StepOutcome, session_run_activity_operation_id,
};

use crate::{
    ConfiguredSessionRepository, CreateSessionCommand, McpAttachmentCandidate,
    McpAttachmentCandidateTarget, SessionApplication, SessionCreationError, SessionMutationError,
    SessionPreparationError, SessionRealizationError, SessionRepositoryOwner,
    SessionRepositoryResourceInput,
};

/// Protocol-independent request to create a Session from one published Agent
/// profile. The Session application resolves Environment, MCP, Resources,
/// credentials, tools, and immutable execution identity exactly once.
pub struct CreateProfiledSessionCommand {
    pub owner_scope: String,
    pub session_id: String,
    pub mutation_policy: awaken_session_contract::SessionMutationPolicy,
    pub agent_id: String,
    /// Exact executable publication source revision. `None` selects the current
    /// publication for ordinary interactive authoring.
    pub source_revision: Option<u64>,
    /// Exact Environment selected by the product at Session creation. The
    /// Session application resolves and freezes it without mutating the
    /// published Agent's authored defaults.
    pub environment_id: Option<String>,
    pub model: Option<String>,
    pub mounts: Vec<awaken_provisioning_contract::MountRequirement>,
    pub env: Vec<awaken_provisioning_contract::EnvVar>,
    pub prompts: Vec<String>,
    /// Session-local typed attachments supplied by the product. They join the
    /// published Agent defaults inside this sole composer and therefore freeze
    /// in the original root rather than through a post-create Manifest write.
    pub resource_inputs: Vec<awaken_session_contract::SessionInputAttachment>,
    /// Explicit Session candidates supplied by the product adapter. Published
    /// Agent candidates are joined and normalized inside the sole composer.
    pub mcp_candidates: Vec<McpAttachmentCandidate>,
    pub repositories: Vec<ProfiledSessionRepositoryInput>,
    pub network_restriction: Option<SessionNetworkPolicy>,
    pub title: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub tools: Option<SessionToolConfiguration>,
    pub idempotency: Option<awaken_session_contract::IdempotencyRecord>,
}

/// One product-authored Repository binding joined into the original profiled
/// Session root. `binding_id` is the caller's stable correlation identity;
/// `repository.id` remains Open's Session-owned Registry identity.
pub struct ProfiledSessionRepositoryInput {
    pub binding_id: awaken_resource_contract::BindingId,
    pub repository: SessionRepositoryResourceInput,
}

pub struct RecoveredSessionProjection {
    pub owner_scope: String,
    pub session: awaken_session_contract::PersistedSession,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionProjectionRecoveryError {
    #[error("Session was not found")]
    NotFound,
    #[error(transparent)]
    Rejected(#[from] RunError),
    #[error("Session recovery is unavailable: {0}")]
    Unavailable(String),
}

fn repository_error(error: SessionRepositoryError) -> RunError {
    match error {
        SessionRepositoryError::NotFound => RunError::bad_request("Session was not found"),
        SessionRepositoryError::Unavailable(message) => RunError::unavailable(message),
        error => RunError::internal(error.to_string()),
    }
}

fn mutation_error(error: SessionMutationError) -> RunError {
    match error {
        SessionMutationError::NotFound => RunError::bad_request("Session was not found"),
        SessionMutationError::Conflict | SessionMutationError::IdempotencyMismatch => {
            RunError::unavailable("Session changed while it was being admitted")
        }
        SessionMutationError::Unavailable(message) => RunError::unavailable(message),
    }
}

fn preparation_error(error: SessionPreparationError) -> RunError {
    match error {
        SessionPreparationError::Rejected(error) => error,
        SessionPreparationError::Unavailable(message) => RunError::unavailable(message),
        SessionPreparationError::NotFound => RunError::bad_request("Session was not found"),
        SessionPreparationError::Conflict => {
            RunError::unavailable("Session changed while it was being admitted")
        }
    }
}

fn realization_error(error: SessionRealizationError) -> RunError {
    match error {
        SessionRealizationError::Effect(error) => error,
        SessionRealizationError::Control(error) => RunError::unavailable(error.to_string()),
        SessionRealizationError::DidNotConverge => {
            RunError::unavailable("Session realization did not converge")
        }
    }
}

fn creation_error(error: SessionCreationError) -> RunError {
    match error {
        SessionCreationError::Rejected(error) => error,
        SessionCreationError::Conflict => RunError::unavailable("Session creation conflicted"),
        SessionCreationError::Tombstoned => {
            RunError::bad_request("Session identity is terminally occupied")
        }
        SessionCreationError::IdempotencyMismatch => {
            RunError::bad_request("Session creation idempotency mismatch")
        }
        SessionCreationError::Unavailable(message) => RunError::unavailable(message),
        SessionCreationError::Internal(message) => RunError::internal(message),
    }
}

fn profiled_repository_attachment(
    repository_id: awaken_resource_contract::RepositoryId,
    mount_path: String,
    binding_id: awaken_resource_contract::BindingId,
) -> awaken_session_contract::SessionInputAttachment {
    awaken_session_contract::SessionInputAttachment {
        binding: awaken_resource_contract::InputBinding {
            binding_id,
            target: awaken_resource_contract::InputResourceId::Repository(repository_id),
            mount_path,
            access: awaken_resource_contract::ResourceAccess::ReadWrite,
            instructions: None,
        },
        replaces: None,
    }
}

impl SessionApplication {
    /// Reserve one stable Run, then commit or recover the exact Session activity
    /// receipt before activation. This is the sole durable Session Run admission
    /// boundary used by both public protocols and Event reconciliation.
    pub async fn admit_session_run(
        &self,
        command: AdmitSessionRun,
    ) -> Result<AdmittedSessionRun, RunError> {
        self.admit_session_run_with_owner(command, None).await
    }

    /// Admit a protocol request under its already-authenticated owner scope.
    /// This is the same durable admission as [`Self::admit_session_run`]; the
    /// extra coordinate is used only to create or authorize the Session before
    /// the shared reservation/activity/activation protocol starts.
    pub async fn admit_session_run_for_owner(
        &self,
        owner_scope: &str,
        command: AdmitSessionRun,
    ) -> Result<AdmittedSessionRun, RunError> {
        self.admit_session_run_with_owner(command, Some(owner_scope))
            .await
    }

    fn admit_session_run_with_owner<'a>(
        &'a self,
        command: AdmitSessionRun,
        expected_owner: Option<&'a str>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<AdmittedSessionRun, RunError>> + Send + 'a>,
    > {
        // Return a boxed future at the application boundary. The admission
        // graph is intentionally deep; boxing here prevents protocol and
        // lifecycle callers from moving its complete state machine through a
        // default Tokio worker stack.
        Box::pin(async move {
            let session_id = command.session_id.clone();
            let agent_id = command.agent_id.clone();
            let run_id = command.run_id.clone();
            let operation = session_run_activity_operation_id(&session_id, &run_id);
            // Cause/effect decision table: an existing exact activity receipt is
            // response-loss truth and outranks current policy; without one, recover
            // the Session projection before Runtime freezes the dispatch. Reservation
            // remains non-executable until the exact epoch is activated.
            let durable_owner = match self.owner(&session_id).await {
                Ok(actual_owner) => {
                    if expected_owner.is_some_and(|expected| expected != actual_owner) {
                        return Err(RunError::bad_request("Session was not found"));
                    }
                    Some(actual_owner)
                }
                Err(SessionMutationError::NotFound) if expected_owner.is_some() => None,
                Err(error) => return Err(mutation_error(error)),
            };
            let existing_activity_epoch = match durable_owner.as_ref() {
                Some(_) => self
                    .recover_activity_for_operation(&session_id, &operation)
                    .await
                    .map_err(crate::SessionActivityError::run_error)?
                    .map(|(_, epoch)| epoch),
                None => None,
            };
            if existing_activity_epoch.is_none() {
                let owner_scope = durable_owner
                    .or_else(|| expected_owner.map(str::to_string))
                    .ok_or_else(|| RunError::bad_request("Session was not found"))?;
                self.admit_run_session(&owner_scope, &session_id, &agent_id)
                    .await?;
            }
            let reservation = self.runtime().reserve_session_run(command).await?;
            let delivery = |session_activity_epoch| SessionRunDelivery {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                session_activity_epoch,
            };

            match reservation {
                SessionRunReservation::Reserved | SessionRunReservation::AlreadyReserved => {
                    let session_activity_epoch = match existing_activity_epoch {
                        Some(epoch) => epoch,
                        None => {
                            self.begin_activity_for_operation(&session_id, &operation)
                                .await
                                .map_err(crate::SessionActivityError::run_error)?
                                .1
                        }
                    };
                    Ok(match reservation {
                        SessionRunReservation::Reserved => {
                            AdmittedSessionRun::Reserved(delivery(session_activity_epoch))
                        }
                        SessionRunReservation::AlreadyReserved => {
                            AdmittedSessionRun::AlreadyReserved(delivery(session_activity_epoch))
                        }
                        _ => unreachable!("matched reserved outcomes"),
                    })
                }
                SessionRunReservation::AlreadyActivated {
                    session_activity_epoch,
                } => {
                    if session_activity_epoch == 0 {
                        return Err(RunError::internal(
                            "activated Session Run has no activity epoch",
                        ));
                    }
                    Ok(AdmittedSessionRun::AlreadyActivated(delivery(
                        session_activity_epoch,
                    )))
                }
                SessionRunReservation::RecoveryClaimed => {
                    Ok(AdmittedSessionRun::RecoveryClaimed { session_id, run_id })
                }
                SessionRunReservation::Completed => {
                    Ok(AdmittedSessionRun::Completed { session_id, run_id })
                }
            }
        })
    }

    /// Admit, register-before-activation, and project one foreground Run from
    /// authoritative committed Thread truth. Foreground/background is only an
    /// observation policy; the durable state transition is the same admission.
    pub async fn run_admitted_session(
        &self,
        command: AdmitSessionRun,
        sink: Option<Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<StepOutcome, RunError> {
        let input_message_ids = command.input_message_ids();
        let admitted = Box::pin(self.admit_session_run(command)).await?;
        Box::pin(
            self.runtime()
                .activate_and_observe_session_run(admitted, input_message_ids, sink),
        )
        .await
    }

    /// Owner-authenticated form of [`Self::run_admitted_session`] used by
    /// public protocol adapters. Both forms converge on the same durable Run
    /// reservation and foreground observation port.
    pub async fn run_admitted_session_for_owner(
        &self,
        owner_scope: &str,
        command: AdmitSessionRun,
        sink: Option<Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
    ) -> Result<StepOutcome, RunError> {
        let input_message_ids = command.input_message_ids();
        let admitted = Box::pin(self.admit_session_run_for_owner(owner_scope, command)).await?;
        Box::pin(
            self.runtime()
                .activate_and_observe_session_run(admitted, input_message_ids, sink),
        )
        .await
    }

    /// Publish one already-admitted Session Run without observing its terminal
    /// result. Background product coordinators use this after
    /// [`Self::admit_session_run_for_owner`] and consume the canonical lifecycle
    /// feed independently; they never need access to the dispatch store.
    pub async fn activate_admitted_session_run(
        &self,
        admitted: AdmittedSessionRun,
    ) -> Result<awaken_session_contract::SessionRunActivation, RunError> {
        match admitted {
            AdmittedSessionRun::Reserved(delivery)
            | AdmittedSessionRun::AlreadyReserved(delivery)
            | AdmittedSessionRun::AlreadyActivated(delivery) => {
                self.runtime().activate_session_run(delivery).await
            }
            AdmittedSessionRun::RecoveryClaimed { .. } => {
                Ok(awaken_session_contract::SessionRunActivation::RecoveryClaimed)
            }
            AdmittedSessionRun::Completed { .. } => {
                Ok(awaken_session_contract::SessionRunActivation::Completed)
            }
        }
    }

    /// Read the one durable Session projection used by every protocol without
    /// driving Runtime effects. Queries must remain projections; realization is
    /// owned by create, run admission, Worker claims, and reconciliation.
    /// `Ok(None)` means no Session aggregate exists; callers with a separate
    /// Thread read model may still project that Thread without inventing one.
    pub async fn read_session_projection(
        &self,
        thread_id: &str,
        expected_owner: Option<&str>,
    ) -> Result<Option<RecoveredSessionProjection>, SessionProjectionRecoveryError> {
        let session = match self.session(thread_id).await {
            Ok(session) => session,
            Err(SessionRepositoryError::NotFound) => return Ok(None),
            Err(error) => {
                return Err(SessionProjectionRecoveryError::Rejected(repository_error(
                    error,
                )));
            }
        };
        if !session.is_publicly_readable() {
            return Err(SessionProjectionRecoveryError::NotFound);
        }
        let owner = self.owner(thread_id).await.map_err(|error| match error {
            SessionMutationError::NotFound => SessionProjectionRecoveryError::NotFound,
            SessionMutationError::Conflict | SessionMutationError::IdempotencyMismatch => {
                SessionProjectionRecoveryError::Unavailable(error.to_string())
            }
            SessionMutationError::Unavailable(message) => {
                SessionProjectionRecoveryError::Unavailable(message)
            }
        })?;
        if expected_owner.is_some_and(|expected| expected != owner) {
            return Err(SessionProjectionRecoveryError::NotFound);
        }
        if matches!(session.baseline, SessionBaselineState::Preparing(_)) {
            return Err(SessionProjectionRecoveryError::Unavailable(
                "Session creation finalization is incomplete".into(),
            ));
        }
        Ok(Some(RecoveredSessionProjection {
            owner_scope: owner,
            session,
        }))
    }

    /// Rebuild the disposable Runtime projection at an execution admission
    /// boundary. This is deliberately separate from [`Self::read_session_projection`].
    pub async fn recover_session_projection(
        &self,
        thread_id: &str,
        expected_owner: Option<&str>,
    ) -> Result<Option<RecoveredSessionProjection>, SessionProjectionRecoveryError> {
        let Some(recovered) = self
            .read_session_projection(thread_id, expected_owner)
            .await?
        else {
            return Ok(None);
        };
        // Archived Sessions remain readable projections but deny every new
        // effect. Recovery must not reacquire a realization lease for them.
        if recovered.session.is_terminal() {
            return Ok(Some(recovered));
        }
        self.refresh_executable_projections()
            .await
            .map_err(|error| {
                SessionProjectionRecoveryError::Unavailable(format!(
                    "executable projection refresh failed: {error}"
                ))
            })?;
        let owner = recovered.owner_scope;
        // Resource convergence is the largest Session state machine. Keep its
        // future behind one heap indirection at this application boundary so
        // protocol handlers do not synchronously embed the complete recovery,
        // credential, and Resource reconciliation graph in their poll stack.
        // The owned aggregate still crosses exactly one domain boundary; this
        // is execution-shape isolation, not another persistence abstraction.
        let session = Box::pin(self.reconcile_persisted_resources(&owner, recovered.session))
            .await
            .map_err(|error| SessionProjectionRecoveryError::Rejected(preparation_error(error)))?;
        let requires_external_realization = self.requires_external_realization(&session);
        let session = if requires_external_realization {
            self.install_dispatch_projection(&owner, &session)
                .await
                .map_err(|error| {
                    SessionProjectionRecoveryError::Rejected(realization_error(error))
                })?;
            self.dispatch_session_work(&session)
                .await
                .map_err(|error| {
                    SessionProjectionRecoveryError::Rejected(RunError::unavailable_classified(
                        "session_work_dispatch_failed",
                        format!("Session realization work could not be dispatched: {error}"),
                    ))
                })?;
            session
        } else {
            self.realize_session_after_refresh(thread_id)
                .await
                .map_err(|error| {
                    SessionProjectionRecoveryError::Rejected(realization_error(error))
                })?
        };
        Ok(Some(RecoveredSessionProjection {
            owner_scope: owner,
            session,
        }))
    }

    async fn recover_admitted_session(
        &self,
        workspace_id: &str,
        thread_id: &str,
    ) -> Result<(), RunApplicationError> {
        let recovered = self
            .recover_session_projection(thread_id, Some(workspace_id))
            .await
            .map_err(|error| match error {
                SessionProjectionRecoveryError::NotFound => {
                    RunError::bad_request("Session was not found")
                }
                SessionProjectionRecoveryError::Rejected(error) => error,
                SessionProjectionRecoveryError::Unavailable(message) => {
                    RunError::unavailable(message)
                }
            })?
            .ok_or_else(|| RunError::bad_request("Session was not found"))?;
        if recovered.session.is_terminal() {
            return Err(RunError::bad_request("Session no longer accepts new Runs"));
        }
        if let Some(baseline) = recovered
            .session
            .frozen_baseline()
            .filter(|baseline| baseline.environment.self_hosted)
        {
            self.environments
                .wake_session_work(
                    &baseline.environment.environment_id,
                    &recovered.session.session_id,
                )
                .await
                .map_err(|error| {
                    RunError::unavailable_classified(
                        "session_work_wake_failed",
                        format!("Session Work could not be awakened: {error}"),
                    )
                })?;
        }
        if !self.requires_external_realization(&recovered.session)
            && !recovered.session.execution.admits_activity()
        {
            return Err(RunError::unavailable_classified(
                "session_not_ready",
                format!(
                    "Session realization has not completed; current state is `{}`",
                    recovered.session.execution
                ),
            ));
        }
        Ok(())
    }

    pub async fn create_profiled_session(
        &self,
        command: CreateProfiledSessionCommand,
    ) -> Result<awaken_session_contract::PersistedSession, SessionCreationError> {
        self.refresh_executable_projections()
            .await
            .map_err(|error| {
                RunError::unavailable_classified(
                    "executable_projection_refresh_failed",
                    format!("Executable projections could not be refreshed: {error}"),
                )
            })?;
        let CreateProfiledSessionCommand {
            owner_scope,
            session_id,
            mutation_policy,
            agent_id,
            source_revision,
            environment_id: requested_environment_id,
            model: requested_model,
            mounts,
            env,
            prompts,
            resource_inputs,
            mut mcp_candidates,
            repositories,
            network_restriction,
            title,
            metadata,
            tools: requested_tools,
            idempotency,
        } = command;
        let profile = source_revision.map_or_else(
            || self.session_profile(&owner_scope, &agent_id),
            |revision| self.session_profile_at_revision(&owner_scope, &agent_id, revision),
        );
        if let (Some(requested), Some(resolved)) = (source_revision, profile.as_ref())
            && !awaken_executable_agent_contract::requested_profile_revision_matches(
                resolved.source_revision,
                requested,
            )
        {
            return Err(RunError::bad_request(format!(
                "agent_version_mismatch: agent `{agent_id}` returned version {} for requested version {requested}",
                resolved.source_revision
            )).into());
        }
        if let Some(revision) = source_revision
            && profile.is_none()
        {
            return Err(RunError::bad_request(format!(
                "agent_version_unavailable: agent `{agent_id}` has no executable publication at version {revision}"
            )).into());
        }
        if source_revision.is_none() && self.agent_unavailable(&owner_scope, &agent_id) {
            return Err(RunError::bad_request(format!(
                "agent_unavailable: agent `{agent_id}` cannot start a new session"
            ))
            .into());
        }
        // Model-override cause/effect rules: R1 absent -> inherit the Agent
        // publication; R2 equal -> reuse it; R3 different + resolvable -> freeze
        // the complete replacement route; R4 different + invalid/unavailable ->
        // fail before persistence or external realization.
        let published_model = profile
            .as_ref()
            .and_then(|profile| profile.model.clone())
            .unwrap_or_else(|| self.model());
        let model_override = match requested_model.as_deref() {
            Some(requested) => Some(
                self.resolve_session_model_override(
                    &owner_scope,
                    requested,
                    &published_model,
                    Default::default(),
                )
                .await?,
            ),
            None => None,
        };
        let model = requested_model.clone().unwrap_or(published_model);
        let execution_model_ref = model_override
            .as_ref()
            .and_then(|model_override| model_override.publication.as_ref())
            .map(|publication| publication.primary.binding().model_ref.clone())
            .or_else(|| {
                profile
                    .as_ref()
                    .and_then(|profile| profile.execution_model_ref.clone())
            })
            .unwrap_or_else(|| model.clone());
        let published_backend_ref = model_override
            .as_ref()
            .and_then(|model_override| model_override.publication.as_ref())
            .map(|publication| publication.primary.binding().backend_ref.clone())
            .or_else(|| {
                profile
                    .as_ref()
                    .map(|profile| profile.backend_ref.trim())
                    .filter(|backend| !backend.is_empty())
                    .map(str::to_owned)
            });
        let capabilities = self.capabilities_for(&session_id);
        let capability_tools = SessionToolConfiguration::from_capabilities(&capabilities);
        let inherited_tools = profile.as_ref().map_or_else(
            || capability_tools.clone(),
            |profile| SessionToolConfiguration {
                toolsets: if profile.toolsets.is_empty() {
                    capability_tools.toolsets.clone()
                } else {
                    profile.toolsets.clone()
                },
                client_tools: profile.client_tools.clone(),
            },
        );
        let tools = requested_tools.unwrap_or(inherited_tools);
        let skills = profile
            .as_ref()
            .map(|profile| profile.skills.clone())
            .unwrap_or_default();
        self.validate_session_skill_total(&owner_scope, &agent_id, profile.as_ref(), &skills)?;
        mcp_candidates.extend(
            profile
                .as_ref()
                .into_iter()
                .flat_map(|profile| &profile.mcp_servers)
                .map(|server| {
                    let published_credential = match (
                        server.credential_source_id.as_ref(),
                        server.credential_revision,
                    ) {
                        (None, None) => Ok(None),
                        (Some(id), Some(revision)) if !id.trim().is_empty() && revision > 0 => {
                            Ok(Some((id.clone(), revision)))
                        }
                        _ => Err(RunError::bad_request(
                            "published MCP credential pin is incomplete",
                        )),
                    }?;
                    Ok(McpAttachmentCandidate {
                        name: server.name.clone(),
                        target: McpAttachmentCandidateTarget::Normalized(server.target.clone()),
                        prompts_as_skills: server.prompts_as_skills,
                        published_credential,
                        origin: McpAttachmentOrigin::Agent,
                    })
                })
                .collect::<Result<Vec<_>, RunError>>()?,
        );
        let (mcp_candidates, mcp_targets) = Self::normalize_mcp_candidate_targets(mcp_candidates)?;
        let mut environment = self
            .resolve_session_environment(
                requested_environment_id.as_deref(),
                profile
                    .as_ref()
                    .and_then(|profile| profile.environment.as_ref()),
                published_backend_ref.as_deref(),
                &mcp_targets,
            )
            .await?
            .snapshot;
        let initial_mcp = self
            .normalize_mcp_drafts(
                &owner_scope,
                mcp_candidates,
                &[],
                &environment.credential_realization.mcp_holder,
            )
            .await?;
        if let Some(restriction) = network_restriction {
            environment.network = environment.network.safe_intersection(&restriction);
        }
        let agent_resources = profile
            .as_ref()
            .map(|profile| profile.resources.as_slice())
            .unwrap_or_default();
        let repository_owner = SessionRepositoryOwner::profiled(&session_id);
        for repository in &repositories {
            if repository.binding_id.as_str().trim().is_empty()
                || repository.repository.workspace_id != owner_scope
                || !repository_owner.owns_repository_id(&repository.repository.id)
            {
                return Err(RunError::bad_request(
                    "Profiled Session Repository binding is empty or outside its Session or Workspace",
                )
                .into());
            }
            if !mutation_policy.admits_repository_credential_mutation()
                && repository.repository.authorization_token.is_some()
            {
                return Err(RunError::bad_request(
                    "profiled Session Repository credentials must be pre-existing Vault references",
                )
                .into());
            }
        }
        let mut collision_preflight = resource_inputs.clone();
        collision_preflight.extend(repositories.iter().map(|repository| {
            profiled_repository_attachment(
                awaken_resource_contract::RepositoryId::from(repository.repository.id.clone()),
                repository.repository.mount_path.clone(),
                repository.binding_id.clone(),
            )
        }));
        awaken_session_contract::SessionInputResolver::effective_bindings(
            agent_resources,
            &collision_preflight,
        )
        .map_err(|error| RunError::bad_request(error.to_string()))?;
        let resolved_skills = if skills.is_empty() {
            None
        } else {
            Some(self.resolve_session_skills(&owner_scope, &skills).await?)
        };
        let mut repository_attachments = resource_inputs;
        repository_attachments.reserve(repositories.len());
        let mut expected_repository_credentials = BTreeMap::new();
        let mut repository_configurations = Vec::<ConfiguredSessionRepository>::new();
        for repository in repositories {
            let mount_path = repository.repository.mount_path.clone();
            let expected_credential = repository.repository.credential.clone();
            let binding_id = repository.binding_id;
            let configured = match self
                .configure_session_repository(repository.repository)
                .await
            {
                Ok(configured) => configured,
                Err(first) => {
                    if !self
                        .abort_unadopted_session_repositories(&repository_configurations)
                        .await
                    {
                        tracing::warn!(
                            session = %session_id,
                            "Profiled Session Repository compensation remains pending after configuration failure"
                        );
                    }
                    return Err(first.into());
                }
            };
            let repository_id = configured.repository_id.clone();
            expected_repository_credentials.insert(repository_id.clone(), expected_credential);
            repository_attachments.push(profiled_repository_attachment(
                repository_id,
                mount_path,
                binding_id,
            ));
            repository_configurations.push(configured);
        }
        let mut resources = match self.resolve_session_inputs(
            &owner_scope,
            agent_resources,
            &repository_attachments,
        ) {
            Ok(resources) => resources,
            Err(first) => {
                if !self
                    .abort_unadopted_session_repositories(&repository_configurations)
                    .await
                {
                    tracing::warn!(
                        session = %session_id,
                        "Profiled Session Repository compensation remains pending after input resolution"
                    );
                }
                return Err(first.into());
            }
        };
        if let Some(resolved_skills) = resolved_skills {
            resources = match resources.with_skills(resolved_skills) {
                Ok(resources) => resources,
                Err(error) => {
                    let first = RunError::bad_request(error.to_string());
                    if !self
                        .abort_unadopted_session_repositories(&repository_configurations)
                        .await
                    {
                        tracing::warn!(
                            session = %session_id,
                            "Profiled Session Repository compensation remains pending after Skill binding"
                        );
                    }
                    return Err(first.into());
                }
            };
        }
        if let Err(error) = self
            .pin_repository_credentials(
                &owner_scope,
                &environment.credential_realization.resource_holder,
                &mut resources,
            )
            .await
        {
            let first = preparation_error(error);
            if !self
                .abort_unadopted_session_repositories(&repository_configurations)
                .await
            {
                tracing::warn!(
                    session = %session_id,
                    "Profiled Session Repository compensation remains pending after credential pinning"
                );
            }
            return Err(first.into());
        }
        for input in resources.inputs() {
            let awaken_session_contract::ResolvedInputSource::Repository {
                repository_id,
                credential,
                ..
            } = &input.source
            else {
                continue;
            };
            let Some(expected) = expected_repository_credentials.get(repository_id) else {
                continue;
            };
            if credential.as_deref().map(|pin| &pin.access.credential) != expected.as_ref() {
                let first = RunError::bad_request(format!(
                    "repository `{repository_id}` credential revision changed before Session admission"
                ));
                if !self
                    .abort_unadopted_session_repositories(&repository_configurations)
                    .await
                {
                    tracing::warn!(
                        session = %session_id,
                        "Profiled Session Repository compensation remains pending after credential drift"
                    );
                }
                return Err(first.into());
            }
        }
        let intent = SessionCreationIntent {
            control: ControlSessionCreationInputs {
                mutation_policy,
                environment,
                runtime_placement: self.runtime_placement(),
                agent_id,
                agent_revision: profile.as_ref().map(|profile| profile.source_revision),
                model,
                execution_model_ref,
                model_override,
                system_prompt: awaken_session_contract::SessionSystemPromptSelection::Inherit,
                runtime: published_backend_ref,
                mcp_authoring: SessionMcpAuthoringContext::default(),
                delegate_ids: profile
                    .as_ref()
                    .map(|profile| {
                        profile
                            .delegates
                            .iter()
                            .map(|delegate| delegate.agent_id.clone())
                            .collect()
                    })
                    .unwrap_or_default(),
                toolsets: tools.toolsets.clone(),
                mounts,
                env,
                prompts,
                transcript_prefix: None,
                resources,
                initial_mcp,
            },
        };
        self.create_session(CreateSessionCommand {
            owner_scope,
            session_id,
            intent,
            title,
            metadata,
            tools,
            budget: awaken_session_contract::SessionBudgetState::Absent,
            repository_configurations,
            idempotency,
            initial_events: None,
        })
        .await
    }

    async fn create_default_run_session(
        &self,
        workspace_id: &str,
        thread_id: &str,
        agent_id: &str,
    ) -> Result<(), RunApplicationError> {
        self.create_profiled_session(CreateProfiledSessionCommand {
            owner_scope: workspace_id.to_string(),
            session_id: thread_id.to_string(),
            mutation_policy: awaken_session_contract::SessionMutationPolicy::Managed,
            agent_id: agent_id.to_string(),
            source_revision: None,
            environment_id: None,
            model: None,
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            resource_inputs: Vec::new(),
            mcp_candidates: Vec::new(),
            repositories: Vec::new(),
            network_restriction: None,
            title: None,
            metadata: Default::default(),
            tools: None,
            idempotency: None,
        })
        .await
        .map_err(creation_error)
        .map(|_| ())
    }

    pub async fn admit_run_session(
        &self,
        workspace_id: &str,
        thread_id: &str,
        agent_id: &str,
    ) -> Result<(), RunApplicationError> {
        match self.session(thread_id).await {
            Ok(_) => self.recover_admitted_session(workspace_id, thread_id).await,
            Err(SessionRepositoryError::NotFound) => {
                match self
                    .create_default_run_session(workspace_id, thread_id, agent_id)
                    .await
                {
                    Ok(()) => Ok(()),
                    // Concurrent first Runs race only at the durable create fence;
                    // every loser adopts the exact winner through the same recovery.
                    Err(error)
                        if error.kind == awaken_session_contract::RunErrorKind::Unavailable =>
                    {
                        match self.session(thread_id).await {
                            Ok(_) => self.recover_admitted_session(workspace_id, thread_id).await,
                            Err(SessionRepositoryError::NotFound) => Err(error),
                            Err(error) => Err(repository_error(error)),
                        }
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(repository_error(error)),
        }
    }
}

/// The sole durable Session Run application for all public Run protocols.
type WorkspaceResolver = dyn Fn(&str) -> String + Send + Sync;
type ProjectedAgentResolver = dyn Fn(&str) -> Option<String> + Send + Sync;

pub struct SessionRunApplication {
    runtime: Arc<dyn RunApplication>,
    sessions: Arc<SessionApplication>,
    workspace: Arc<WorkspaceResolver>,
    projected_agent: Arc<ProjectedAgentResolver>,
}

impl SessionRunApplication {
    #[must_use]
    pub fn new(
        runtime: Arc<dyn RunApplication>,
        sessions: Arc<SessionApplication>,
        workspace: impl Fn(&str) -> String + Send + Sync + 'static,
        projected_agent: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            runtime,
            sessions,
            workspace: Arc::new(workspace),
            projected_agent: Arc::new(projected_agent),
        }
    }

    fn agent_for(&self, thread: &str, requested_agent: Option<&str>) -> String {
        let projected = (self.projected_agent)(thread);
        requested_agent
            .or(projected.as_deref())
            .unwrap_or("assistant")
            .to_string()
    }

    fn admission_command(
        &self,
        operation_id: &str,
        thread: &str,
        requested_agent: Option<&str>,
        messages: Vec<Message>,
        traceparent: Option<String>,
        replacement: awaken_session_contract::SessionRunReplacement,
    ) -> Result<AdmitSessionRun, RunApplicationError> {
        if operation_id.trim().is_empty() {
            return Err(RunError::bad_request(
                "Session Run requires a stable operation identity",
            ));
        }
        if messages.is_empty() {
            return Err(RunError::bad_request(
                "Session Run requires at least one new input message",
            ));
        }
        Ok(AdmitSessionRun {
            session_id: thread.to_string(),
            agent_id: self.agent_for(thread, requested_agent),
            operation_id: operation_id.to_string(),
            run_id: awaken_session_contract::session_run_id(thread, operation_id),
            messages,
            data_subject_id: None,
            traceparent,
            execution_requirements: Default::default(),
            replacement,
        })
    }

    async fn run_admitted(
        &self,
        operation_id: &str,
        thread: &str,
        requested_agent: Option<String>,
        messages: Vec<Message>,
        sink: Option<Arc<dyn awaken_agent_contract::stream::sink::Sink>>,
        replacement: awaken_session_contract::SessionRunReplacement,
    ) -> Result<StepOutcome, RunApplicationError> {
        let command = self.admission_command(
            operation_id,
            thread,
            requested_agent.as_deref(),
            messages,
            None,
            replacement,
        )?;
        let owner_scope = (self.workspace)(thread);
        Box::pin(
            self.sessions
                .run_admitted_session_for_owner(&owner_scope, command, sink),
        )
        .await
    }
}

#[async_trait::async_trait]
impl RunApplication for SessionRunApplication {
    async fn run(
        &self,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        self.run_admitted(
            operation_id,
            thread,
            agent,
            messages,
            None,
            awaken_session_contract::SessionRunReplacement::PreservePrior,
        )
        .await
    }

    async fn run_streaming(
        &self,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
        sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunApplicationError> {
        self.run_admitted(
            operation_id,
            thread,
            agent,
            messages,
            Some(sink),
            awaken_session_contract::SessionRunReplacement::PreservePrior,
        )
        .await
    }

    async fn resume(
        &self,
        operation_id: &str,
        thread: &str,
        tool_use_id: &str,
        resume: RunResume,
    ) -> Result<StepOutcome, RunApplicationError> {
        self.sessions
            .resume_session_run_for_owner(
                &(self.workspace)(thread),
                operation_id,
                thread,
                tool_use_id,
                resume,
            )
            .await
    }

    async fn interrupt(&self, thread: &str) -> Result<(), RunApplicationError> {
        self.runtime.interrupt(thread).await
    }

    async fn pending(&self, thread: &str) -> Result<Option<Pending>, RunApplicationError> {
        self.runtime.pending(thread).await
    }

    async fn history(&self, thread: &str) -> Result<Vec<Message>, RunApplicationError> {
        self.runtime.history(thread).await
    }

    fn model(&self) -> String {
        self.runtime.model()
    }

    async fn usage(&self, thread: &str) -> Result<(u64, u64), RunApplicationError> {
        self.runtime.usage(thread).await
    }
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionRunReplacementApplication for SessionRunApplication {
    async fn supersede_session_run(
        &self,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        self.run_admitted(
            operation_id,
            thread,
            agent,
            messages,
            None,
            awaken_session_contract::SessionRunReplacement::SupersedePrior,
        )
        .await
    }
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionRunBackgroundApplication for SessionRunApplication {
    async fn submit_session_run_background(
        &self,
        operation_id: &str,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
        traceparent: Option<String>,
    ) -> Result<awaken_agent_contract::agent::run::Id, RunApplicationError> {
        let command = self.admission_command(
            operation_id,
            thread,
            agent.as_deref(),
            messages,
            traceparent,
            awaken_session_contract::SessionRunReplacement::PreservePrior,
        )?;
        let run_id = command.run_id.clone();
        let owner_scope = (self.workspace)(thread);
        let admitted = Box::pin(
            self.sessions
                .admit_session_run_for_owner(&owner_scope, command),
        )
        .await?;
        self.sessions
            .activate_admitted_session_run(admitted)
            .await?;
        Ok(run_id)
    }
}
