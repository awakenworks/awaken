// @formal entrypoint executable_projection.failed_refresh_never_enters_handler
// @formal entrypoint model_route.session_override_reuses_only_exact_identity_and_resolves_every_mismatch
// @formal entrypoint runtime.backend_owned_environment_is_materialized_only_by_the_claimed_worker
// @formal entrypoint session_creation.selected_environment_is_frozen_in_baseline
// @formal entrypoint session_creation.work_projection_is_disposable_repairable_and_non_authoritative
// @formal entrypoint session_environment.owner_topology_is_exact_and_frozen
// @formal entrypoint session_environment.terminal_fence_admits_only_inflight_binding_receipts
// @formal entrypoint session_run.execution_requires_dispatch_work_and_realization_owner
// @formal entrypoint session_time.root_observations_are_revision_bound_and_replay_stable
// @formal entrypoint session_work.only_a_claimed_nonterminal_run_may_revive_stopped_work
use awaken_session_state_contract::{
    SessionEnvironmentEffectAuthorization, SessionEnvironmentEffectIntent,
    SessionEnvironmentReceipt, SessionEnvironmentReceiptError,
    SessionEnvironmentReceiptRecoveryAction, SessionEnvironmentState, SessionMutationResult,
    SessionRepositoryError,
};
use awaken_time::WallClockExt;
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

/// Reject a frozen topology whose idle policy would require Coordinator-local
/// continuation effects against a Worker-owned Environment. External Workers
/// have one realization/terminal-cleanup claim path, but no claim-fenced
/// nonterminal quiesce, checkpoint, disposal, or restore transport.
fn validate_environment_continuation_topology(
    placement: awaken_session_state_contract::SessionPlacementClass,
    idle_retention: &awaken_session_configuration_contract::EnvironmentIdleRetentionPolicy,
) -> Result<(), RunError> {
    if placement.requires_external_environment_owner()
        && idle_retention.mode
            == awaken_session_configuration_contract::EnvironmentIdleRetentionMode::CheckpointAndRelease
    {
        return Err(RunError::bad_request(
            "checkpoint_and_release idle retention is unsupported for external Worker placement",
        ));
    }
    Ok(())
}

#[cfg(kani)]
mod environment_owner_kani_proof {
    use super::{
        SessionExecutionPlacement, SessionRuntimePlacement, runtime_placement_for_topology,
    };
    use awaken_session_state_contract::SessionPlacementClass;

    /// @obligation runtime.backend_owned_environment_is_materialized_only_by_the_claimed_worker
    /// @obligation session_environment.owner_topology_is_exact_and_frozen
    /// @obligation session_run.execution_requires_dispatch_work_and_realization_owner
    #[kani::proof]
    fn environment_owner_topology_is_exact_and_frozen() {
        // Process topology freezes exactly one runtime placement, and the one
        // contract placement class selects the Environment owner from that
        // placement alone: the self-hosted Environment bit never moves it.
        let registered: bool = kani::any();
        let topology = if registered {
            SessionExecutionPlacement::RegisteredWorker
        } else {
            SessionExecutionPlacement::LocalWorker
        };
        let frozen = runtime_placement_for_topology(topology);
        assert_eq!(
            frozen,
            if registered {
                SessionRuntimePlacement::Worker
            } else {
                SessionRuntimePlacement::Local
            }
        );
        assert_eq!(
            SessionPlacementClass::new(frozen, kani::any()).requires_external_environment_owner(),
            registered
        );
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
    pub quarantined: Vec<awaken_session_state_contract::SessionRecoveryQuarantine>,
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

/// One immutable persistence selection made before the execution Host exists.
/// The application and its Environment receipt sink always share these exact
/// Store/Clock handles; no app-to-Host installation is needed to create it.
pub struct SessionPersistence {
    store: Arc<dyn awaken_session_repository_contract::SessionStore>,
    clock: Arc<dyn awaken_time::WallClock>,
    environment_binding_sink: Arc<dyn SessionEnvironmentBindingSink>,
}

impl SessionPersistence {
    #[must_use]
    pub fn new(
        store: Arc<dyn awaken_session_repository_contract::SessionStore>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        let repo: Arc<dyn SessionRepository> = store.clone();
        let environment_binding_sink =
            Arc::new(RepositoryEnvironmentBindingSink::new(repo, clock.clone()));
        Self {
            store,
            clock,
            environment_binding_sink,
        }
    }

    #[must_use]
    pub fn environment_binding_sink(&self) -> Arc<dyn SessionEnvironmentBindingSink> {
        self.environment_binding_sink.clone()
    }
}

/// Canonical application object shared by wire adapters.
pub struct SessionApplication {
    clock: Arc<dyn awaken_time::WallClock>,
    configuration: SessionApplicationConfiguration,
    runtime: crate::SessionExecutor,
    mcp_realizer: Arc<dyn McpAttachmentRealizer>,
    credential_source: Option<Arc<dyn SessionCredentialSource>>,
    credential_material_ingress: Option<Arc<dyn CredentialMaterialIngress>>,
    environments: Arc<dyn SessionEnvironmentSource>,
    config_source: Option<Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>>,
    model_publication_resolver:
        Option<Arc<dyn awaken_session_configuration_contract::SessionModelPublicationResolver>>,
    resource_registry: Option<Arc<dyn awaken_resource_contract::ResourceRegistry>>,
    resource_purge_scheduler: Option<Arc<dyn awaken_resource_contract::ResourcePurgeScheduler>>,
    resource_references: Option<Arc<dyn awaken_resource_contract::ResourceReferenceIndex>>,
    resource_files: Option<Arc<dyn awaken_resource_contract::FileCatalog>>,
    managed_list_prices: Option<Arc<dyn awaken_session_budget_contract::ManagedListPriceProvider>>,
    sessions_repo: Arc<dyn SessionRepository>,
    environment_binding_sink: Arc<dyn SessionEnvironmentBindingSink>,
    /// Drain side of the same store's transactional lifecycle outbox. The
    /// application never reads it; it only hands it to the one delivery
    /// consumer bound through [`Self::set_lifecycle_notifier`].
    lifecycle_outbox: Arc<dyn awaken_session_repository_contract::LifecycleOutbox>,
    /// One process-owned high-water reconciler for the rebuildable Agent and
    /// Environment projections. It is late-bound by split-process composition;
    /// AllInOne has no cross-replica projection and leaves it absent.
    executable_projection_refresh:
        Option<Arc<awaken_session_application_contract::ExecutableProjectionRefresh>>,
    /// One process-owned wake channel for the durable lifecycle outbox.  This is
    /// late-bound because scenario composition shares an already-`Arc`'d
    /// application, and write-once so a second outbox consumer cannot replace
    /// the canonical supervisor after traffic starts.
    lifecycle_notifier: OnceLock<Arc<dyn LifecycleFactNotifier>>,
    /// Canonical registered-Worker observation used immediately before an
    /// external terminal-cleanup lease CAS. The HTTP adapter authenticates the
    /// request, while this application-owned read prevents direct callers or a
    /// stale adapter snapshot from assigning an incompatible Worker.
    worker_observations: OnceLock<Arc<dyn awaken_worker_contract::WorkerObservationSource>>,
    /// Read side of the existing dispatch authority, bound once at composition.
    /// It grants no execution, claim, resume or persistence authority.
    dispatch_observations: OnceLock<Arc<dyn awaken_dispatch_contract::DispatchOperationalFeed>>,
    session_preparation_placement:
        OnceLock<Arc<dyn awaken_session_realization_contract::SessionPreparationPlacement>>,
    /// Disposable keyset position for one bounded preparation-candidate page
    /// per poll. The root and its CAS remain the scheduling authority.
    session_preparation_cursors: tokio::sync::Mutex<
        BTreeMap<
            String,
            Arc<tokio::sync::Mutex<Option<awaken_session_state_contract::SessionScanCursor>>>,
        >,
    >,
    local_realization_owner: String,
    runtime_incarnation: String,
    #[cfg(any(test, feature = "test-support"))]
    mutation_sequence: AtomicU64,
    lifecycle_supervisor_started: AtomicBool,
    lifecycle_wakeup: tokio::sync::Notify,
    event_batch_cutover_validation: Arc<SessionEventBatchCutoverValidationSource>,
    /// Process-local mutual exclusion for the co-located realization driver.
    local_realization_locks: Arc<crate::realization::LocalRealizationLocks>,
}

static APPLICATION_INCARNATION_SEQ: AtomicU64 = AtomicU64::new(0);

impl SessionApplication {
    /// Install the existing clock port before sharing the application.
    #[must_use]
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_clock(mut self, clock: Arc<dyn awaken_time::WallClock>) -> Self {
        self.clock = clock;
        self.environment_binding_sink = Arc::new(RepositoryEnvironmentBindingSink::new(
            self.sessions_repo.clone(),
            self.clock.clone(),
        ));
        self
    }

    /// The injected clock port, for an adapter that stamps observations on the
    /// same time line as this application's commands.
    #[must_use]
    pub fn clock(&self) -> Arc<dyn awaken_time::WallClock> {
        self.clock.clone()
    }

    pub(crate) fn realization_retry_budget(&self) -> u32 {
        self.configuration.realization_retry_budget.max(1)
    }

    /// `clock` is the process's one wall clock, chosen by the composition that
    /// builds this application.
    #[must_use]
    pub fn new(
        runtime: impl Into<crate::SessionExecutor>,
        mcp_realizer: Arc<dyn McpAttachmentRealizer>,
        persistence: SessionPersistence,
        environments: Arc<dyn SessionEnvironmentSource>,
    ) -> Self {
        Self::new_with_configuration(
            runtime,
            mcp_realizer,
            persistence,
            environments,
            SessionApplicationConfiguration::default(),
        )
    }

    /// `clock` stamps every Session fact this application writes and judges
    /// every lease it holds; the composition passes the process's one clock.
    #[must_use]
    pub fn new_with_configuration(
        runtime: impl Into<crate::SessionExecutor>,
        mcp_realizer: Arc<dyn McpAttachmentRealizer>,
        persistence: SessionPersistence,
        environments: Arc<dyn SessionEnvironmentSource>,
        configuration: SessionApplicationConfiguration,
    ) -> Self {
        let SessionPersistence {
            store,
            clock,
            environment_binding_sink,
        } = persistence;
        let lifecycle_outbox: Arc<dyn awaken_session_repository_contract::LifecycleOutbox> =
            store.clone();
        let sessions_repo: Arc<dyn SessionRepository> = store;
        // The incarnation stem: this application's start instant on its clock.
        let started_at = clock.now().as_unix_nanos();
        let local_realization_owner = configuration.local_realization_owner.trim().to_string();
        assert!(
            !local_realization_owner.is_empty(),
            "local Session realization owner must not be empty"
        );
        Self {
            clock,
            configuration,
            runtime: runtime.into(),
            mcp_realizer,
            credential_source: None,
            credential_material_ingress: None,
            environments,
            config_source: None,
            model_publication_resolver: None,
            resource_registry: None,
            resource_purge_scheduler: None,
            resource_references: None,
            resource_files: None,
            managed_list_prices: None,
            sessions_repo,
            environment_binding_sink,
            lifecycle_outbox,
            executable_projection_refresh: None,
            lifecycle_notifier: OnceLock::new(),
            worker_observations: OnceLock::new(),
            dispatch_observations: OnceLock::new(),
            session_preparation_placement: OnceLock::new(),
            session_preparation_cursors: tokio::sync::Mutex::new(BTreeMap::new()),
            local_realization_owner,
            runtime_incarnation: format!(
                "session:{}:{started_at}:{}",
                std::process::id(),
                APPLICATION_INCARNATION_SEQ.fetch_add(1, Ordering::Relaxed)
            ),
            local_realization_locks: Arc::default(),
            #[cfg(any(test, feature = "test-support"))]
            mutation_sequence: AtomicU64::new(0),
            lifecycle_supervisor_started: AtomicBool::new(false),
            lifecycle_wakeup: tokio::sync::Notify::new(),
            event_batch_cutover_validation: Arc::new(
                SessionEventBatchCutoverValidationSource::new(),
            ),
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
        session.requires_external_environment_owner()
    }

    /// Claim the one lifecycle supervisor for this application instance.
    pub fn claim_lifecycle_supervisor(&self) -> bool {
        claim_once(&self.lifecycle_supervisor_started)
    }

    pub(crate) fn wake_lifecycle_supervisor(&self) {
        self.lifecycle_wakeup.notify_one();
    }

    /// Signal that committed Runtime progress may unblock retained Session work.
    /// Process composition connects its completion observer here; the
    /// application remains the sole owner of reconciliation state and effects.
    pub fn request_lifecycle_reconciliation(&self) {
        self.wake_lifecycle_supervisor();
    }

    #[must_use]
    pub(crate) fn runtime(&self) -> &crate::SessionExecutor {
        &self.runtime
    }

    /// Invoke the Runtime's pure provider-effective Sandbox-layout gate. This
    /// method performs no durable mutation or external realization and is the
    /// one application seam used before create, live Resource, and final root
    /// persistence boundaries.
    pub fn validate_session_sandbox_layout(
        &self,
        session_id: &str,
        layout: &awaken_session_configuration_contract::SessionSandboxLayout,
    ) -> Result<(), RunError> {
        let realization_runtime = self.runtime().realization();

        validate_environment_continuation_topology(
            awaken_session_state_contract::SessionPlacementClass::new(
                layout.runtime_placement,
                layout.environment.self_hosted,
            ),
            &layout.environment.idle_retention,
        )?;
        realization_runtime.validate_session_sandbox_layout(session_id, layout)
    }

    /// Revalidate a proposed complete Resource manifest against the immutable
    /// baseline already owned by a Session. The caller may pass provisional
    /// typed bindings before Repository/Vault configuration because layout does
    /// not depend on catalog versions or credential pins.
    pub fn validate_persisted_session_resource_layout(
        &self,
        workspace_id: &str,
        session: &PersistedSession,
        resources: Vec<awaken_resource_contract::InputBinding>,
    ) -> Result<(), RunError> {
        let baseline = session
            .frozen_baseline()
            .ok_or_else(|| RunError::unavailable("Session creation intent was not consumed"))?;
        self.validate_session_sandbox_layout(
            session.session_id.as_str(),
            &awaken_session_configuration_contract::SessionSandboxLayout {
                workspace_id: workspace_id.to_string(),
                agent_id: baseline.agent_id.clone(),
                agent_revision: baseline.agent_revision,
                runtime_placement: baseline.runtime_placement,
                model_override: baseline.model_override.clone(),
                runtime: baseline.runtime.clone(),
                mounts: baseline.mounts.clone(),
                env: baseline.env.clone(),
                environment: baseline.environment.clone(),
                resources,
            },
        )
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
    pub(crate) fn credential_material_ingress(&self) -> Option<&dyn CredentialMaterialIngress> {
        self.credential_material_ingress.as_deref()
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
    pub(crate) fn session_repository(&self) -> &dyn SessionRepository {
        self.sessions_repo.as_ref()
    }

    /// Read the Session aggregate's one durable realization-owner authority.
    /// Lease expiry does not release an incumbent Worker: only verified terminal
    /// cleanup can make its retained physical source safe for replacement.
    /// The repository query is complete and fail-closed across Workspaces.
    pub async fn has_unfinished_worker_realizations(
        &self,
        worker_id: &str,
    ) -> Result<bool, RunError> {
        if worker_id.trim().is_empty() {
            return Err(RunError::bad_request("Worker realization owner is empty"));
        }
        let scan = self
            .session_repository()
            .scan(
                awaken_session_state_contract::SessionScanQuery::WorkerRealizationOwner {
                    worker_id,
                },
            )
            .await
            .map_err(session_repository_error)?;
        if scan.next_cursor.is_some() || !scan.quarantined.is_empty() {
            return Err(RunError::unavailable(
                "Worker realization-owner read is incomplete",
            ));
        }
        for scoped in scan.sessions {
            let session = scoped.session;
            if session
                .realization
                .as_ref()
                .is_none_or(|lease| lease.owner != worker_id)
            {
                return Err(RunError::unavailable(
                    "Worker realization-owner read disagrees with Session root",
                ));
            }
            if session
                .has_unfinished_worker_realization()
                .map_err(|error| RunError::unavailable(error.to_string()))?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub async fn resolve_managed_list_price_snapshot(
        &self,
        request: awaken_session_budget_contract::ManagedListPriceRequest,
    ) -> Result<
        awaken_session_budget_contract::ManagedListPriceSnapshot,
        awaken_session_budget_contract::ManagedListPriceError,
    > {
        let provider = self.managed_list_prices.as_deref().ok_or_else(|| {
            awaken_session_budget_contract::ManagedListPriceError::Unavailable(
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
    pub fn session_repository_handle(&self) -> Arc<dyn SessionRepository> {
        self.sessions_repo.clone()
    }

    /// Clone the drain port of the same store for the one lifecycle delivery
    /// consumer installed by Coordinator composition.
    #[must_use]
    pub fn lifecycle_outbox_handle(
        &self,
    ) -> Arc<dyn awaken_session_repository_contract::LifecycleOutbox> {
        self.lifecycle_outbox.clone()
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

    /// Serialize this process's own realization drivers for one Session.
    pub(crate) fn local_realization_locks(
        &self,
    ) -> &Arc<crate::realization::LocalRealizationLocks> {
        &self.local_realization_locks
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
        if !awaken_session_configuration_contract::resolved_environment_snapshot_is_exact(
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
    ) -> Result<bool, awaken_worker_contract::WorkQueueError> {
        dispatch_session_work(self.environments.as_ref(), session).await
    }

    /// Restore every missing Session-to-WorkQueue projection from durable truth.
    pub async fn reconcile_work_dispatches(&self) -> WorkDispatchReconciliation {
        reconcile_work_dispatches(self.sessions_repo.as_ref(), self.environments.as_ref()).await
    }

    pub fn environment_binding_sink(&self) -> Arc<dyn SessionEnvironmentBindingSink> {
        self.environment_binding_sink.clone()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn replace_repository(
        &mut self,
        repo: Arc<dyn awaken_session_repository_contract::SessionStore>,
    ) {
        self.lifecycle_outbox = repo.clone();
        self.sessions_repo = repo;
        self.environment_binding_sink = Arc::new(RepositoryEnvironmentBindingSink::new(
            self.sessions_repo.clone(),
            self.clock.clone(),
        ));
    }

    pub fn replace_environment_source(&mut self, source: Arc<dyn SessionEnvironmentSource>) {
        self.environments = source;
    }

    pub fn set_managed_list_price_provider(
        &mut self,
        provider: Arc<dyn awaken_session_budget_contract::ManagedListPriceProvider>,
    ) {
        self.managed_list_prices = Some(provider);
    }

    /// Bind the one lifecycle-outbox wake channel. Duplicate composition is an
    /// error instead of silently replacing (or running alongside) its owner.
    pub fn set_lifecycle_notifier(
        &self,
        notifier: Arc<dyn LifecycleFactNotifier>,
    ) -> Result<(), &'static str> {
        self.lifecycle_notifier
            .set(notifier)
            .map_err(|_| "Session lifecycle notifier is already installed")
    }

    /// Bind the one Coordinator-owned registered-Worker observation source.
    /// Duplicate composition is rejected instead of letting HTTP and the root
    /// CAS consult different registries.
    pub fn set_worker_observation_source(
        &self,
        source: Arc<dyn awaken_worker_contract::WorkerObservationSource>,
    ) -> Result<(), &'static str> {
        self.worker_observations
            .set(source)
            .map_err(|_| "Worker observation source is already installed")
    }

    /// Bind the same dispatch feed used by the Runtime. A second source cannot
    /// replace the cursor partition after Run observation has begun.
    pub fn set_dispatch_observation_source(
        &self,
        source: Arc<dyn awaken_dispatch_contract::DispatchOperationalFeed>,
    ) -> Result<(), &'static str> {
        self.dispatch_observations
            .set(source)
            .map_err(|_| "Session dispatch observation source is already installed")
    }

    pub(crate) fn worker_observation_source(
        &self,
    ) -> Option<&dyn awaken_worker_contract::WorkerObservationSource> {
        self.worker_observations.get().map(Arc::as_ref)
    }

    /// Bind the Runtime Host's one Sandbox/Resource capability compiler. A
    /// missing compiler disables no-Run Worker claims rather than guessing a
    /// weaker placement vector in the Session application.
    pub fn set_session_preparation_placement(
        &self,
        placement: Arc<dyn awaken_session_realization_contract::SessionPreparationPlacement>,
    ) -> Result<(), &'static str> {
        self.session_preparation_placement
            .set(placement)
            .map_err(|_| "Session preparation placement is already installed")
    }

    pub(crate) fn session_preparation_placement(
        &self,
    ) -> Option<&dyn awaken_session_realization_contract::SessionPreparationPlacement> {
        self.session_preparation_placement.get().map(Arc::as_ref)
    }

    /// Bind the one executable-projection reconciliation operation used by
    /// request and autonomous Session boundaries. Duplicate installation is an
    /// error instead of allowing two freshness authorities to overlap.
    pub fn set_executable_projection_refresh(
        &mut self,
        refresh: Arc<awaken_session_application_contract::ExecutableProjectionRefresh>,
    ) -> Result<(), &'static str> {
        if self.executable_projection_refresh.is_some() {
            return Err("executable projection refresh is already installed");
        }
        self.executable_projection_refresh = Some(refresh);
        Ok(())
    }

    /// Advance the injected executable projections, or no-op in an AllInOne
    /// composition where the authoritative catalogs are process-local.
    pub async fn refresh_executable_projections(
        &self,
    ) -> Result<awaken_session_application_contract::ExecutableProjectionAdmission, String> {
        match self.executable_projection_refresh.as_ref() {
            Some(refresh) => refresh.refresh().await,
            None => Ok(
                awaken_session_application_contract::process_local_executable_projection_admission(
                ),
            ),
        }
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

    pub fn set_credential_material_ingress(&mut self, ingress: Arc<dyn CredentialMaterialIngress>) {
        self.credential_material_ingress = Some(ingress);
    }

    pub fn set_config_source(
        &mut self,
        source: Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>,
    ) {
        self.config_source = Some(source);
    }

    pub fn set_model_publication_resolver(
        &mut self,
        resolver: Arc<dyn awaken_session_configuration_contract::SessionModelPublicationResolver>,
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
    ) -> Result<awaken_session_configuration_contract::SessionModelOverride, RunError> {
        let publication = match awaken_session_configuration_contract::session_model_override_decision(
            requested_model,
            published_model,
        ) {
            awaken_session_configuration_contract::SessionModelOverrideDecision::ReusePublished => None,
            awaken_session_configuration_contract::SessionModelOverrideDecision::ResolveComplete => {
                let resolver = self.model_publication_resolver.as_deref().ok_or_else(|| {
                    RunError::unavailable("Session model override resolution is not configured")
                })?;
                let publication = resolver
                    .resolve_session_model(workspace_id, requested_model)
                    .await
                    .map_err(|error| match error {
                        awaken_session_configuration_contract::SessionModelResolutionError::Invalid(_) => {
                            RunError::bad_request(error.to_string())
                        }
                        awaken_session_configuration_contract::SessionModelResolutionError::Unavailable(_) => {
                            RunError::unavailable(error.to_string())
                        }
                    })?;
                publication
                    .validate_for_workspace(workspace_id)
                    .map_err(|error| RunError::bad_request(error.to_string()))?;
                Some(Box::new(publication))
            }
        };
        Ok(
            awaken_session_configuration_contract::SessionModelOverride {
                publication,
                inference,
            },
        )
    }

    /// Compile the complete published model roster for a budgeted Session and
    /// reject execution backends that cannot cross the per-request admission
    /// boundary. The caller supplies the already-frozen root profile and model
    /// publication; ordinary roster members are reopened only by the exact
    /// revisions carried on that profile, and the Advisor's complete candidate
    /// comes from that same executable snapshot. Primary and fallback candidates
    /// cross this one walk. It owns both backend compatibility and the complete
    /// price-snapshot model request.
    pub fn managed_session_model_refs(
        &self,
        workspace_id: &str,
        root_agent_id: &str,
        root_profile: Option<&awaken_executable_agent_contract::ExecutableAgentSessionProfile>,
        root_execution_model_ref: &str,
        root_backend_ref: Option<&str>,
        root_model_publication: Option<
            &awaken_session_configuration_contract::SessionModelPublication,
        >,
    ) -> Result<Vec<String>, RunError> {
        fn add_candidate(
            models: &mut std::collections::BTreeSet<String>,
            agent_id: &str,
            candidate: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
        ) -> Result<(), RunError> {
            let backend_ref = &candidate.binding().identity().2;
            if !is_native_session_runtime(Some(backend_ref)) {
                return Err(RunError::bad_request(format!(
                    "budget_backend_unsupported: Agent `{agent_id}` uses external backend `{backend_ref}`"
                )));
            }
            if candidate.binding().identity().1.trim().is_empty() {
                return Err(RunError::bad_request(format!(
                    "budget_price_roster_unavailable: Agent `{agent_id}` has no model"
                )));
            }
            models.insert(candidate.binding().identity().1.to_owned());
            Ok(())
        }

        let mut models = std::collections::BTreeSet::new();
        let source = self.config_source.as_ref();
        let root_snapshot = root_profile
            .filter(|profile| profile.source_revision > 0)
            .map(|profile| {
                source
                    .and_then(|source| {
                        source.executable_snapshot_at_revision_in(
                            workspace_id,
                            root_agent_id,
                            profile.source_revision,
                        )
                    })
                    .ok_or_else(|| {
                        RunError::bad_request(format!(
                            "budget_price_roster_unavailable: Agent `{root_agent_id}` revision {} has no executable publication",
                            profile.source_revision
                        ))
                    })
            })
            .transpose()?;
        if let Some(snapshot) = &root_snapshot
            && snapshot.root_agent_id.0 != root_agent_id
        {
            return Err(RunError::internal(format!(
                "Agent `{root_agent_id}` executable identity does not match its profile"
            )));
        }
        if let Some(publication) = root_model_publication {
            for candidate in
                std::iter::once(&publication.primary).chain(publication.candidates.iter())
            {
                add_candidate(&mut models, root_agent_id, candidate)?;
            }
        } else if let Some(snapshot) = &root_snapshot {
            for candidate in std::iter::once(&snapshot.resolved_spec.model_binding)
                .chain(snapshot.resolved_spec.model_candidates.iter())
            {
                add_candidate(&mut models, root_agent_id, candidate)?;
            }
        } else {
            if !is_native_session_runtime(root_backend_ref) {
                let backend_ref = root_backend_ref.unwrap_or_default();
                return Err(RunError::bad_request(format!(
                    "budget_backend_unsupported: Agent `{root_agent_id}` uses external backend `{backend_ref}`"
                )));
            }
            if root_execution_model_ref.trim().is_empty() {
                return Err(RunError::bad_request(format!(
                    "budget_price_roster_unavailable: Agent `{root_agent_id}` has no model"
                )));
            }
            models.insert(root_execution_model_ref.to_owned());
        }
        if let Some(advisor) = root_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.resolved_spec.plugin_config.agent.advisor.as_ref())
        {
            add_candidate(&mut models, root_agent_id, &advisor.candidate)?;
        } else if root_profile.is_some_and(|profile| profile.advisor_model.is_some()) {
            return Err(RunError::bad_request(format!(
                "budget_price_roster_unavailable: Agent `{root_agent_id}` Advisor has no exact executable publication"
            )));
        }

        let Some(root_profile) = root_profile else {
            return Ok(models.into_iter().collect());
        };
        if root_profile.delegates.is_empty() {
            return Ok(models.into_iter().collect());
        }
        let source = source.ok_or_else(|| {
            RunError::bad_request(
                "budget_price_roster_unavailable: Agent publication source is unavailable",
            )
        })?;
        for delegate in &root_profile.delegates {
            let agent_id = &delegate.agent_id;
            let revision = delegate.source_revision.ok_or_else(|| {
                RunError::bad_request(format!(
                    "budget_price_roster_unavailable: Agent `{agent_id}` has no exact publication revision"
                ))
            })?;
            let snapshot = source
                .executable_snapshot_at_revision_in(workspace_id, agent_id, revision)
                .ok_or_else(|| {
                    RunError::bad_request(format!(
                        "budget_price_roster_unavailable: Agent `{agent_id}` revision {revision} has no executable publication"
                    ))
                })?;
            if snapshot.root_agent_id.0 != *agent_id {
                return Err(RunError::internal(format!(
                    "Agent `{agent_id}` executable identity does not match its profile"
                )));
            }
            for candidate in std::iter::once(&snapshot.resolved_spec.model_binding)
                .chain(snapshot.resolved_spec.model_candidates.iter())
            {
                add_candidate(&mut models, agent_id, candidate)?;
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

fn is_native_session_runtime(runtime: Option<&str>) -> bool {
    runtime.is_none_or(|backend_ref| {
        matches!(
            awaken_runtime_contract::resolved::Backend::from_ref(backend_ref),
            awaken_runtime_contract::resolved::Backend::Native
        )
    })
}

fn validate_sandbox_provisioning_runtime(
    provisioning: SandboxProvisioning,
    runtime: Option<&str>,
) -> Result<(), RunError> {
    if provisioning == SandboxProvisioning::OnToolUse && !is_native_session_runtime(runtime) {
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
) -> Result<bool, awaken_worker_contract::WorkQueueError> {
    if session.is_terminal() {
        // Keep the externally auditable terminal guard at the effect boundary.
        // The shared plan below repeats the same immutable fact so recovery and
        // create-time callers cannot acquire a second projection policy.
        return Ok(false);
    }
    if !session.requires_work_projection() {
        return Ok(false);
    }
    let Some(baseline) = session.frozen_baseline() else {
        // The closed production decision above cannot admit this state. Keep
        // the adapter fail-closed if the representation is ever extended.
        return Ok(false);
    };
    // Terminal Work retirement belongs exclusively to the recoverable resource
    // cleanup driver. A second reconciler would duplicate the external attempt
    // and obscure its durable completion boundary.
    environments
        .enqueue_session_work(
            &baseline.environment.environment_id,
            session.session_id.as_str(),
        )
        .await?;
    Ok(true)
}

async fn reconcile_work_dispatches(
    sessions: &dyn SessionRepository,
    environments: &dyn SessionEnvironmentSource,
) -> WorkDispatchReconciliation {
    let mut report = WorkDispatchReconciliation::default();
    let candidates = match scan_recovery_candidates(sessions, false).await {
        Ok(candidates) => candidates,
        Err(error) => {
            report.failures.push(WorkDispatchFailure {
                session_id: "<repository>".to_string(),
                environment_id: String::new(),
                message: error.to_string(),
            });
            return report;
        }
    };
    report.pending = candidates.sessions.len();
    report.quarantined = candidates.quarantined;
    for candidate in candidates.sessions {
        // The complete scan retains identities only. Reuse the ordinary fresh
        // root read and dispatch policy before an external WorkQueue effect.
        let session = match sessions.get(&candidate.session_id).await {
            Ok(session) => session,
            Err(SessionRepositoryError::NotFound) => continue,
            Err(error) => {
                report.failures.push(WorkDispatchFailure {
                    session_id: candidate.session_id.to_string(),
                    environment_id: String::new(),
                    message: error.to_string(),
                });
                continue;
            }
        };
        match dispatch_session_work(environments, &session).await {
            Ok(true) => report.settled += 1,
            Ok(false) => {}
            Err(error) => report.failures.push(WorkDispatchFailure {
                session_id: session.session_id.to_string(),
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
    session_id: &awaken_session_types::SessionId,
) -> Result<Option<(String, bool)>, awaken_worker_contract::WorkQueueError> {
    let session = match application.session_repository().get(session_id).await {
        Ok(session) => session,
        Err(SessionRepositoryError::NotFound) => return Ok(None),
        Err(error) => {
            return Err(awaken_worker_contract::WorkQueueError::Storage(
                error.to_string().into(),
            ));
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
impl awaken_worker_contract::SessionWorkLeaseAuthority for SessionApplication {
    fn request_lifecycle_reconciliation(&self) {
        SessionApplication::request_lifecycle_reconciliation(self);
    }

    async fn acquire_session_work(
        &self,
        session_id: &awaken_session_types::SessionId,
        worker_owner: &str,
        now_ms: u64,
        acquisition: awaken_worker_contract::SessionWorkAcquisition,
    ) -> Result<awaken_worker_contract::SessionWorkOwnership, awaken_worker_contract::WorkQueueError>
    {
        use awaken_worker_contract::SessionWorkOwnership;

        let Some((environment_id, terminal)) =
            self_hosted_work_environment(self, session_id).await?
        else {
            return Ok(SessionWorkOwnership::NotRequired);
        };
        let acquired = self
            .environments
            .acquire_session_work(&environment_id, session_id.as_str(), worker_owner, now_ms)
            .await?;
        if !awaken_worker_contract::session_work_revival_is_authorized(
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
            .wake_session_work(&environment_id, session_id.as_str())
            .await?;
        Ok(self
            .environments
            .acquire_session_work(&environment_id, session_id.as_str(), worker_owner, now_ms)
            .await?
            .map(SessionWorkOwnership::Leased)
            .unwrap_or(SessionWorkOwnership::Unowned))
    }

    async fn release_session_work(
        &self,
        lease: &awaken_worker_contract::SessionWorkLease,
    ) -> Result<bool, awaken_worker_contract::WorkQueueError> {
        let Some((environment_id, _terminal)) =
            self_hosted_work_environment(self, lease.session_id()).await?
        else {
            return Ok(true);
        };
        if lease.environment_id() != environment_id {
            return Ok(false);
        }
        self.environments.release_session_work(lease).await
    }

    async fn release_worker_session_work(
        &self,
        worker_owner: &str,
    ) -> Result<usize, awaken_worker_contract::WorkQueueError> {
        self.environments
            .release_worker_session_work(worker_owner)
            .await
    }
}

/// Repository-backed implementation installed at the runtime materialization boundary.
pub struct RepositoryEnvironmentBindingSink {
    repo: Arc<dyn SessionRepository>,
    clock: Arc<dyn awaken_time::WallClock>,
}

impl RepositoryEnvironmentBindingSink {
    /// `clock` stamps each binding receipt; the owning application passes its own.
    #[must_use]
    pub fn new(repo: Arc<dyn SessionRepository>, clock: Arc<dyn awaken_time::WallClock>) -> Self {
        Self { repo, clock }
    }

    fn receipt_error(error: SessionEnvironmentReceiptError) -> RunError {
        match error.recovery_action() {
            SessionEnvironmentReceiptRecoveryAction::Retry => {
                let code = if matches!(
                    error,
                    SessionEnvironmentReceiptError::MaterializationUnknown
                        | SessionEnvironmentReceiptError::MaterializationPending
                ) {
                    "session_environment_materialization_pending"
                } else if matches!(error, SessionEnvironmentReceiptError::RealizationStale) {
                    "session_realization_stale"
                } else {
                    "session_environment_receipt_stale"
                };
                RunError::unavailable_classified(code, error.to_string())
            }
            SessionEnvironmentReceiptRecoveryAction::Reject => {
                RunError::classified("session_environment_receipt_rejected", error.to_string())
            }
        }
    }

    fn committed_authority(
        session: &PersistedSession,
        receipt: &SessionEnvironmentReceipt,
    ) -> Result<SessionEnvironmentState, RunError> {
        // The aggregate receipt kernel has already restricted terminal writes
        // to the Fenced join window. Accept its exact Store-read Resident value
        // so terminal cleanup can dispose an Environment whose provider effect
        // crossed the archive CAS; this function must not re-decide that phase.
        session
            .environment
            .committed_receipt_generation(receipt)
            .map(|_| session.environment.clone())
            .map_err(|_| {
                RunError::classified(
                    "session_environment_receipt_rejected",
                    "durable Environment does not match the exact binding receipt",
                )
            })
    }
}

/// Closed commands of the one repository-backed Environment CAS protocol.
/// Domain admission remains in Session; this enum only selects the existing
/// command and its exact readback/error policy, never an arbitrary DTO mutator.
enum EnvironmentBindingCommand<'a> {
    Admit(&'a SessionEnvironmentEffectIntent),
    Persist(&'a SessionEnvironmentReceipt),
}

enum EnvironmentBindingResult {
    Admitted(SessionEnvironmentEffectAuthorization),
    Bound(Box<SessionEnvironmentState>),
}

impl EnvironmentBindingCommand<'_> {
    fn session_id(&self) -> &awaken_session_types::SessionId {
        match self {
            Self::Admit(intent) => intent.session_id(),
            Self::Persist(receipt) => &receipt.session_id,
        }
    }

    fn operation(&self) -> &'static str {
        match self {
            Self::Admit(_) => "admit-environment",
            Self::Persist(_) => "bind-environment",
        }
    }

    fn apply(
        &self,
        session: &mut SessionAggregate,
        owner: &str,
        now: u64,
    ) -> Result<(bool, EnvironmentBindingResult), RunError> {
        match self {
            Self::Admit(intent) => {
                let (changed, authorization) = session
                    .environment()
                    .admit_environment_effect(owner, intent, now)
                    .map_err(RepositoryEnvironmentBindingSink::receipt_error)?;
                Ok((changed, EnvironmentBindingResult::Admitted(authorization)))
            }
            Self::Persist(receipt) => {
                // Pure phase/source/lease/Resource law is evaluated once before
                // CAS. Its exact Fenced late receipt join remains authoritative.
                let changed = session
                    .environment()
                    .try_apply_environment_receipt(owner, receipt, now)
                    .map_err(RepositoryEnvironmentBindingSink::receipt_error)?;
                let state =
                    RepositoryEnvironmentBindingSink::committed_authority(session, receipt)?;
                Ok((changed, EnvironmentBindingResult::Bound(Box::new(state))))
            }
        }
    }

    fn readback(
        &self,
        committed: PersistedSession,
        owner: &str,
        now: u64,
    ) -> Result<EnvironmentBindingResult, RunError> {
        match self {
            Self::Admit(_) => {
                let mut aggregate = crate::mutation::aggregate_from_snapshot(committed)
                    .map_err(session_repository_error)?;
                let (changed, original) = self.apply(&mut aggregate, owner, now)?;
                if changed {
                    return Err(RunError::unavailable_classified(
                        "session_environment_admission_unconfirmed",
                        "first Environment admission is not Store-confirmed",
                    ));
                }
                Ok(original)
            }
            Self::Persist(receipt) => {
                // Do not re-decide the phase after a successful CAS: terminal
                // cleanup may have advanced while the exact binding is read.
                RepositoryEnvironmentBindingSink::committed_authority(&committed, receipt)
                    .map(|state| EnvironmentBindingResult::Bound(Box::new(state)))
            }
        }
    }

    fn contended(&self) -> RunError {
        match self {
            Self::Admit(_) => RunError::unavailable_classified(
                "session_environment_admission_cas_contended",
                "Session Environment admission CAS exhausted",
            ),
            Self::Persist(_) => RunError::unavailable_classified(
                "session_environment_binding_cas_contended",
                "Session environment binding CAS exhausted",
            ),
        }
    }

    fn idempotency_mismatch(&self) -> RunError {
        RunError::internal(match self {
            Self::Admit(_) => "Session Environment admission idempotency mismatch",
            Self::Persist(_) => "Session environment binding idempotency mismatch",
        })
    }
}

impl RepositoryEnvironmentBindingSink {
    async fn execute(
        &self,
        command: EnvironmentBindingCommand<'_>,
    ) -> Result<EnvironmentBindingResult, RunError> {
        let session_id = command.session_id();
        // Admission keeps its original owner lookup/Unowned classification;
        // Persist rechecks the repository owner on every attempt as before.
        let admission_owner = if let EnvironmentBindingCommand::Admit(intent) = &command {
            intent.verify().map_err(Self::receipt_error)?;
            match self.repo.owner(session_id).await {
                Ok(owner) => Some(owner),
                Err(SessionRepositoryError::NotFound) => {
                    if intent.realization().is_some() {
                        return Err(RunError::unavailable_classified(
                            "session_environment_root_missing",
                            "owned Environment effect lost its Session root",
                        ));
                    }
                    return Ok(EnvironmentBindingResult::Admitted(
                        SessionEnvironmentEffectAuthorization::Unowned {
                            intent: Box::new((*intent).clone()),
                        },
                    ));
                }
                Err(error) => return Err(session_repository_error(error)),
            }
        } else {
            None
        };
        const CAS_ATTEMPTS: usize = 3;
        for attempt in 0..CAS_ATTEMPTS {
            let owner = match &admission_owner {
                Some(owner) => owner.clone(),
                None => self
                    .repo
                    .owner(session_id)
                    .await
                    .map_err(session_repository_error)?,
            };
            let mut session = self
                .repo
                .get(session_id)
                .await
                .and_then(crate::mutation::aggregate_from_snapshot)
                .map_err(session_repository_error)?;
            let now = self.clock.now_ms();
            let (changed, outcome) = command.apply(&mut session, &owner, now)?;
            if !changed {
                return Ok(outcome);
            }
            let idempotency = crate::mutation::compile_session_snapshot_idempotency(
                &session,
                command.operation(),
            );
            match crate::mutation::commit_observed_snapshot(
                self.repo.as_ref(),
                &owner,
                &mut session,
                idempotency,
                Vec::new(),
                now,
            )
            .await
            {
                Ok(
                    SessionMutationResult::Applied { .. } | SessionMutationResult::Replayed { .. },
                ) => {
                    let committed = self
                        .repo
                        .get(session_id)
                        .await
                        .map_err(session_repository_error)?;
                    return command.readback(committed, &owner, self.clock.now_ms());
                }
                Ok(SessionMutationResult::Conflict { .. }) if attempt + 1 < CAS_ATTEMPTS => {}
                Ok(SessionMutationResult::Conflict { .. }) => break,
                Ok(SessionMutationResult::IdempotencyMismatch) => {
                    return Err(command.idempotency_mismatch());
                }
                Err(error) => {
                    // Only admission historically recovers a lost response via
                    // exact authorized readback; Persist retains its old error.
                    if matches!(command, EnvironmentBindingCommand::Admit(_))
                        && let Ok(committed) = self.repo.get(session_id).await
                        && let Ok(original) =
                            command.readback(committed, &owner, self.clock.now_ms())
                    {
                        return Ok(original);
                    }
                    return Err(session_repository_error(error));
                }
            }
        }
        Err(command.contended())
    }
}

#[async_trait::async_trait]
impl SessionEnvironmentBindingSink for RepositoryEnvironmentBindingSink {
    async fn authorize(
        &self,
        intent: &SessionEnvironmentEffectIntent,
    ) -> Result<SessionEnvironmentEffectAuthorization, RunError> {
        match self
            .execute(EnvironmentBindingCommand::Admit(intent))
            .await?
        {
            EnvironmentBindingResult::Admitted(authorization) => Ok(authorization),
            EnvironmentBindingResult::Bound(_) => unreachable!("Admit has no binding result"),
        }
    }

    async fn persist(
        &self,
        receipt: SessionEnvironmentReceipt,
    ) -> Result<SessionEnvironmentState, RunError> {
        match self
            .execute(EnvironmentBindingCommand::Persist(&receipt))
            .await?
        {
            EnvironmentBindingResult::Bound(state) => Ok(*state),
            EnvironmentBindingResult::Admitted(_) => {
                unreachable!("Persist has no admission result")
            }
        }
    }
}
