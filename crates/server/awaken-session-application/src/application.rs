/// Process-level execution topology interpreted by the Session application.
/// The selected value is frozen into every newly admitted Session; protocol
/// adapters never inspect or override it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SessionExecutionPlacement {
    #[default]
    LocalWorker,
    RegisteredWorker,
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
    pub quarantined: Vec<awaken_session_contract::SessionRecoveryQuarantine>,
    pub pending: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionApplicationConfiguration {
    pub execution_placement: SessionExecutionPlacement,
    /// Stable logical owner for the co-located Runtime. The process-unique
    /// incarnation remains separate so a restart can fence its predecessor
    /// without impersonating a different owner.
    pub local_realization_owner: String,
    /// Maximum time a synchronous ordinary create may wait for its exact Worker
    /// realization acknowledgement. Application-contribution creates are inert
    /// and intentionally do not cross this barrier.
    pub create_readiness_timeout: std::time::Duration,
    /// Poll cadence for the repository-backed readiness barrier. The aggregate
    /// remains the authority; this interval is only wake-up policy.
    pub create_readiness_poll_interval: std::time::Duration,
    /// Maximum fenced assignments allowed for initial Environment
    /// realization before the aggregate enters `activation_failed`.
    pub realization_retry_budget: u32,
}

impl Default for SessionApplicationConfiguration {
    fn default() -> Self {
        Self {
            execution_placement: SessionExecutionPlacement::default(),
            local_realization_owner: "local-runtime".into(),
            create_readiness_timeout: std::time::Duration::from_secs(120),
            create_readiness_poll_interval: std::time::Duration::from_millis(25),
            realization_retry_budget: 3,
        }
    }
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
    resource_references: Option<Arc<dyn awaken_resource_contract::ResourceReferenceIndex>>,
    resource_files: Option<Arc<dyn awaken_resource_contract::FileCatalog>>,
    sessions_repo: Arc<dyn ManagedSessionRepository>,
    lifecycle_sink: Option<Arc<dyn SessionLifecycleFactSink>>,
    local_realization_owner: String,
    runtime_incarnation: String,
    lifecycle_supervisor_started: AtomicBool,
}

static APPLICATION_INCARNATION_SEQ: AtomicU64 = AtomicU64::new(0);

impl SessionApplication {
    pub(crate) fn create_readiness_timeout(&self) -> std::time::Duration {
        self.configuration.create_readiness_timeout
    }

    pub(crate) fn create_readiness_poll_interval(&self) -> std::time::Duration {
        self.configuration.create_readiness_poll_interval
    }

    pub(crate) fn realization_retry_budget(&self) -> u32 {
        self.configuration.realization_retry_budget.max(1)
    }

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
        let local_realization_owner = configuration.local_realization_owner.trim().to_string();
        assert!(
            !local_realization_owner.is_empty(),
            "local Session realization owner must not be empty"
        );
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
            resource_references: None,
            resource_files: None,
            sessions_repo,
            lifecycle_sink: None,
            local_realization_owner,
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
    pub(crate) fn runtime(&self) -> &dyn SessionRuntime {
        self.runtime.as_ref()
    }

    #[must_use]
    pub(crate) fn mcp_realizer(&self) -> &dyn McpAttachmentRealizer {
        self.mcp_realizer.as_ref()
    }

    #[must_use]
    pub(crate) fn credential_source(&self) -> Option<&dyn SessionCredentialSource> {
        self.credential_source.as_deref()
    }

    #[must_use]
    pub(crate) fn repository_credential_ingress(&self) -> Option<&dyn RepositoryCredentialIngress> {
        self.repository_credential_ingress.as_deref()
    }

    #[must_use]
    pub(crate) fn resource_catalog(
        &self,
    ) -> Option<&dyn awaken_resource_contract::ResourceCatalog> {
        self.resource_catalog.as_deref()
    }

    #[must_use]
    pub(crate) fn resource_purge_scheduler(
        &self,
    ) -> Option<&dyn awaken_resource_contract::ResourcePurgeScheduler> {
        self.resource_purge_scheduler.as_deref()
    }

    #[must_use]
    pub(crate) fn session_repository(&self) -> &dyn ManagedSessionRepository {
        self.sessions_repo.as_ref()
    }

    #[must_use]
    pub fn runtime_incarnation(&self) -> &str {
        &self.runtime_incarnation
    }

    /// Stable logical identity paired with the process-unique Runtime
    /// incarnation in Session realization leases.
    #[must_use]
    pub fn local_realization_owner(&self) -> &str {
        &self.local_realization_owner
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

    pub fn set_lifecycle_sink(&mut self, sink: Arc<dyn SessionLifecycleFactSink>) {
        self.lifecycle_sink = Some(sink);
    }

    pub fn set_resource_purge_scheduler(
        &mut self,
        scheduler: Arc<dyn awaken_resource_contract::ResourcePurgeScheduler>,
    ) {
        self.resource_purge_scheduler = Some(scheduler);
    }

    /// Install the Resource authority ports used to publish the Session's
    /// durable active+pending retention set. These ports are paired so logical
    /// File ids and physical blob identities can never be sourced from
    /// different Resource components.
    pub fn set_resource_reference_authority(
        &mut self,
        references: Arc<dyn awaken_resource_contract::ResourceReferenceIndex>,
        files: Arc<dyn awaken_resource_contract::FileCatalog>,
    ) {
        self.resource_references = Some(references);
        self.resource_files = Some(files);
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
    let native = runtime.is_none_or(|backend_ref| {
        matches!(
            awaken_runtime_contract::resolved::Backend::from_ref(backend_ref),
            awaken_runtime_contract::resolved::Backend::Native
        )
    });
    if provisioning == SandboxProvisioning::OnToolUse && !native {
        return Err(RunError::bad_request(format!(
            "sandbox_provisioning_unsupported: `on_tool_use` requires a native backend, got `{}`",
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
    let sessions = match sessions.reconcilable_sessions().await {
        Ok(sessions) => sessions,
        Err(error) => {
            report.failures.push(WorkDispatchFailure {
                session_id: "<repository>".to_string(),
                environment_id: String::new(),
                message: error.to_string(),
            });
            return report;
        }
    };
    report.pending = sessions.sessions.len();
    report.quarantined.clone_from(&sessions.quarantined);
    for scoped in sessions.sessions {
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
    async fn owns(&self, session_id: &str) -> Result<bool, RunError> {
        match self.repo.owner(session_id).await {
            Ok(_) => Ok(true),
            Err(awaken_session_contract::SessionRepositoryError::NotFound) => Ok(false),
            Err(error) => Err(RunError::internal(error.to_string())),
        }
    }

    async fn persist(
        &self,
        receipt: awaken_session_contract::SessionEnvironmentReceipt,
    ) -> Result<(), RunError> {
        receipt
            .verify()
            .map_err(|error| RunError::internal(error.to_string()))?;
        let session_id = receipt.session_id.as_str();
        let binding = receipt.binding.as_str();
        const CAS_ATTEMPTS: usize = 3;
        for attempt in 0..CAS_ATTEMPTS {
            let owner = self
                .repo
                .owner(session_id)
                .await
                .map_err(|error| RunError::internal(error.to_string()))?;
            let mut session = self
                .repo
                .get(session_id)
                .await
                .map_err(|error| RunError::internal(error.to_string()))?;
            let now_unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
                .unwrap_or_default();
            let realization_is_current =
                match (session.realization.as_ref(), receipt.realization.as_ref()) {
                    (Some(current), Some(asserted)) => {
                        awaken_session_contract::realization_lease_authorizes(
                            current,
                            asserted,
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
            // Even an equal-binding replay must prove the current owner. Once
            // authorized, an exact receipt is a no-write replay; an adoption of
            // the same substrate under a newer lease commits the newer evidence.
            if session.environment.binding() == Some(binding)
                && session.environment.effect_id() == Some(receipt.effect_id.as_str())
            {
                return Ok(());
            }
            session.environment.apply_receipt(&receipt);
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
