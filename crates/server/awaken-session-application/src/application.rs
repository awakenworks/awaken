/// Process-level execution topology interpreted by the Session application.
/// The selected value is frozen into every newly admitted Session; protocol
/// adapters never inspect or override it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SessionExecutionPlacement {
    #[default]
    LocalWorker,
    RegisteredWorker,
}

/// Freeze process topology into the only two executable Session placements.
#[must_use]
pub const fn runtime_placement_for_topology(
    topology: SessionExecutionPlacement,
) -> SessionRuntimePlacement {
    match topology {
        SessionExecutionPlacement::LocalWorker => SessionRuntimePlacement::Local,
        SessionExecutionPlacement::RegisteredWorker => SessionRuntimePlacement::Worker,
    }
}

/// Select the one physical Environment-realization owner from the frozen fact.
#[must_use]
pub const fn environment_requires_external_owner(frozen: SessionRuntimePlacement) -> bool {
    match frozen {
        SessionRuntimePlacement::Local => false,
        SessionRuntimePlacement::Worker => true,
    }
}

#[cfg(kani)]
mod environment_owner_kani_proof {
    use super::{
        SessionExecutionPlacement, SessionRuntimePlacement, environment_requires_external_owner,
        runtime_placement_for_topology,
    };

    #[kani::proof]
    fn environment_owner_topology_is_exact_and_frozen() {
        let registered: bool = kani::any();
        let topology = if registered {
            SessionExecutionPlacement::RegisteredWorker
        } else {
            SessionExecutionPlacement::LocalWorker
        };

        assert_eq!(
            runtime_placement_for_topology(topology),
            if registered {
                SessionRuntimePlacement::Worker
            } else {
                SessionRuntimePlacement::Local
            }
        );
        assert!(!environment_requires_external_owner(
            SessionRuntimePlacement::Local
        ));
        assert!(environment_requires_external_owner(
            SessionRuntimePlacement::Worker
        ));
    }
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
    /// Maximum fenced assignments allowed for initial Environment
    /// realization before the aggregate enters `activation_failed`.
    pub realization_retry_budget: u32,
}

impl Default for SessionApplicationConfiguration {
    fn default() -> Self {
        Self {
            execution_placement: SessionExecutionPlacement::default(),
            local_realization_owner: "local-runtime".into(),
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
    model_publication_resolver:
        Option<Arc<dyn awaken_session_contract::SessionModelPublicationResolver>>,
    resource_registry: Option<Arc<dyn awaken_resource_contract::ResourceRegistry>>,
    resource_purge_scheduler: Option<Arc<dyn awaken_resource_contract::ResourcePurgeScheduler>>,
    resource_references: Option<Arc<dyn awaken_resource_contract::ResourceReferenceIndex>>,
    resource_files: Option<Arc<dyn awaken_resource_contract::FileCatalog>>,
    managed_list_prices: Option<Arc<dyn awaken_session_contract::ManagedListPriceProvider>>,
    sessions_repo: Arc<dyn ManagedSessionRepository>,
    lifecycle_notifier: Option<Arc<dyn LifecycleFactNotifier>>,
    local_realization_owner: String,
    runtime_incarnation: String,
    lifecycle_supervisor_started: AtomicBool,
    lifecycle_wakeup: tokio::sync::Notify,
}

static APPLICATION_INCARNATION_SEQ: AtomicU64 = AtomicU64::new(0);

impl SessionApplication {
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
            model_publication_resolver: None,
            resource_registry: None,
            resource_purge_scheduler: None,
            resource_references: None,
            resource_files: None,
            managed_list_prices: None,
            sessions_repo,
            lifecycle_notifier: None,
            local_realization_owner,
            runtime_incarnation: format!(
                "session:{}:{started_at}:{}",
                std::process::id(),
                APPLICATION_INCARNATION_SEQ.fetch_add(1, Ordering::Relaxed)
            ),
            lifecycle_supervisor_started: AtomicBool::new(false),
            lifecycle_wakeup: tokio::sync::Notify::new(),
        }
    }

    /// Exact immutable fact written into a newly compiled Session baseline.
    #[must_use]
    pub fn runtime_placement(&self) -> SessionRuntimePlacement {
        runtime_placement_for_topology(self.configuration.execution_placement)
    }

    /// Resolve the sole physical-realization owner from the immutable baseline.
    #[must_use]
    pub fn requires_external_realization(&self, session: &PersistedSession) -> bool {
        session
            .frozen_baseline()
            .is_some_and(|baseline| environment_requires_external_owner(baseline.runtime_placement))
    }

    /// Claim the one lifecycle supervisor for this application instance.
    pub fn claim_lifecycle_supervisor(&self) -> bool {
        claim_once(&self.lifecycle_supervisor_started)
    }

    pub(crate) fn wake_lifecycle_supervisor(&self) {
        self.lifecycle_wakeup.notify_one();
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
    pub(crate) fn resource_registry(
        &self,
    ) -> Option<&dyn awaken_resource_contract::ResourceRegistry> {
        self.resource_registry.as_deref()
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

    pub async fn resolve_managed_list_price_snapshot(
        &self,
        request: awaken_session_contract::ManagedListPriceRequest,
    ) -> Result<
        awaken_session_contract::ManagedListPriceSnapshot,
        awaken_session_contract::ManagedListPriceError,
    > {
        let provider = self.managed_list_prices.as_deref().ok_or_else(|| {
            awaken_session_contract::ManagedListPriceError::Unavailable(
                "no Managed list-price provider is configured".into(),
            )
        })?;
        let snapshot = provider.resolve_snapshot(request.clone()).await?;
        snapshot.validate(&request.model_refs)?;
        Ok(snapshot)
    }

    /// Clone the durable aggregate port for Coordinator-side claim-fenced
    /// transports that must authorize a projection frozen after dispatch.
    #[must_use]
    pub fn session_repository_handle(&self) -> Arc<dyn ManagedSessionRepository> {
        self.sessions_repo.clone()
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
            .unwrap_or_else(|| {
                awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID.to_string()
            });
        let exact_publication = published_environment.filter(|binding| {
            requested_environment_id.is_none_or(|requested| requested == binding.environment_id)
        });
        let required_revision = exact_publication.map(|binding| binding.revision);
        let resolved = if let Some(binding) = exact_publication {
            self.environments
                .resolve_exact_for_session(
                    &binding.environment_id,
                    binding.revision,
                    published_backend_ref,
                    mcp_targets,
                )
                .await
        } else {
            self.environments
                .resolve_current_for_session(&environment_id, published_backend_ref, mcp_targets)
                .await
        }
        .map_err(|error| RunError::unavailable(error.to_string()))?
        .ok_or_else(|| {
            RunError::bad_request(format!("environment `{environment_id}` is unavailable"))
        })?;
        if !awaken_session_contract::resolved_environment_snapshot_is_exact(
            resolved.snapshot.environment_id == environment_id,
            resolved.snapshot.revision.0,
            required_revision,
        ) {
            return Err(RunError::bad_request(format!(
                "environment `{environment_id}` resolver returned another identity or revision"
            )));
        }
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

    pub fn set_managed_list_price_provider(
        &mut self,
        provider: Arc<dyn awaken_session_contract::ManagedListPriceProvider>,
    ) {
        self.managed_list_prices = Some(provider);
    }

    pub fn set_lifecycle_notifier(&mut self, notifier: Arc<dyn LifecycleFactNotifier>) {
        self.lifecycle_notifier = Some(notifier);
    }

    pub fn set_resource_purge_scheduler(
        &mut self,
        scheduler: Arc<dyn awaken_resource_contract::ResourcePurgeScheduler>,
    ) {
        self.resource_purge_scheduler = Some(scheduler);
    }

    /// Install the Resource authorities used to publish the Session's durable
    /// active+pending retention set. These authorities are paired so logical
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

    pub fn set_model_publication_resolver(
        &mut self,
        resolver: Arc<dyn awaken_session_contract::SessionModelPublicationResolver>,
    ) {
        self.model_publication_resolver = Some(resolver);
    }

    /// Resolve a Session override only when it differs from the immutable Agent
    /// publication. This is the single admission rule shared by every protocol
    /// and internal Session creator.
    pub async fn resolve_session_model_override(
        &self,
        workspace_id: &str,
        requested_model: &str,
        published_model: &str,
        inference: awaken_runtime_contract::agent_bindings::InferenceOptions,
    ) -> Result<awaken_session_contract::SessionModelOverride, RunError> {
        let publication = match awaken_session_contract::session_model_override_decision(
            requested_model,
            published_model,
        ) {
            awaken_session_contract::SessionModelOverrideDecision::ReusePublished => None,
            awaken_session_contract::SessionModelOverrideDecision::ResolveComplete => {
                let resolver = self.model_publication_resolver.as_deref().ok_or_else(|| {
                    RunError::unavailable("Session model override resolution is not configured")
                })?;
                let publication = resolver
                    .resolve_session_model(workspace_id, requested_model)
                    .await
                    .map_err(|error| match error {
                        awaken_session_contract::SessionModelResolutionError::Invalid(_) => {
                            RunError::bad_request(error.to_string())
                        }
                        awaken_session_contract::SessionModelResolutionError::Unavailable(_) => {
                            RunError::unavailable(error.to_string())
                        }
                    })?;
                publication
                    .validate_for_workspace(workspace_id)
                    .map_err(|error| RunError::bad_request(error.to_string()))?;
                Some(Box::new(publication))
            }
        };
        Ok(awaken_session_contract::SessionModelOverride {
            publication,
            inference,
        })
    }

    /// Resolve the complete published multi-agent model roster before a
    /// budgeted Session is committed. The executable catalog remains the only
    /// Agent authority; this method merely projects its already-published
    /// profiles into the price-snapshot request.
    pub fn managed_session_model_refs(
        &self,
        workspace_id: &str,
        root_agent_id: &str,
        root_execution_model_ref: &str,
    ) -> Result<Vec<String>, RunError> {
        let mut models = std::collections::BTreeSet::from([root_execution_model_ref.to_owned()]);
        let Some(source) = &self.config_source else {
            return Ok(models.into_iter().collect());
        };
        let mut pending = source
            .session_profile_in(workspace_id, root_agent_id)
            .map(|profile| {
                if let Some(advisor) = profile.advisor_model {
                    models.insert(advisor);
                }
                profile.delegates
            })
            .unwrap_or_default();
        let mut visited = std::collections::BTreeSet::new();
        while let Some(delegate) = pending.pop() {
            let agent_id = delegate.agent_id;
            if !visited.insert(agent_id.clone()) {
                continue;
            }
            let profile = delegate
                .source_revision
                .and_then(|revision| {
                    source.session_profile_at_revision_in(workspace_id, &agent_id, revision)
                })
                .or_else(|| source.session_profile_in(workspace_id, &agent_id))
                .ok_or_else(|| {
                    RunError::bad_request(format!(
                        "budget_price_roster_unavailable: agent `{agent_id}` has no executable profile"
                    ))
                })?;
            let model = profile
                .execution_model_ref
                .or(profile.model)
                .filter(|model| !model.trim().is_empty())
                .ok_or_else(|| {
                    RunError::bad_request(format!(
                        "budget_price_roster_unavailable: agent `{agent_id}` has no model"
                    ))
                })?;
            models.insert(model);
            pending.extend(profile.delegates);
            if visited.len() > 25 {
                return Err(RunError::bad_request(
                    "budget_price_roster_unavailable: multi-agent roster exceeds 25 agents",
                ));
            }
        }
        Ok(models.into_iter().collect())
    }

    pub fn set_resource_registry(
        &mut self,
        catalog: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    ) {
        self.resource_registry = Some(catalog);
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
    let Some(baseline) = session.frozen_baseline() else {
        return Ok(false);
    };
    if !baseline.environment.self_hosted {
        return Ok(false);
    }
    if session.is_terminal() {
        // Terminal Work retirement belongs exclusively to the recoverable
        // resource cleanup driver. A second reconciler would duplicate the
        // external attempt and obscure its durable completion boundary.
        return Ok(false);
    }
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

async fn self_hosted_work_environment(
    application: &SessionApplication,
    session_id: &str,
) -> Result<Option<(String, bool)>, awaken_session_contract::work_queue::WorkQueueError> {
    let session = match application.session_repository().get(session_id).await {
        Ok(session) => session,
        Err(awaken_session_contract::SessionRepositoryError::NotFound) => return Ok(None),
        Err(error) => {
            return Err(
                awaken_session_contract::work_queue::WorkQueueError::Storage(error.to_string()),
            );
        }
    };
    Ok(session
        .frozen_baseline()
        .filter(|baseline| baseline.environment.self_hosted)
        .map(|baseline| {
            (
                baseline.environment.environment_id.clone(),
                session.is_terminal(),
            )
        }))
}

#[async_trait::async_trait]
impl awaken_session_contract::work_queue::SessionWorkLeaseAuthority for SessionApplication {
    async fn acquire_session_work(
        &self,
        session_id: &str,
        worker_owner: &str,
        now_ms: u64,
        acquisition: awaken_session_contract::work_queue::SessionWorkAcquisition,
    ) -> Result<
        awaken_session_contract::work_queue::SessionWorkOwnership,
        awaken_session_contract::work_queue::WorkQueueError,
    > {
        use awaken_session_contract::work_queue::SessionWorkOwnership;

        let Some((environment_id, terminal)) =
            self_hosted_work_environment(self, session_id).await?
        else {
            return Ok(SessionWorkOwnership::NotRequired);
        };
        let acquired = self
            .environments
            .acquire_session_work(&environment_id, session_id, worker_owner, now_ms)
            .await?;
        if !awaken_session_contract::work_queue::session_work_revival_is_authorized(
            acquisition,
            terminal,
            acquired.is_some(),
        ) {
            return Ok(acquired
                .map(SessionWorkOwnership::Leased)
                .unwrap_or(SessionWorkOwnership::Unowned));
        }
        // A predecessor may retire the stable Session Work item after a
        // successor admission already attempted to wake it. The authenticated,
        // claimed successor Run is itself a durable wake intent: revive through
        // the one WorkQueue path and retry acquisition exactly once. Terminal
        // Sessions are excluded above and can never be resurrected here.
        self.environments
            .wake_session_work(&environment_id, session_id)
            .await?;
        Ok(self
            .environments
            .acquire_session_work(&environment_id, session_id, worker_owner, now_ms)
            .await?
            .map(SessionWorkOwnership::Leased)
            .unwrap_or(SessionWorkOwnership::Unowned))
    }

    async fn release_session_work(
        &self,
        session_id: &str,
        worker_owner: &str,
        now_ms: u64,
    ) -> Result<bool, awaken_session_contract::work_queue::WorkQueueError> {
        let Some((environment_id, _terminal)) =
            self_hosted_work_environment(self, session_id).await?
        else {
            return Ok(true);
        };
        // Renew-and-compare through the one WorkQueue authority before retiring.
        // The refreshed lease prevents a different Worker from taking ownership
        // between the comparison and the idempotent Session retirement.
        let Some(lease) = self
            .environments
            .acquire_session_work(&environment_id, session_id, worker_owner, now_ms)
            .await?
        else {
            return Ok(false);
        };
        if lease.owner != worker_owner {
            return Ok(false);
        }
        self.environments
            .retire_session_work(&environment_id, session_id)
            .await?;
        Ok(true)
    }

    async fn release_worker_session_work(
        &self,
        worker_owner: &str,
    ) -> Result<usize, awaken_session_contract::work_queue::WorkQueueError> {
        self.environments
            .release_worker_session_work(worker_owner)
            .await
    }
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
                tracing::warn!(
                    session_id,
                    current_owner = session
                        .realization
                        .as_ref()
                        .map(|lease| lease.owner.as_str()),
                    current_incarnation = session
                        .realization
                        .as_ref()
                        .map(|lease| lease.runtime_incarnation.as_str()),
                    current_epoch = session.realization.as_ref().map(|lease| lease.epoch),
                    asserted_owner = receipt
                        .realization
                        .as_ref()
                        .map(|lease| lease.owner.as_str()),
                    asserted_incarnation = receipt
                        .realization
                        .as_ref()
                        .map(|lease| lease.runtime_incarnation.as_str()),
                    asserted_epoch = receipt.realization.as_ref().map(|lease| lease.epoch),
                    "rejected stale Session environment receipt"
                );
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
            if session.environment.generation().is_none()
                && let Some(baseline) = session.frozen_baseline()
            {
                let retention_ms = baseline
                    .environment
                    .idle_retention
                    .retention_secs
                    .saturating_mul(1_000);
                let expires_at_unix_ms = if retention_ms == 0 {
                    u64::MAX
                } else {
                    now_unix_ms.saturating_add(retention_ms)
                };
                let environment_fingerprint = baseline.environment.config_fingerprint.0.clone();
                let base_image_fingerprint = baseline
                    .environment
                    .prepared_image
                    .clone()
                    .unwrap_or_else(|| {
                        awaken_session_contract::stable_fingerprint(&baseline.environment.sandbox)
                    });
                session.environment.assign_generation(
                    awaken_session_contract::SandboxGeneration::new(
                        session_id,
                        now_unix_ms,
                        expires_at_unix_ms,
                        environment_fingerprint,
                        base_image_fingerprint,
                    ),
                );
            }
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
