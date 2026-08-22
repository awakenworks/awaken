//! Session-owned admission in front of the neutral Run application port.
//!
//! Runtime Host executes an already-admitted Run. Session creation/recovery is
//! application policy, so it is configured here once for every public protocol.

use std::collections::BTreeMap;
use std::sync::Arc;

#[cfg(test)]
use awaken_agent_contract::agent::awaiting::PermissionDecision;
use awaken_agent_contract::agent::message::Message;
use awaken_session_contract::{
    ControlSessionCreationInputs, McpAttachmentOrigin, Pending, RunApplication,
    RunApplicationError, RunError, RunResume, SessionBaselineState, SessionCreationIntent,
    SessionMcpAuthoringContext, SessionNetworkPolicy, SessionRepositoryError,
    SessionToolConfiguration, StepOutcome,
};

use crate::{
    CreateSessionCommand, McpAttachmentCandidate, McpAttachmentCandidateTarget, SessionApplication,
    SessionCreationError, SessionMutationError, SessionPreparationError, SessionRealizationError,
    SessionRepositoryResourceInput,
};

#[async_trait::async_trait]
pub trait SessionRunAdmission: Send + Sync {
    async fn admit(
        &self,
        workspace_id: &str,
        thread_id: &str,
        agent_id: &str,
    ) -> Result<(), RunApplicationError>;
}

/// Protocol-independent request to create a Session from one published Agent
/// profile. The Session application resolves Environment, MCP, Resources,
/// credentials, tools, and immutable execution identity exactly once.
pub struct CreateProfiledSessionCommand {
    pub owner_scope: String,
    pub session_id: String,
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
    /// Explicit Session candidates supplied by the product adapter. Published
    /// Agent candidates are joined and normalized inside the sole composer.
    pub mcp_candidates: Vec<McpAttachmentCandidate>,
    pub repositories: Vec<SessionRepositoryResourceInput>,
    pub network_restriction: Option<SessionNetworkPolicy>,
    pub title: Option<String>,
    pub metadata: BTreeMap<String, String>,
    pub tools: Option<SessionToolConfiguration>,
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
        SessionCreationError::IdempotencyMismatch => {
            RunError::internal("Session creation idempotency mismatch")
        }
        SessionCreationError::Unavailable(message) => RunError::unavailable(message),
    }
}

impl SessionApplication {
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
        if !session.is_publicly_readable() || session.is_hidden() {
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
            self.realize_session(thread_id).await.map_err(|error| {
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
    ) -> Result<awaken_session_contract::PersistedSession, RunError> {
        let CreateProfiledSessionCommand {
            owner_scope,
            session_id,
            agent_id,
            source_revision,
            environment_id: requested_environment_id,
            model: requested_model,
            mounts,
            env,
            prompts,
            mut mcp_candidates,
            repositories,
            network_restriction,
            title,
            metadata,
            tools: requested_tools,
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
            )));
        }
        if let Some(revision) = source_revision
            && profile.is_none()
        {
            return Err(RunError::bad_request(format!(
                "agent_version_unavailable: agent `{agent_id}` has no executable publication at version {revision}"
            )));
        }
        if source_revision.is_none() && self.agent_unavailable(&owner_scope, &agent_id) {
            return Err(RunError::bad_request(format!(
                "agent_unavailable: agent `{agent_id}` cannot start a new session"
            )));
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
        let initial_mcp = self
            .normalize_mcp_drafts(&owner_scope, mcp_candidates, &[])
            .await?;
        let mcp_targets = initial_mcp
            .iter()
            .map(|attachment| attachment.target.clone())
            .collect::<Vec<_>>();
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
        if let Some(restriction) = network_restriction {
            environment.network = environment.network.safe_intersection(&restriction);
        }
        let agent_resources = profile
            .as_ref()
            .map(|profile| profile.resources.as_slice())
            .unwrap_or_default();
        let mut repository_attachments = Vec::with_capacity(repositories.len());
        let mut expected_repository_credentials = BTreeMap::new();
        for repository in repositories {
            let mount_path = repository.mount_path.clone();
            let expected_credential = repository.credential.clone();
            let repository_id = self.configure_session_repository(repository).await?;
            expected_repository_credentials.insert(repository_id.clone(), expected_credential);
            repository_attachments.push(awaken_session_contract::SessionInputAttachment {
                binding: awaken_resource_contract::InputBinding {
                    binding_id: awaken_resource_contract::BindingId::new(format!(
                        "profiled:{session_id}:repository:{}",
                        repository_id.as_str()
                    )),
                    target: awaken_resource_contract::InputResourceId::Repository(repository_id),
                    mount_path,
                    access: awaken_resource_contract::ResourceAccess::ReadWrite,
                    instructions: None,
                },
                replaces: None,
            });
        }
        let mut resources =
            self.resolve_session_inputs(&owner_scope, agent_resources, &repository_attachments)?;
        if !skills.is_empty() {
            resources = resources
                .with_skills(self.resolve_session_skills(&owner_scope, &skills).await?)
                .map_err(|error| RunError::bad_request(error.to_string()))?;
        }
        self.pin_repository_credentials(
            &owner_scope,
            &environment.credential_realization.resource_holder,
            &mut resources,
        )
        .await
        .map_err(preparation_error)?;
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
                return Err(RunError::bad_request(format!(
                    "repository `{repository_id}` credential revision changed before Session admission"
                )));
            }
        }
        let intent = SessionCreationIntent {
            control: ControlSessionCreationInputs {
                environment,
                runtime_placement: self.runtime_placement(),
                agent_id,
                agent_revision: profile.as_ref().map(|profile| profile.source_revision),
                model,
                execution_model_ref,
                model_override,
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
        })
        .await
        .map_err(creation_error)
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
            agent_id: agent_id.to_string(),
            source_revision: None,
            environment_id: None,
            model: None,
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            mcp_candidates: Vec::new(),
            repositories: Vec::new(),
            network_restriction: None,
            title: None,
            metadata: Default::default(),
            tools: None,
        })
        .await
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
                    // Concurrent first turns race only at the durable create fence;
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

#[async_trait::async_trait]
impl SessionRunAdmission for SessionApplication {
    async fn admit(
        &self,
        workspace_id: &str,
        thread_id: &str,
        agent_id: &str,
    ) -> Result<(), RunApplicationError> {
        self.admit_run_session(workspace_id, thread_id, agent_id)
            .await
    }
}

/// The sole admission decorator for all public Run protocols.
type WorkspaceResolver = dyn Fn(&str) -> String + Send + Sync;
type ProjectedAgentResolver = dyn Fn(&str) -> Option<String> + Send + Sync;

pub struct AdmittedRunApplication {
    runtime: Arc<dyn RunApplication>,
    admission: Arc<dyn SessionRunAdmission>,
    workspace: Arc<WorkspaceResolver>,
    projected_agent: Arc<ProjectedAgentResolver>,
}

impl AdmittedRunApplication {
    #[must_use]
    pub fn new(
        runtime: Arc<dyn RunApplication>,
        admission: Arc<dyn SessionRunAdmission>,
        workspace: impl Fn(&str) -> String + Send + Sync + 'static,
        projected_agent: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            runtime,
            admission,
            workspace: Arc::new(workspace),
            projected_agent: Arc::new(projected_agent),
        }
    }

    async fn admit_run(
        &self,
        thread: &str,
        requested_agent: Option<&str>,
    ) -> Result<(), RunApplicationError> {
        let projected = (self.projected_agent)(thread);
        self.admission
            .admit(
                &(self.workspace)(thread),
                thread,
                requested_agent
                    .or(projected.as_deref())
                    .unwrap_or("assistant"),
            )
            .await
    }
}

#[async_trait::async_trait]
impl RunApplication for AdmittedRunApplication {
    async fn run(
        &self,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
    ) -> Result<StepOutcome, RunApplicationError> {
        self.admit_run(thread, agent.as_deref()).await?;
        self.runtime.run(thread, agent, messages).await
    }

    async fn run_streaming(
        &self,
        thread: &str,
        agent: Option<String>,
        messages: Vec<Message>,
        sink: Arc<dyn awaken_agent_contract::stream::sink::Sink>,
    ) -> Result<StepOutcome, RunApplicationError> {
        self.admit_run(thread, agent.as_deref()).await?;
        self.runtime
            .run_streaming(thread, agent, messages, sink)
            .await
    }

    async fn resume(
        &self,
        thread: &str,
        tool_use_id: &str,
        resume: RunResume,
    ) -> Result<StepOutcome, RunApplicationError> {
        self.admit_run(thread, None).await?;
        self.runtime.resume(thread, tool_use_id, resume).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Admission {
        calls: Mutex<Vec<(String, String, String)>>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl SessionRunAdmission for Admission {
        async fn admit(
            &self,
            workspace_id: &str,
            thread_id: &str,
            agent_id: &str,
        ) -> Result<(), RunApplicationError> {
            self.calls.lock().unwrap().push((
                workspace_id.to_owned(),
                thread_id.to_owned(),
                agent_id.to_owned(),
            ));
            if self.fail {
                Err(RunApplicationError::unavailable("session store offline"))
            } else {
                Ok(())
            }
        }
    }

    struct Runtime(AtomicUsize);

    #[async_trait::async_trait]
    impl RunApplication for Runtime {
        async fn run(
            &self,
            _thread: &str,
            _agent: Option<String>,
            _messages: Vec<Message>,
        ) -> Result<StepOutcome, RunApplicationError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(StepOutcome::ended(
                Vec::new(),
                awaken_agent_contract::agent::run::EndCause::NaturalEnd,
                false,
                false,
            ))
        }

        async fn resume(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _resume: RunResume,
        ) -> Result<StepOutcome, RunApplicationError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(StepOutcome::ended(
                Vec::new(),
                awaken_agent_contract::agent::run::EndCause::NaturalEnd,
                false,
                false,
            ))
        }

        async fn pending(&self, _thread: &str) -> Result<Option<Pending>, RunApplicationError> {
            Ok(None)
        }

        async fn history(&self, _thread: &str) -> Result<Vec<Message>, RunApplicationError> {
            Ok(Vec::new())
        }

        fn model(&self) -> String {
            "test".into()
        }
    }

    #[tokio::test]
    async fn session_admission_is_the_single_gate_before_run_execution() {
        // Cause/effect decision table: R1 explicit Agent + admission success =>
        // exact Agent admitted then one Run; R2 no Agent + recovered projection =>
        // projected Agent admitted; R3 admission unavailable => zero Runs and the
        // retryable error preserved; R4 read-only history => no admission; R5 a
        // continuation is admitted before Runtime resume. These
        // rules keep application policy out of Runtime Host without introducing a
        // second execution path.
        let admission = Arc::new(Admission {
            calls: Mutex::new(Vec::new()),
            fail: false,
        });
        let runtime = Arc::new(Runtime(AtomicUsize::new(0)));
        let app = AdmittedRunApplication::new(
            runtime.clone(),
            admission.clone(),
            |_| "workspace-a".into(),
            |_| Some("projected-agent".into()),
        );

        app.run("thread-a", Some("explicit-agent".into()), Vec::new())
            .await
            .expect("R1");
        app.run("thread-b", None, Vec::new()).await.expect("R2");
        app.resume(
            "thread-resume",
            "tool-a",
            RunResume::Permission(PermissionDecision::Allow { note: None }),
        )
        .await
        .expect("R5");
        app.history("thread-a").await.expect("R4");
        assert_eq!(runtime.0.load(Ordering::SeqCst), 3, "R1/R2/R4/R5");
        assert_eq!(
            admission.calls.lock().unwrap().as_slice(),
            [
                (
                    "workspace-a".into(),
                    "thread-a".into(),
                    "explicit-agent".into()
                ),
                (
                    "workspace-a".into(),
                    "thread-b".into(),
                    "projected-agent".into()
                ),
                (
                    "workspace-a".into(),
                    "thread-resume".into(),
                    "projected-agent".into()
                ),
            ],
            "R1/R2/R4/R5"
        );

        let denied_runtime = Arc::new(Runtime(AtomicUsize::new(0)));
        let denied = AdmittedRunApplication::new(
            denied_runtime.clone(),
            Arc::new(Admission {
                calls: Mutex::new(Vec::new()),
                fail: true,
            }),
            |_| "workspace-a".into(),
            |_| None,
        );
        let error = denied.run("thread-c", None, Vec::new()).await.unwrap_err();
        assert_eq!(
            error.kind,
            awaken_session_contract::RunErrorKind::Unavailable,
            "R3"
        );
        assert_eq!(denied_runtime.0.load(Ordering::SeqCst), 0, "R3");
    }
}
