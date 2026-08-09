//! Coordinator-owned Session application state and ports.
//!
//! Protocol adapters retain DTO projection, route parsing, and transient stream
//! caches.  This crate owns the application collaborators and durable Session
//! repository so every protocol drives the same Session authority.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use awaken_agent_contract::RedactedString;
use awaken_credential_contract::{
    CredentialAccess, CredentialExecutionPolicy, CredentialSourceId, CredentialUsage,
};
use awaken_environment_contract::EnvItem;
use awaken_environment_realization_contract::EnvironmentImageBuildError;
use awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError;
use awaken_session_contract::{
    ManagedSessionRepository, McpAttachmentRealizer, McpTarget, PersistedSession, RunError,
    SandboxProvisioning, SessionEnvironmentBindingSink, SessionLifecycleSink, SessionRuntime,
    SessionRuntimePlacement,
};

mod mutation;
pub use mutation::SessionMutationError;
mod activity;
pub use activity::SessionActivityError;
mod contribution;
mod credentials;
mod projection;
pub use credentials::SessionPreparationError;
mod mcp;
mod realization;
mod resource_reconciliation;
pub use mcp::{McpAttachmentCandidate, McpAttachmentCandidateTarget};
pub use realization::{
    SessionRealizationError, SessionReconciliation, SessionReconciliationFailure,
};

/// Secret-free credential selection used while compiling a Session.
#[async_trait::async_trait]
pub trait SessionCredentialSource: Send + Sync {
    async fn has_vault(&self, id: &str) -> Result<bool, String>;

    async fn mcp_credential_source_for_url(
        &self,
        vault_ids: &[String],
        url: &str,
    ) -> Result<Option<CredentialSourceId>, String>;

    async fn mcp_access_for_source(
        &self,
        source_id: &CredentialSourceId,
    ) -> Result<CredentialAccess, String>;

    async fn credential_access_for_source(
        &self,
        source_id: &CredentialSourceId,
        workspace_id: &str,
        usage: CredentialUsage,
        policy: CredentialExecutionPolicy,
    ) -> Result<CredentialAccess, String>;
}

/// Write-only ingress for repository material. Implementations seal material
/// before returning the opaque source id used by Session compilation.
#[async_trait::async_trait]
pub trait RepositoryCredentialIngress: Send + Sync {
    async fn enter_repository_token(
        &self,
        source_id: CredentialSourceId,
        workspace_id: &str,
        token: RedactedString,
    ) -> Result<CredentialSourceId, String>;

    async fn rotate_repository_token(
        &self,
        source_id: &CredentialSourceId,
        workspace_id: &str,
        token: RedactedString,
    ) -> Result<(), String>;
}

/// Exact executable Environment projection resolved for one Session creation.
pub struct ResolvedSessionEnvironment {
    pub snapshot: awaken_session_contract::EnvironmentSnapshot,
}

/// Coordinator projection consumed by Session compilation.  Environment HTTP
/// routes may use the same concrete adapter, but their wire types do not cross
/// this port.
#[async_trait::async_trait]
pub trait SessionEnvironmentSource: Send + Sync {
    async fn get(
        &self,
        environment_id: &str,
    ) -> Result<Option<EnvItem>, ExecutableEnvironmentRegistrationError>;

    async fn resolve_current_for_session(
        &self,
        environment_id: &str,
        runtime: Option<&str>,
        mcp_targets: &[McpTarget],
    ) -> Result<Option<ResolvedSessionEnvironment>, EnvironmentImageBuildError>;

    async fn resolve_exact_for_session(
        &self,
        environment_id: &str,
        revision: u64,
        runtime: Option<&str>,
        mcp_targets: &[McpTarget],
    ) -> Result<Option<ResolvedSessionEnvironment>, EnvironmentImageBuildError>;

    async fn enqueue_session_work(
        &self,
        environment_id: &str,
        session_id: &str,
    ) -> Result<String, awaken_session_contract::work_queue::WorkQueueError>;
}

/// One failed durable Session-to-WorkQueue projection. The durable Session
/// remains authoritative and a later reconciliation may retry this failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkDispatchFailure {
    pub session_id: String,
    pub environment_id: String,
    pub message: String,
}

/// Result of one complete reconciliation scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkDispatchReconciliation {
    pub settled: usize,
    pub failures: Vec<WorkDispatchFailure>,
}

/// Process-level execution topology interpreted by the Session application.
/// The selected value is frozen into every newly admitted Session; protocol
/// adapters never inspect or override it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SessionExecutionPlacement {
    #[default]
    LocalWorker,
    RegisteredWorker,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionApplicationConfiguration {
    pub execution_placement: SessionExecutionPlacement,
}

/// Canonical application object shared by wire adapters.
pub struct SessionApplication {
    configuration: SessionApplicationConfiguration,
    runtime: Arc<dyn SessionRuntime>,
    mcp_realizer: Arc<dyn McpAttachmentRealizer>,
    credential_source: Option<Arc<dyn SessionCredentialSource>>,
    repository_credential_ingress: Option<Arc<dyn RepositoryCredentialIngress>>,
    environments: Arc<dyn SessionEnvironmentSource>,
    config_source: Option<Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>>,
    resource_catalog: Option<Arc<dyn awaken_resource_contract::ResourceCatalog>>,
    resource_purge_scheduler: Option<Arc<dyn awaken_resource_contract::ResourcePurgeScheduler>>,
    sessions_repo: Arc<dyn ManagedSessionRepository>,
    lifecycle_sink: Option<Arc<dyn SessionLifecycleSink>>,
    runtime_incarnation: String,
    lifecycle_supervisor_started: AtomicBool,
}

static APPLICATION_INCARNATION_SEQ: AtomicU64 = AtomicU64::new(0);

impl SessionApplication {
    #[must_use]
    pub fn new(
        runtime: Arc<dyn SessionRuntime>,
        mcp_realizer: Arc<dyn McpAttachmentRealizer>,
        sessions_repo: Arc<dyn ManagedSessionRepository>,
        environments: Arc<dyn SessionEnvironmentSource>,
    ) -> Self {
        Self::new_with_configuration(
            runtime,
            mcp_realizer,
            sessions_repo,
            environments,
            SessionApplicationConfiguration::default(),
        )
    }

    #[must_use]
    pub fn new_with_configuration(
        runtime: Arc<dyn SessionRuntime>,
        mcp_realizer: Arc<dyn McpAttachmentRealizer>,
        sessions_repo: Arc<dyn ManagedSessionRepository>,
        environments: Arc<dyn SessionEnvironmentSource>,
        configuration: SessionApplicationConfiguration,
    ) -> Self {
        runtime.install_environment_binding_sink(Arc::new(RepositoryEnvironmentBindingSink::new(
            sessions_repo.clone(),
        )));
        let started_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        Self {
            configuration,
            runtime,
            mcp_realizer,
            credential_source: None,
            repository_credential_ingress: None,
            environments,
            config_source: None,
            resource_catalog: None,
            resource_purge_scheduler: None,
            sessions_repo,
            lifecycle_sink: None,
            runtime_incarnation: format!(
                "session:{}:{started_at}:{}",
                std::process::id(),
                APPLICATION_INCARNATION_SEQ.fetch_add(1, Ordering::Relaxed)
            ),
            lifecycle_supervisor_started: AtomicBool::new(false),
        }
    }

    /// Exact immutable fact written into a newly compiled Session baseline.
    #[must_use]
    pub fn runtime_placement(&self) -> SessionRuntimePlacement {
        match self.configuration.execution_placement {
            SessionExecutionPlacement::LocalWorker => SessionRuntimePlacement::Local,
            SessionExecutionPlacement::RegisteredWorker => SessionRuntimePlacement::Worker,
        }
    }

    /// Resolve the sole physical-realization owner. `LegacyUnspecified` is an
    /// explicit upgrade state and is the only case allowed to consult current
    /// process topology; explicit frozen facts remain immutable across restarts.
    #[must_use]
    pub fn requires_external_realization(&self, session: &PersistedSession) -> bool {
        session.frozen_baseline().is_some_and(|baseline| {
            baseline.application.is_some()
                || match baseline.runtime_placement {
                    SessionRuntimePlacement::LegacyUnspecified => {
                        self.configuration.execution_placement
                            == SessionExecutionPlacement::RegisteredWorker
                    }
                    SessionRuntimePlacement::Local => false,
                    SessionRuntimePlacement::Worker => true,
                }
        })
    }

    /// Claim the one lifecycle supervisor for this application instance.
    pub fn claim_lifecycle_supervisor(&self) -> bool {
        claim_once(&self.lifecycle_supervisor_started)
    }

    #[must_use]
    pub fn runtime(&self) -> &dyn SessionRuntime {
        self.runtime.as_ref()
    }

    #[must_use]
    pub fn mcp_realizer(&self) -> &dyn McpAttachmentRealizer {
        self.mcp_realizer.as_ref()
    }

    #[must_use]
    pub fn credential_source(&self) -> Option<&dyn SessionCredentialSource> {
        self.credential_source.as_deref()
    }

    #[must_use]
    pub fn repository_credential_ingress(&self) -> Option<&dyn RepositoryCredentialIngress> {
        self.repository_credential_ingress.as_deref()
    }

    #[must_use]
    pub fn config_source(
        &self,
    ) -> Option<&dyn awaken_executable_agent_contract::ExecutableAgentProfileSource> {
        self.config_source.as_deref()
    }

    #[must_use]
    pub fn resource_catalog(&self) -> Option<&dyn awaken_resource_contract::ResourceCatalog> {
        self.resource_catalog.as_deref()
    }

    #[must_use]
    pub fn resource_purge_scheduler(
        &self,
    ) -> Option<&dyn awaken_resource_contract::ResourcePurgeScheduler> {
        self.resource_purge_scheduler.as_deref()
    }

    #[must_use]
    pub fn session_repository(&self) -> &dyn ManagedSessionRepository {
        self.sessions_repo.as_ref()
    }

    #[must_use]
    pub fn lifecycle_sink(&self) -> Option<&dyn SessionLifecycleSink> {
        self.lifecycle_sink.as_deref()
    }

    #[must_use]
    pub fn runtime_incarnation(&self) -> &str {
        &self.runtime_incarnation
    }

    /// Read the current executable Environment projection for Deployment admission.
    pub async fn deployment_environment(
        &self,
        environment_id: &str,
    ) -> Result<Option<EnvItem>, ExecutableEnvironmentRegistrationError> {
        self.environments.get(environment_id).await
    }

    /// Whether the Control projection marks an Agent unavailable for launch.
    #[must_use]
    pub fn deployment_agent_unavailable(&self, workspace_id: &str, agent_id: &str) -> bool {
        self.config_source
            .as_ref()
            .is_some_and(|source| source.agent_unavailable_in(workspace_id, agent_id))
    }

    /// The unavailable delegate that blocks one Deployment launch, if any.
    #[must_use]
    pub fn deployment_unavailable_delegate(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<String> {
        self.config_source
            .as_ref()
            .and_then(|source| source.unavailable_delegate_in(workspace_id, agent_id))
    }

    /// Resolve and validate the exact Environment snapshot frozen into a new Session.
    pub async fn resolve_session_environment(
        &self,
        requested_environment_id: Option<&str>,
        published_environment: Option<
            &awaken_executable_agent_contract::ExecutableAgentEnvironment,
        >,
        published_backend_ref: Option<&str>,
        mcp_targets: &[McpTarget],
    ) -> Result<ResolvedSessionEnvironment, RunError> {
        let environment_id = requested_environment_id
            .map(str::to_owned)
            .or_else(|| published_environment.map(|binding| binding.environment_id.clone()))
            .unwrap_or_else(|| "env_local".to_string());
        let resolved = match (requested_environment_id, published_environment) {
            (None, Some(binding)) => {
                self.environments
                    .resolve_exact_for_session(
                        &binding.environment_id,
                        binding.revision,
                        published_backend_ref,
                        mcp_targets,
                    )
                    .await
            }
            _ => {
                self.environments
                    .resolve_current_for_session(
                        &environment_id,
                        published_backend_ref,
                        mcp_targets,
                    )
                    .await
            }
        }
        .map_err(|error| RunError::unavailable(error.to_string()))?
        .ok_or_else(|| {
            RunError::bad_request(format!("environment `{environment_id}` is unavailable"))
        })?;
        validate_sandbox_provisioning_runtime(
            resolved.snapshot.sandbox_provisioning,
            published_backend_ref,
        )?;
        Ok(resolved)
    }

    /// Project one durable Session into its Environment WorkQueue when its
    /// frozen baseline requires external execution. This is the sole fast-path
    /// and recovery implementation; callers never enqueue independently.
    pub async fn dispatch_session_work(
        &self,
        session: &PersistedSession,
    ) -> Result<bool, awaken_session_contract::work_queue::WorkQueueError> {
        dispatch_session_work(self.environments.as_ref(), session).await
    }

    /// Restore every missing Session-to-WorkQueue projection from durable truth.
    pub async fn reconcile_work_dispatches(&self) -> WorkDispatchReconciliation {
        reconcile_work_dispatches(self.sessions_repo.as_ref(), self.environments.as_ref()).await
    }

    pub fn replace_repository(&mut self, repo: Arc<dyn ManagedSessionRepository>) {
        self.runtime.install_environment_binding_sink(Arc::new(
            RepositoryEnvironmentBindingSink::new(repo.clone()),
        ));
        self.sessions_repo = repo;
    }

    pub fn replace_environment_source(&mut self, source: Arc<dyn SessionEnvironmentSource>) {
        self.environments = source;
    }

    pub fn set_lifecycle_sink(&mut self, sink: Arc<dyn SessionLifecycleSink>) {
        self.lifecycle_sink = Some(sink);
    }

    pub fn set_resource_purge_scheduler(
        &mut self,
        scheduler: Arc<dyn awaken_resource_contract::ResourcePurgeScheduler>,
    ) {
        self.resource_purge_scheduler = Some(scheduler);
    }

    pub fn set_credential_source(&mut self, source: Arc<dyn SessionCredentialSource>) {
        self.credential_source = Some(source);
    }

    pub fn set_repository_credential_ingress(
        &mut self,
        ingress: Arc<dyn RepositoryCredentialIngress>,
    ) {
        self.repository_credential_ingress = Some(ingress);
    }

    pub fn set_config_source(
        &mut self,
        source: Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>,
    ) {
        self.config_source = Some(source);
    }

    pub fn set_resource_catalog(
        &mut self,
        catalog: Arc<dyn awaken_resource_contract::ResourceCatalog>,
    ) {
        self.resource_catalog = Some(catalog);
    }
}

fn validate_sandbox_provisioning_runtime(
    provisioning: SandboxProvisioning,
    runtime: Option<&str>,
) -> Result<(), RunError> {
    if provisioning == SandboxProvisioning::OnToolUse && !matches!(runtime, None | Some("awaken")) {
        return Err(RunError::bad_request(format!(
            "sandbox_provisioning_unsupported: `on_tool_use` requires the native awaken runtime, got `{}`",
            runtime.unwrap_or_default()
        )));
    }
    Ok(())
}

async fn dispatch_session_work(
    environments: &dyn SessionEnvironmentSource,
    session: &PersistedSession,
) -> Result<bool, awaken_session_contract::work_queue::WorkQueueError> {
    if !session.needs_work_dispatch() {
        return Ok(false);
    }
    let baseline = session
        .frozen_baseline()
        .expect("needs_work_dispatch requires a frozen baseline");
    environments
        .enqueue_session_work(&baseline.environment.environment_id, &session.session_id)
        .await?;
    Ok(true)
}

async fn reconcile_work_dispatches(
    sessions: &dyn ManagedSessionRepository,
    environments: &dyn SessionEnvironmentSource,
) -> WorkDispatchReconciliation {
    let mut report = WorkDispatchReconciliation::default();
    for scoped in sessions.reconcilable_sessions().await {
        let session = scoped.session;
        match dispatch_session_work(environments, &session).await {
            Ok(true) => report.settled += 1,
            Ok(false) => {}
            Err(error) => report.failures.push(WorkDispatchFailure {
                session_id: session.session_id.clone(),
                environment_id: session.environment_id().to_string(),
                message: error.to_string(),
            }),
        }
    }
    report
}

fn claim_once(fence: &AtomicBool) -> bool {
    !fence.swap(true, Ordering::AcqRel)
}

/// Repository-backed implementation installed at the runtime materialization boundary.
pub struct RepositoryEnvironmentBindingSink {
    repo: Arc<dyn ManagedSessionRepository>,
}

impl RepositoryEnvironmentBindingSink {
    #[must_use]
    pub fn new(repo: Arc<dyn ManagedSessionRepository>) -> Self {
        Self { repo }
    }
}

#[async_trait::async_trait]
impl SessionEnvironmentBindingSink for RepositoryEnvironmentBindingSink {
    async fn owns(&self, session_id: &str) -> bool {
        self.repo.owner(session_id).await.is_some()
    }

    async fn persist(
        &self,
        session_id: &str,
        binding: &str,
        realization: Option<&awaken_session_contract::SessionRealizationLease>,
    ) -> Result<(), RunError> {
        const CAS_ATTEMPTS: usize = 3;
        for attempt in 0..CAS_ATTEMPTS {
            let owner = self.repo.owner(session_id).await.ok_or_else(|| {
                RunError::internal(format!("Session `{session_id}` is not durable"))
            })?;
            let mut session = self.repo.get(session_id).await.ok_or_else(|| {
                RunError::internal(format!("Session `{session_id}` is not durable"))
            })?;
            let now_unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
                .unwrap_or_default();
            let realization_is_current = match (session.realization.as_ref(), realization) {
                (Some(current), Some(asserted)) => {
                    current == asserted
                        && awaken_session_contract::realization_lease_is_live_at(
                            current.expires_at_unix_ms,
                            now_unix_ms,
                        )
                }
                (None, None) => true,
                _ => false,
            };
            if !realization_is_current {
                return Err(RunError::classified(
                    "session_realization_stale",
                    "Session environment binding was fenced by another realization owner",
                ));
            }
            if session.environment.binding() == Some(binding) {
                return Ok(());
            }
            session.environment.set_resident(binding);
            let expected_revision = session.revision;
            let payload = awaken_session_contract::SessionMutationPayload::Replace(session);
            let payload_hash = payload.stable_hash();
            let mutation = awaken_session_contract::SessionMutation {
                expected_revision,
                idempotency: awaken_session_contract::IdempotencyRecord {
                    key: format!(
                        "session:bind-environment:{session_id}:{}:{payload_hash}",
                        expected_revision.0
                    ),
                    payload_hash,
                },
                payload,
                lifecycle_facts: Vec::new(),
            };
            match self
                .repo
                .commit_mutation(&owner, mutation)
                .await
                .map_err(|error| RunError::internal(error.to_string()))?
            {
                awaken_session_contract::SessionMutationResult::Applied { .. }
                | awaken_session_contract::SessionMutationResult::Replayed { .. } => return Ok(()),
                awaken_session_contract::SessionMutationResult::Conflict { .. }
                    if attempt + 1 < CAS_ATTEMPTS => {}
                awaken_session_contract::SessionMutationResult::Conflict { .. } => {
                    return Err(RunError::internal(
                        "Session environment binding CAS exhausted",
                    ));
                }
                awaken_session_contract::SessionMutationResult::IdempotencyMismatch => {
                    return Err(RunError::internal(
                        "Session environment binding idempotency mismatch",
                    ));
                }
            }
        }
        Err(RunError::internal(
            "Session environment binding CAS exhausted",
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    use super::*;

    struct NoopRuntime;

    #[async_trait::async_trait]
    impl SessionRuntime for NoopRuntime {
        async fn run(
            &self,
            _agent: &str,
            _thread: &str,
            _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
        ) -> Result<awaken_session_contract::StepOutcome, RunError> {
            Err(RunError::internal("unused test runtime"))
        }

        async fn resume(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _decision: awaken_session_contract::ToolPermissionDecision,
        ) -> Result<awaken_session_contract::StepOutcome, RunError> {
            Err(RunError::internal("unused test runtime"))
        }

        async fn resume_custom(
            &self,
            _thread: &str,
            _tool_use_id: &str,
            _content: Vec<awaken_agent_contract::agent::content::ContentBlock>,
            _is_error: bool,
        ) -> Result<awaken_session_contract::StepOutcome, RunError> {
            Err(RunError::internal("unused test runtime"))
        }

        async fn add_system(&self, _thread: &str, _text: &str) -> Result<(), RunError> {
            Err(RunError::internal("unused test runtime"))
        }

        async fn define_outcome(
            &self,
            _thread: &str,
            _description: &str,
            _rubric: &str,
            _max_iterations: u32,
        ) -> Result<awaken_session_contract::OutcomeReport, RunError> {
            Err(RunError::internal("unused test runtime"))
        }

        fn model(&self) -> String {
            "unused".into()
        }
    }

    struct NoopMcpRealizer;

    #[async_trait::async_trait]
    impl McpAttachmentRealizer for NoopMcpRealizer {}

    #[derive(Default)]
    struct RecordingEnvironmentSource {
        dispatched: Mutex<BTreeSet<String>>,
        failures: Mutex<BTreeSet<String>>,
    }

    impl RecordingEnvironmentSource {
        fn fail_for(&self, session_id: &str) {
            self.failures.lock().unwrap().insert(session_id.into());
        }
    }

    #[async_trait::async_trait]
    impl SessionEnvironmentSource for RecordingEnvironmentSource {
        async fn get(
            &self,
            _environment_id: &str,
        ) -> Result<Option<EnvItem>, ExecutableEnvironmentRegistrationError> {
            unreachable!("work dispatch does not reopen the Environment catalog")
        }

        async fn resolve_current_for_session(
            &self,
            _environment_id: &str,
            _runtime: Option<&str>,
            _mcp_targets: &[McpTarget],
        ) -> Result<Option<ResolvedSessionEnvironment>, EnvironmentImageBuildError> {
            unreachable!("work dispatch consumes the frozen Session baseline")
        }

        async fn resolve_exact_for_session(
            &self,
            _environment_id: &str,
            _revision: u64,
            _runtime: Option<&str>,
            _mcp_targets: &[McpTarget],
        ) -> Result<Option<ResolvedSessionEnvironment>, EnvironmentImageBuildError> {
            unreachable!("work dispatch consumes the frozen Session baseline")
        }

        async fn enqueue_session_work(
            &self,
            _environment_id: &str,
            session_id: &str,
        ) -> Result<String, awaken_session_contract::work_queue::WorkQueueError> {
            if self.failures.lock().unwrap().contains(session_id) {
                return Err(
                    awaken_session_contract::work_queue::WorkQueueError::Storage(format!(
                        "injected failure for {session_id}"
                    )),
                );
            }
            self.dispatched
                .lock()
                .unwrap()
                .insert(session_id.to_string());
            Ok(format!("work:{session_id}"))
        }
    }

    fn persisted(id: &str, self_hosted: bool, application: bool, status: &str) -> PersistedSession {
        let environment = awaken_session_contract::EnvironmentSnapshot {
            environment_id: "env-worker".into(),
            revision: awaken_environment_contract::EnvironmentRevision(7),
            self_hosted,
            config_fingerprint: awaken_session_contract::EnvironmentFingerprint("env-7".into()),
            sandbox: serde_json::json!({}),
            sandbox_provisioning: Default::default(),
            packages: Default::default(),
            prepared_image: None,
            network: awaken_session_contract::SessionNetworkPolicy::Unrestricted,
            credential_realization:
                awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
        };
        PersistedSession {
            session_id: id.into(),
            revision: Default::default(),
            baseline: awaken_session_contract::SessionBaselineState::Frozen(
                awaken_session_contract::SessionBaseline::compile(
                    awaken_session_contract::SessionBaselineInputs {
                        environment,
                        runtime_placement: SessionRuntimePlacement::Local,
                        mcp_authoring: Default::default(),
                        agent_id: "agent".into(),
                        model: "model".into(),
                        runtime: None,
                        application: application.then(|| {
                            awaken_session_contract::ApplicationContributionReceipt {
                                plan_fingerprint: "plan".into(),
                                input_fingerprint: "input".into(),
                            }
                        }),
                        delegate_ids: Vec::new(),
                        toolsets: Vec::new(),
                        mounts: Vec::new(),
                        env: Vec::new(),
                        prompts: Vec::new(),
                    },
                ),
            ),
            title: None,
            metadata: Default::default(),
            tools: Default::default(),
            activity_epoch: 0,
            environment: Default::default(),
            mcp: Default::default(),
            resources: Default::default(),
            realization: None,
            status: status.into(),
            archived_at: None,
        }
    }

    async fn create(repo: &dyn ManagedSessionRepository, session: PersistedSession) {
        let id = session.session_id.clone();
        repo.create(
            "workspace",
            session,
            awaken_session_contract::IdempotencyRecord {
                key: format!("create:{id}"),
                payload_hash: format!("payload:{id}"),
            },
            Vec::new(),
        )
        .await
        .expect("persist fixture");
    }

    fn application(
        repo: Arc<dyn ManagedSessionRepository>,
        environments: Arc<dyn SessionEnvironmentSource>,
    ) -> SessionApplication {
        application_with_configuration(
            repo,
            environments,
            SessionApplicationConfiguration::default(),
        )
    }

    fn application_with_configuration(
        repo: Arc<dyn ManagedSessionRepository>,
        environments: Arc<dyn SessionEnvironmentSource>,
        configuration: SessionApplicationConfiguration,
    ) -> SessionApplication {
        SessionApplication::new_with_configuration(
            Arc::new(NoopRuntime),
            Arc::new(NoopMcpRealizer),
            repo,
            environments,
            configuration,
        )
    }

    /// Cause/effect graph: C1 a baseline is frozen; C2 its explicit Runtime
    /// placement is Local or Worker; C3 a retained pre-placement row is marked
    /// LegacyUnspecified; C4 the process composition is local or registered;
    /// C5 an application contribution independently requires Worker custody.
    /// The realization lease is intentionally absent from the causes: it is an
    /// assignment fence, never placement policy. Effects are E1 local physical
    /// realization or E2 dispatch-only Coordinator projection.
    ///
    /// | Rule | Frozen placement | Process placement | Application | Effect |
    /// |---|---|---|---|---|
    /// | P1 | preparing | any | n/a | E1 (not yet realizable) |
    /// | P2 | local | local/registered | absent | E1 |
    /// | P3 | worker | local/registered | absent | E2 |
    /// | P4 | local/worker | any | present | E2 |
    /// | P5 | legacy | local | absent | E1 |
    /// | P6 | legacy | registered | absent | E2 |
    ///
    /// P5/P6 are the one-way upgrade interpretation for rows serialized before
    /// placement existed. New creation is separately asserted never to emit the
    /// legacy value.
    #[test]
    fn realization_owner_follows_the_application_placement_decision_table() {
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("session repository"),
        );
        let environments = Arc::new(RecordingEnvironmentSource::default());
        let local = application_with_configuration(
            repo.clone(),
            environments.clone(),
            SessionApplicationConfiguration {
                execution_placement: SessionExecutionPlacement::LocalWorker,
            },
        );
        let registered = application_with_configuration(
            repo,
            environments,
            SessionApplicationConfiguration {
                execution_placement: SessionExecutionPlacement::RegisteredWorker,
            },
        );

        let frozen = |placement: SessionRuntimePlacement, application: bool| {
            let mut value = persisted("placement", false, false, "idle");
            let awaken_session_contract::SessionBaselineState::Frozen(baseline) =
                &mut value.baseline
            else {
                unreachable!("fixture is frozen")
            };
            baseline.runtime_placement = placement;
            baseline.application =
                application.then(|| awaken_session_contract::ApplicationContributionReceipt {
                    plan_fingerprint: "plan".into(),
                    input_fingerprint: "input".into(),
                });
            value
        };
        let preparing = {
            let mut value = frozen(SessionRuntimePlacement::Local, false);
            let baseline = value.frozen_baseline().expect("frozen").clone();
            value.baseline = awaken_session_contract::SessionBaselineState::Preparing(
                awaken_session_contract::SessionCreationIntent {
                    control: awaken_session_contract::ControlSessionCreationInputs {
                        environment: baseline.environment,
                        runtime_placement: SessionRuntimePlacement::Local,
                        agent_id: baseline.agent_id,
                        model: baseline.model,
                        execution_model_ref: baseline.execution_model_ref,
                        runtime: baseline.runtime,
                        mcp_authoring: baseline.mcp_authoring,
                        delegate_ids: baseline.delegate_ids,
                        toolsets: baseline.toolsets,
                        mounts: baseline.mounts,
                        env: baseline.env,
                        prompts: baseline.prompts,
                        resources: Default::default(),
                        initial_mcp: Vec::new(),
                    },
                    application: awaken_session_contract::ApplicationContributionState::Absent,
                },
            );
            value
        };

        for (rule, value, local_expected, registered_expected) in [
            ("P1", preparing, false, false),
            (
                "P2",
                frozen(SessionRuntimePlacement::Local, false),
                false,
                false,
            ),
            (
                "P3",
                frozen(SessionRuntimePlacement::Worker, false),
                true,
                true,
            ),
            (
                "P4a",
                frozen(SessionRuntimePlacement::Local, true),
                true,
                true,
            ),
            (
                "P4b",
                frozen(SessionRuntimePlacement::Worker, true),
                true,
                true,
            ),
            (
                "P5/P6",
                frozen(SessionRuntimePlacement::LegacyUnspecified, false),
                false,
                true,
            ),
        ] {
            assert_eq!(
                local.requires_external_realization(&value),
                local_expected,
                "{rule}/local"
            );
            assert_eq!(
                registered.requires_external_realization(&value),
                registered_expected,
                "{rule}/registered"
            );
        }
        assert_eq!(local.runtime_placement(), SessionRuntimePlacement::Local);
        assert_eq!(
            registered.runtime_placement(),
            SessionRuntimePlacement::Worker
        );
    }

    #[tokio::test]
    async fn root_mutation_cause_effect_decision_table() {
        // Cause-effect graph: C1 expected revision is current; C2 idempotency key
        // and payload hash replay exactly; C3 expected revision is stale; C4 an
        // existing key is reused with another hash. Effects: E1 apply once and
        // advance revision; E2 replay current truth without another advance; E3
        // conflict; E4 idempotency mismatch. No interface adapter owns a second
        // CAS or retry algorithm.
        //
        // | Rule | C1 | C2 | C3 | C4 | Effect |
        // |---|---|---|---|---|---|
        // | M1 | yes | no | no | no | E1 applied |
        // | M2 | no | yes | no | no | E2 replayed |
        // | M3 | no | no | yes | no | E3 conflict |
        // | M4 | no | no | no | yes | E4 mismatch |
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("session repository"),
        );
        create(repo.as_ref(), persisted("mutation", false, false, "idle")).await;
        let app = application(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let original = repo.get("mutation").await.expect("fixture");
        let mut candidate = original.clone();
        candidate.title = Some("applied".into());
        let payload = awaken_session_contract::SessionMutationPayload::Replace(candidate.clone());
        let record = awaken_session_contract::IdempotencyRecord {
            key: "mutation:one".into(),
            payload_hash: payload.stable_hash(),
        };

        let (applied, changed) = app
            .commit_session_snapshot_with_record(
                "workspace",
                candidate.clone(),
                record.clone(),
                Vec::new(),
            )
            .await
            .expect("M1");
        assert!(changed, "M1");
        assert!(applied.revision > original.revision, "M1");

        let (replayed, changed) = app
            .commit_session_snapshot_with_record(
                "workspace",
                candidate.clone(),
                record.clone(),
                Vec::new(),
            )
            .await
            .expect("M2");
        assert!(!changed, "M2");
        assert_eq!(replayed.revision, applied.revision, "M2");

        let stale = app
            .commit_session_snapshot("workspace", candidate.clone(), "stale", Vec::new())
            .await;
        assert_eq!(stale, Err(SessionMutationError::Conflict), "M3");

        let mismatch = app
            .commit_session_snapshot_with_record(
                "workspace",
                candidate,
                awaken_session_contract::IdempotencyRecord {
                    key: record.key,
                    payload_hash: "another-payload".into(),
                },
                Vec::new(),
            )
            .await;
        assert_eq!(
            mismatch,
            Err(SessionMutationError::IdempotencyMismatch),
            "M4"
        );
    }

    /// Activity-fence FMECA cause/effect graph. Causes: C1 the Session exists;
    /// C2 it is nonterminal; C3 the epoch can advance; C4 settlement presents
    /// the current epoch; C5 a later admission or terminal transition has
    /// fenced that settlement. Effects: E1 admission commits `running` with one
    /// unique monotonic epoch; E2 only the current completion commits `idle`;
    /// E3 stale/terminal completions are no-ops; E4 missing, terminal-admission,
    /// and exhausted-epoch failures do not mutate durable truth.
    ///
    /// | Rule | Exists | Terminal | Epoch available | Current settle | Fence | Effect |
    /// |---|---|---|---|---|---|---|
    /// | A1 | yes | no | yes | n/a | concurrent admit | E1, distinct epochs |
    /// | A2 | yes | no | n/a | no | newer epoch | E3, remains running |
    /// | A3 | yes | no | n/a | yes | none | E2, idle |
    /// | A4 | yes | yes | n/a | any | terminal | E3, terminal preserved |
    /// | A5 | yes | yes | any | n/a | n/a | E4, reject admission |
    /// | A6 | yes | no | no | n/a | n/a | E4, reject exhaustion |
    /// | A7 | no | n/a | n/a | n/a | n/a | E4, not found |
    #[tokio::test]
    async fn activity_fence_decision_table_preserves_monotonic_and_terminal_truth() {
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("session repository"),
        );
        create(repo.as_ref(), persisted("activity", false, false, "idle")).await;
        let app = application(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );

        let (first, second) = tokio::join!(
            app.begin_activity("activity"),
            app.begin_activity("activity")
        );
        let first = first.expect("A1 first admission");
        let second = second.expect("A1 concurrent admission");
        let mut epochs = [first.activity_epoch, second.activity_epoch];
        epochs.sort_unstable();
        assert_eq!(epochs, [1, 2], "A1");
        let active = repo.get("activity").await.expect("A1 durable Session");
        assert_eq!(active.status, "running", "A1");

        let stale = app
            .settle_activity("activity", epochs[0])
            .await
            .expect("A2 stale settlement");
        assert_eq!(stale.status, "running", "A2");
        assert_eq!(stale.activity_epoch, epochs[1], "A2");

        let idle = app
            .settle_activity("activity", epochs[1])
            .await
            .expect("A3 current settlement");
        assert_eq!(idle.status, "idle", "A3");

        let running = app
            .begin_activity("activity")
            .await
            .expect("A4 activity before terminal transition");
        let mut terminated = running.clone();
        terminated.status = "terminated".into();
        let terminated = app
            .commit_session_snapshot(
                "workspace",
                terminated,
                "activity-test-terminal",
                Vec::new(),
            )
            .await
            .expect("A4 terminal transition");
        let fenced = app
            .settle_activity("activity", running.activity_epoch)
            .await
            .expect("A4 terminal settlement is idempotent");
        assert_eq!(fenced, terminated, "A4");
        assert_eq!(
            app.begin_activity("activity").await,
            Err(SessionActivityError::Terminal),
            "A5"
        );

        let mut exhausted = persisted("activity-exhausted", false, false, "idle");
        exhausted.activity_epoch = u64::MAX;
        create(repo.as_ref(), exhausted).await;
        let exhausted_before = repo
            .get("activity-exhausted")
            .await
            .expect("A6 durable Session before admission");
        assert_eq!(
            app.begin_activity("activity-exhausted").await,
            Err(SessionActivityError::EpochExhausted),
            "A6"
        );
        assert_eq!(
            repo.get("activity-exhausted")
                .await
                .expect("A6 durable Session"),
            exhausted_before,
            "A6"
        );
        assert_eq!(
            app.begin_activity("missing").await,
            Err(SessionActivityError::NotFound),
            "A7"
        );
    }

    /// Cause/effect graph: C1 the Runtime presents the exact durable realization
    /// lease; C2 the binding is new or an idempotent replay; C3 a replacement
    /// owner/epoch has fenced the Runtime. C1 permits the ordinary root CAS; C3
    /// rejects before even an equal binding can be treated as a replay. This
    /// prevents a stale sandbox owner from publishing after lease replacement.
    ///
    /// | Rule | Asserted lease | Binding | Effect |
    /// |---|---|---|---|
    /// | B1 | exact | new | persist once |
    /// | B2 | exact | equal | idempotent success |
    /// | B3 | stale owner/epoch | new/equal | fenced, no mutation |
    /// | B4 | aggregate/assertion both absent | new/equal | legacy CAS path |
    /// | B5 | exact but expired | new/equal | fenced, no mutation |
    #[tokio::test]
    async fn environment_binding_persistence_is_fenced_by_exact_realization() {
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
                .expect("session repository"),
        );
        let mut session = persisted("binding-fence", false, false, "idle");
        let current = awaken_session_contract::SessionRealizationLease {
            owner: "runtime-a".into(),
            runtime_incarnation: "runtime-a/boot-1".into(),
            epoch: 3,
            expires_at_unix_ms: u64::MAX,
        };
        session.realization = Some(current.clone());
        create(repo.as_ref(), session).await;
        let sink = RepositoryEnvironmentBindingSink::new(repo.clone());

        sink.persist("binding-fence", "sandbox-a", Some(&current))
            .await
            .expect("B1");
        sink.persist("binding-fence", "sandbox-a", Some(&current))
            .await
            .expect("B2");
        create(
            repo.as_ref(),
            persisted("binding-unassigned", false, false, "idle"),
        )
        .await;
        sink.persist("binding-unassigned", "sandbox-legacy", None)
            .await
            .expect("B4");
        let stale = awaken_session_contract::SessionRealizationLease {
            owner: "runtime-b".into(),
            runtime_incarnation: "runtime-b/boot-1".into(),
            epoch: 4,
            expires_at_unix_ms: u64::MAX,
        };
        let error = sink
            .persist("binding-fence", "sandbox-a", Some(&stale))
            .await
            .expect_err("B3 stale replay is fenced");
        assert_eq!(error.code, "session_realization_stale", "B3");
        let expired = awaken_session_contract::SessionRealizationLease {
            expires_at_unix_ms: 0,
            ..current.clone()
        };
        let mut expired_session = repo.get("binding-fence").await.expect("B5 fixture");
        expired_session.realization = Some(expired.clone());
        let expected_revision = expired_session.revision;
        let payload = awaken_session_contract::SessionMutationPayload::Replace(expired_session);
        let payload_hash = payload.stable_hash();
        assert!(matches!(
            repo.commit_mutation(
                "workspace",
                awaken_session_contract::SessionMutation {
                    expected_revision,
                    idempotency: awaken_session_contract::IdempotencyRecord {
                        key: "binding-fence:expire".into(),
                        payload_hash,
                    },
                    payload,
                    lifecycle_facts: Vec::new(),
                },
            )
            .await
            .expect("B5 fixture mutation"),
            awaken_session_contract::SessionMutationResult::Applied { .. }
        ));
        assert_eq!(
            sink.persist("binding-fence", "sandbox-a", Some(&expired))
                .await
                .expect_err("B5")
                .code,
            "session_realization_stale",
            "B5"
        );
        assert_eq!(
            repo.get("binding-fence")
                .await
                .and_then(|session| session.environment.binding().map(str::to_owned))
                .as_deref(),
            Some("sandbox-a"),
            "B3"
        );
    }

    #[test]
    fn lifecycle_supervisor_claim_is_one_shot() {
        // Cause/effect decision table: C1=unclaimed fence, C2=already claimed.
        // R1 C1 -> E1 first caller becomes owner; R2 C2 -> E2 every later caller
        // is rejected. This proves moving the fence out of the protocol adapter
        // cannot start parallel Session lifecycle supervisors.
        let fence = AtomicBool::new(false);
        assert!(claim_once(&fence), "R1");
        assert!(!claim_once(&fence), "R2");
    }

    #[test]
    fn native_only_lazy_provisioning_decision_table() {
        // Causes: C1 eager policy; C2 lazy policy; C3 implicit/native runtime;
        // C4 ACP/unknown runtime. Effects: E1 accept; E2 reject before Session
        // realization. Environment policy tests own disabled/exact-version rules.
        //
        // | Rule | policy | runtime | effect |
        // | R1 | eager | any | accept |
        // | R2 | lazy | implicit/native | accept |
        // | R3 | lazy | ACP/unknown | reject |
        use SandboxProvisioning::{Eager, OnToolUse};
        for (case, provisioning, runtime, accepted) in [
            ("R1 eager native", Eager, None, true),
            ("R1 eager ACP", Eager, Some("acp:claude"), true),
            ("R2 lazy implicit native", OnToolUse, None, true),
            ("R2 lazy explicit native", OnToolUse, Some("awaken"), true),
            ("R3 lazy ACP", OnToolUse, Some("acp:claude"), false),
            ("R3 lazy unknown runtime", OnToolUse, Some("remote"), false),
        ] {
            assert_eq!(
                validate_sandbox_provisioning_runtime(provisioning, runtime).is_ok(),
                accepted,
                "{case}"
            );
        }
    }

    #[tokio::test]
    async fn durable_session_truth_owns_one_work_projection_path() {
        // Cause/effect graph: C1 frozen Environment is self-hosted; C2 Session
        // has no Application-owned execution; C3 Session is nonterminal; C4 the
        // projection command is replayed. Effects: E1 only C1+C2+C3 dispatches;
        // E2 replay uses the same idempotent port and creates no second identity.
        //
        // | Rule | self-hosted | application | terminal | replay | effect |
        // | R1 | yes | no | no | no | project one |
        // | R2 | yes | no | no | yes | retain one |
        // | R3 | no | no | no | any | skip |
        // | R4 | yes | yes | no | any | skip |
        // | R5 | yes | no | yes | any | skip |
        let repo = awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository");
        create(&repo, persisted("external", true, false, "idle")).await;
        create(&repo, persisted("local", false, false, "idle")).await;
        create(&repo, persisted("application", true, true, "idle")).await;
        create(&repo, persisted("terminal", true, false, "terminated")).await;
        let environments = RecordingEnvironmentSource::default();

        let first = reconcile_work_dispatches(&repo, &environments).await;
        assert_eq!(first.settled, 1, "R1/R3/R4/R5");
        assert!(first.failures.is_empty());
        assert_eq!(environments.dispatched.lock().unwrap().len(), 1, "R1");

        let replay = reconcile_work_dispatches(&repo, &environments).await;
        assert_eq!(replay.settled, 1, "R2");
        assert!(replay.failures.is_empty());
        assert_eq!(environments.dispatched.lock().unwrap().len(), 1, "R2");
    }

    #[tokio::test]
    async fn work_dispatch_reconciliation_isolates_each_session_failure() {
        // Work-projection FMECA decision table. Causes: C1 a durable Session needs
        // external work; C2 its queue write succeeds; C3 a sibling queue write
        // fails; C4 an unrelated Session needs no dispatch. Effects: E1 every
        // eligible Session is attempted; E2 successes settle independently; E3
        // failures retain exact Session/Environment diagnostics for later retry;
        // E4 ineligible Sessions cause no side effect. Rules: W1 C1+C2=>E1+E2;
        // W2 C1+C3=>E1+E3 without aborting W1; W3 C4=>E4.
        let repo = awaken_session_store::SqliteManagedSessionRepository::open_in_memory()
            .expect("session repository");
        create(&repo, persisted("external-failed", true, false, "idle")).await;
        create(&repo, persisted("external-settled", true, false, "idle")).await;
        create(&repo, persisted("local-skip", false, false, "idle")).await;
        let environments = RecordingEnvironmentSource::default();
        environments.fail_for("external-failed");

        let report = reconcile_work_dispatches(&repo, &environments).await;
        assert_eq!(report.settled, 1, "W1/W2");
        assert_eq!(
            environments
                .dispatched
                .lock()
                .unwrap()
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            ["external-settled".to_string()],
            "W1/W3"
        );
        assert_eq!(report.failures.len(), 1, "W2");
        assert_eq!(report.failures[0].session_id, "external-failed", "W2");
        assert_eq!(report.failures[0].environment_id, "env-worker", "W2");
        assert!(
            report.failures[0].message.contains("injected failure"),
            "W2 preserves the retry diagnostic"
        );
    }
}
