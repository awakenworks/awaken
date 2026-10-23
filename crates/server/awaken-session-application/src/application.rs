// @formal entrypoint executable_projection.failed_refresh_never_enters_handler
// @formal entrypoint model_route.session_override_reuses_only_exact_identity_and_resolves_every_mismatch
// @formal entrypoint runtime.backend_owned_environment_is_materialized_only_by_the_claimed_worker
// @formal entrypoint session_creation.selected_environment_is_frozen_in_baseline
// @formal entrypoint session_environment.owner_topology_is_exact_and_frozen
// @formal entrypoint session_environment.terminal_fence_admits_only_inflight_binding_receipts
// @formal entrypoint session_run.execution_requires_dispatch_work_and_realization_owner
// @formal entrypoint session_time.root_observations_are_revision_bound_and_replay_stable
use crate::mutation::ROOT_CAS_ATTEMPTS;
use awaken_session_application_contract::process_local_executable_projection_admission;
use awaken_session_state_contract::{
    SessionEnvironmentEffectAuthorization, SessionEnvironmentEffectIntent,
    SessionEnvironmentReceipt, SessionEnvironmentReceiptError,
    SessionEnvironmentReceiptRecoveryAction, SessionEnvironmentState, SessionMutationResult,
    SessionRepositoryError, SessionTerminalCleanupAction,
};
use awaken_time::WallClockExt;
use awaken_tool_contract::ToolConfigurationCompiler;
/// Reject a layout whose idle policy its placement cannot honour
/// ([`awaken_session_state_contract::SessionPlacementClass::supports_idle_retention`]).
fn validate_environment_continuation_topology(
    placement: awaken_session_state_contract::SessionPlacementClass,
    idle_retention: &awaken_session_configuration_contract::EnvironmentIdleRetentionPolicy,
) -> Result<(), RunError> {
    if !placement.supports_idle_retention(idle_retention) {
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

/// The process's executable-projection refresh, or process-local admission
/// when the authoritative catalogs live in this process. Every component that
/// enters a lifecycle admission boundary shares this one selection.
#[derive(Clone)]
pub(crate) struct ExecutableProjections(
    pub(crate) Option<Arc<awaken_session_application_contract::ExecutableProjectionRefresh>>,
);

impl ExecutableProjections {
    pub(crate) async fn refresh(
        &self,
    ) -> Result<awaken_session_application_contract::ExecutableProjectionAdmission, String> {
        match &self.0 {
            Some(refresh) => refresh.refresh().await,
            None => Ok(process_local_executable_projection_admission()),
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
///
/// Role-forwarding source capability table:
/// W1: absent/available/unavailable selected Agent source keeps the original
/// Queries boolean. W2: Runtime capability Ok(true), Ok(false), or Err(error)
/// reaches the Events role unchanged. W3: Managed keeps size -> capability ->
/// pending-reply refusal order; neither removed wrapper owned a decision or CAS.
/// The original system-message admission table and Runtime poisoned-read tests
/// own runtime behavior. These independent examples prove source capability
/// only; they do not execute that runtime matrix or authorize Session effects.
///
/// W4: the old deployment query inherent entry is absent independently.
///
/// ```compile_fail,E0599
/// use awaken_session_application::SessionApplication;
/// let _ = SessionApplication::deployment_agent_unavailable;
/// ```
///
/// W5: the old Runtime capability inherent entry is independently absent.
///
/// ```compile_fail,E0599
/// use awaken_session_application::SessionApplication;
/// let _ = SessionApplication::supports_mid_conversation_system;
/// ```
///
/// W6: the canonical Query owner and original Deployment role remain available.
///
/// ```
/// use awaken_session_application::{
///     DeploymentSessionEnvironment, SessionApplication, SessionQueries,
/// };
/// fn existing_query(queries: &SessionQueries) -> bool {
///     queries.agent_unavailable("workspace", "agent")
/// }
/// fn selected_role(application: &SessionApplication) -> &dyn DeploymentSessionEnvironment {
///     application
/// }
/// ```
///
/// W7: the canonical Streaming role and original Events role remain available.
///
/// ```
/// use awaken_session_application::{SessionApplication, SessionEventBatches};
/// use awaken_session_execution_contract::StreamingRunRuntime;
/// use awaken_session_types::RunError;
/// async fn existing_policy(runs: &dyn StreamingRunRuntime) -> Result<bool, RunError> {
///     runs.supports_mid_conversation_system("thread").await
/// }
/// fn selected_role(application: &SessionApplication) -> &dyn SessionEventBatches {
///     application
/// }
/// ```
///
/// W8-W10 source-capability table: each removed inherent forwarding entry is
/// independently absent; its original narrow role still accepts the same
/// arguments and result type. Runtime lookup/error/scope behavior belongs to
/// the D/L native cases, not these compile-only examples. No complete Runtime
/// write role or new Environment/profile source is exposed to the protocol.
///
/// W8a: the old inherent Environment lookup is absent without importing a role.
///
/// ```compile_fail,E0599
/// use awaken_session_application::SessionApplication;
/// let _ = SessionApplication::deployment_environment;
/// ```
///
/// W8b: the existing narrow Deployment role preserves the typed lookup result.
///
/// ```
/// use awaken_session_application::DeploymentSessionEnvironment;
/// async fn lookup(role: &dyn DeploymentSessionEnvironment)
///     -> Result<Option<awaken_environment_contract::EnvItem>, awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError>
/// {
///     role.deployment_environment("environment").await
/// }
/// ```
///
/// W9a: delegate availability has no second inherent entry.
///
/// ```compile_fail,E0599
/// use awaken_session_application::SessionApplication;
/// let _ = SessionApplication::deployment_unavailable_delegate;
/// ```
///
/// W9b: the same Deployment role keeps its original optional delegate answer.
///
/// ```
/// use awaken_session_application::DeploymentSessionEnvironment;
/// fn delegate(role: &dyn DeploymentSessionEnvironment) -> Option<String> {
///     role.deployment_unavailable_delegate("workspace", "agent")
/// }
/// ```
///
/// W10a: live observation has no second inherent entry.
///
/// ```compile_fail,E0599
/// use awaken_session_application::SessionApplication;
/// let _ = SessionApplication::subscribe_session_thread_live;
/// ```
///
/// W10b: the existing read-only role keeps its selected Runtime subscription.
///
/// ```
/// use awaken_session_application::{SessionApplication, SessionObservation};
/// use awaken_session_execution_contract::SessionThreadLiveSubscription;
/// use awaken_session_types::{RunError, SessionId};
/// fn selected(application: &SessionApplication) -> &dyn SessionObservation { application }
/// async fn live(role: &dyn SessionObservation, session: &SessionId)
///     -> Result<Option<Box<dyn SessionThreadLiveSubscription>>, RunError>
/// {
///     role.subscribe_session_thread_live(session, session.as_str()).await
/// }
/// ```
///
/// Construction capability table: a selected Store/Clock enters the existing
/// persistence value before construction; a live application has no Store
/// replacement operation. These examples prove source capability only; the
/// original constructor/binding tests exercise the actual selected ports.
///
/// ```
/// use std::sync::Arc;
/// use awaken_session_application::SessionPersistence;
/// use awaken_session_repository_contract::SessionStore;
/// fn select(store: Arc<dyn SessionStore>, clock: Arc<dyn awaken_time::WallClock>)
///     -> SessionPersistence
/// {
///     SessionPersistence::new(store, clock)
/// }
/// ```
///
/// ```compile_fail,E0599
/// use std::sync::Arc;
/// use awaken_session_application::SessionApplication;
/// use awaken_session_repository_contract::SessionStore;
/// fn replace_live(application: &mut SessionApplication, store: Arc<dyn SessionStore>) {
///     application.replace_repository(store);
/// }
/// ```
///
/// Runtime capability table: Runtime roles remain the command/read owners.
/// Z1a-Z1j: each old App entry is independently absent (E0599); one failure
/// cannot mask a different restored facade. Z2a-Z2e: the existing narrow roles
/// still expose the same operations. These examples prove source capability
/// only; original Runtime/Session tests own effects, admission, and persistence.
///
/// Z1a: the redundant `SessionApplication::coordinated_threads` entry is absent.
///
/// ```compile_fail,E0599
/// use awaken_session_application::SessionApplication;
/// let _ = SessionApplication::coordinated_threads;
/// ```
///
/// Z1b: the redundant `SessionApplication::session_thread_disposition` entry is absent.
///
/// ```compile_fail,E0599
/// use awaken_session_application::SessionApplication;
/// let _ = SessionApplication::session_thread_disposition;
/// ```
///
/// Z1c: the redundant `SessionApplication::pending_tool` entry is absent.
///
/// ```compile_fail,E0599
/// use awaken_session_application::SessionApplication;
/// let _ = SessionApplication::pending_tool;
/// ```
///
/// Z1d: the redundant `SessionApplication::rebind_model` entry is absent.
///
/// ```compile_fail,E0599
/// use awaken_session_application::SessionApplication;
/// let _ = SessionApplication::rebind_model;
/// ```
///
/// Z1e: the redundant `SessionApplication::run_streaming_attributed` entry is absent.
///
/// ```compile_fail,E0599
/// use awaken_session_application::SessionApplication;
/// let _ = SessionApplication::run_streaming_attributed;
/// ```
///
/// Z1f: the redundant `SessionApplication::resume` entry is absent.
///
/// ```compile_fail,E0599
/// use awaken_session_application::SessionApplication;
/// let _ = SessionApplication::resume;
/// ```
///
/// Z1g: the redundant `SessionApplication::resume_custom` entry is absent.
///
/// ```compile_fail,E0599
/// use awaken_session_application::SessionApplication;
/// let _ = SessionApplication::resume_custom;
/// ```
///
/// Z1h: the redundant `SessionApplication::define_outcome` entry is absent.
///
/// ```compile_fail,E0599
/// use awaken_session_application::SessionApplication;
/// let _ = SessionApplication::define_outcome;
/// ```
///
/// Z1i: the redundant `SessionApplication::continue_outcome` entry is absent.
///
/// ```compile_fail,E0599
/// use awaken_session_application::SessionApplication;
/// let _ = SessionApplication::continue_outcome;
/// ```
///
/// Z1j: the redundant `SessionApplication::committed_outcome_projection` entry is absent.
///
/// ```compile_fail,E0599
/// use awaken_session_application::SessionApplication;
/// let _ = SessionApplication::committed_outcome_projection;
/// ```
///
/// Z2a: coordinated links remain on the original read role.
///
/// ```
/// use awaken_session_execution_contract::SessionCoordinationReads;
/// use awaken_session_types::SessionId;
/// async fn existing_role(reads: &dyn SessionCoordinationReads, session: &SessionId) {
///     let _ = reads.coordinated_threads(session, None).await;
/// }
/// ```
///
/// Z2b: Thread disposition and exact Outcome reads keep their original owner.
///
/// ```
/// use awaken_session_execution_contract::ThreadReadRuntime;
/// use awaken_session_types::SessionId;
/// async fn existing_role(reads: &dyn ThreadReadRuntime, session: &SessionId) {
///     let _ = reads.session_thread_disposition(session, "thread").await;
///     let _ = reads.committed_outcome_projection("thread", "outcome").await;
/// }
/// ```
///
/// Z2c: streaming operations remain on the original execution role.
///
/// ```
/// use std::sync::Arc;
/// use awaken_agent_contract::stream::sink::Sink;
/// use awaken_session_execution_contract::StreamingRunRuntime;
/// async fn existing_role(runs: &dyn StreamingRunRuntime, sink: Arc<dyn Sink>) {
///     let _ = runs.pending_tool("thread").await;
///     let _ = runs.rebind_model("thread", "model").await;
///     let _ = runs
///         .run_streaming_attributed("agent", "thread", Vec::new(), None, sink)
///         .await;
/// }
/// ```
///
/// Z2d: ticket results remain on the original Runtime role.
///
/// ```
/// use awaken_session_execution_contract::{SessionRuntime, ToolPermissionDecision};
/// async fn existing_role(runs: &dyn SessionRuntime, decision: ToolPermissionDecision) {
///     let _ = runs.resume("thread", "call", decision).await;
///     let _ = runs.resume_custom("thread", "call", Vec::new(), false).await;
/// }
/// ```
///
/// Z2e: Outcome commands remain on the original Outcome role.
///
/// ```
/// use awaken_session_execution_contract::OutcomeRuntime;
/// async fn existing_role(outcomes: &dyn OutcomeRuntime) {
///     let _ = outcomes.define_outcome("thread", "goal", "rubric", 3).await;
///     let _ = outcomes.continue_outcome("thread").await;
/// }
/// ```
///
pub struct SessionApplication {
    clock: Arc<dyn awaken_time::WallClock>,
    configuration: SessionApplicationConfiguration,
    runtime: crate::SessionExecutor,
    tool_configuration_compiler: Arc<dyn ToolConfigurationCompiler>,
    credentials: Arc<crate::SessionCredentials>,
    environments: SessionEnvironments,
    model_publication_resolver:
        Option<Arc<dyn awaken_session_configuration_contract::SessionModelPublicationResolver>>,
    resource_registry: Option<Arc<dyn awaken_resource_contract::ResourceRegistry>>,
    resource_references: Arc<crate::SessionResourceReferences>,
    work_assignments: Arc<crate::SessionWorkAssignments>,
    repository_participants: Arc<crate::SessionRepositoryParticipants>,
    resource_files: Option<Arc<dyn awaken_resource_contract::FileCatalog>>,
    managed_list_prices: Option<Arc<dyn awaken_session_budget_contract::ManagedListPriceProvider>>,
    mutations: Arc<crate::SessionMutations>,
    lifecycle: Arc<crate::SessionLifecycle>,
    queries: Arc<crate::SessionQueries>,
    inputs: Arc<crate::SessionInputs>,
    resource_effect_authorization: Arc<crate::SessionResourceEffectAuthorization>,
    environment_binding_sink: Arc<dyn SessionEnvironmentBindingSink>,
    /// The one process-owned high-water reconciler, invoked at lifecycle
    /// admission boundaries. Query reads never drive it.
    executable_projections: ExecutableProjections,
    realizations: Arc<crate::SessionRealizations>,
    terminal_cleanups: Arc<crate::SessionTerminalCleanups>,
    continuations: Arc<crate::SessionContinuations>,
    activities: Arc<crate::SessionActivities>,
    termination_commands: Arc<crate::SessionTerminationCommands>,
    update_commands: Arc<crate::SessionUpdateCommands>,
    /// One complete set of immutable execution observations.
    execution_admission: Arc<SessionExecutionAdmission>,
    runtime_incarnation: String,
    event_batch_cutover_validation: Arc<SessionEventBatchCutoverValidationSource>,
    /// The composition inputs a fixture may still change before sharing.
    #[cfg(any(test, feature = "test-support"))]
    store: Arc<dyn awaken_session_repository_contract::SessionStore>,
    #[cfg(any(test, feature = "test-support"))]
    fixture_assembly: crate::construction::SessionAssembly,
}

static APPLICATION_INCARNATION_SEQ: AtomicU64 = AtomicU64::new(0);

impl SessionApplication {
    /// The canonical root CAS component shared by all Session use cases.
    /// This is a constructed component, not a discovered optional capability.
    pub fn mutations(&self) -> &crate::SessionMutations {
        &self.mutations
    }

    /// The independently held exact credential command owner.
    #[must_use]
    pub fn credentials(&self) -> Arc<crate::SessionCredentials> {
        self.credentials.clone()
    }

    /// The independently held Worker Resource-effect authorization owner.
    #[must_use]
    pub fn resource_effect_authorization(&self) -> Arc<crate::SessionResourceEffectAuthorization> {
        self.resource_effect_authorization.clone()
    }

    /// The independently held lifecycle command and signal owner.
    #[must_use]
    pub fn lifecycle(&self) -> Arc<crate::SessionLifecycle> {
        self.lifecycle.clone()
    }

    /// The injected clock port, for an adapter that stamps observations on the
    /// same time line as this application's commands.
    #[must_use]
    pub fn clock(&self) -> Arc<dyn awaken_time::WallClock> {
        self.clock.clone()
    }

    /// Exact immutable fact written into a newly compiled Session baseline.
    #[must_use]
    pub fn runtime_placement(&self) -> SessionRuntimePlacement {
        runtime_placement_for_topology(self.configuration.execution_placement)
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
        let effects = self.runtime().realization_effects();

        validate_environment_continuation_topology(
            awaken_session_state_contract::SessionPlacementClass::new(
                layout.runtime_placement,
                layout.environment.self_hosted,
            ),
            &layout.environment.idle_retention,
        )?;
        effects.validate_session_sandbox_layout(session_id, layout)
    }

    #[must_use]
    pub(crate) fn session_repository(&self) -> &dyn SessionRepository {
        self.mutations.repository().as_ref()
    }

    /// One complete Worker-owned Session read. Normal occupancy never depends
    /// on provider availability. Explicit retirement preserves occupancy and
    /// additionally qualifies each original source (terminal or live Resident) without effects,
    /// lease changes, installed projections or cleanup receipts.
    pub async fn observe_worker_realizations(
        &self,
        read: crate::WorkerRealizationRead<'_>,
    ) -> Result<crate::WorkerRealizationObservation, RunError> {
        let (worker_id, identity) = match read {
            crate::WorkerRealizationRead::Occupancy(worker_id) => (worker_id, None),
            crate::WorkerRealizationRead::Retirement(identity) => {
                (identity.worker_id.as_str(), Some(identity))
            }
        };
        if worker_id.trim().is_empty() {
            return Err(RunError::bad_request("Worker realization owner is empty"));
        }
        if identity.is_some_and(|identity| {
            identity.incarnation_id.trim().is_empty() || identity.generation == 0
        }) {
            return Err(RunError::bad_request(
                "exact Worker realization identity is required",
            ));
        }
        let sessions =
            crate::recovery_scan::worker_realizations(self.session_repository(), worker_id)
                .await
                .map_err(session_repository_error)?;
        let mut occupied = false;
        let blocked = crate::WorkerRealizationObservation {
            occupied: true,
            sources_retained: false,
        };
        for scoped in sessions {
            let session = scoped.session;
            if !session
                .has_unfinished_worker_realization()
                .map_err(|error| RunError::unavailable(error.to_string()))?
            {
                continue;
            }
            occupied = true;
            let Some(identity) = identity else {
                return Ok(blocked);
            };
            let lease = session
                .realization
                .as_ref()
                .ok_or_else(|| RunError::unavailable("Worker realization owner missing"))?;
            if lease.runtime_incarnation != identity.lease_owner()
                || session.has_incomplete_event_batches()
            {
                return Ok(blocked);
            }
            let prepared = if session.is_terminal() {
                let Some(action) = session
                    .terminal_cleanup_work_action()
                    .map_err(|error| RunError::unavailable(error.to_string()))?
                else {
                    return Ok(blocked);
                };
                if matches!(action, SessionTerminalCleanupAction::Waiting) {
                    return Ok(blocked);
                }
                if awaken_session_state_contract::session_has_terminal_no_create_evidence(&session)
                    || crate::archive_intent::unattempted_profiled_archive_witness(
                        self.session_repository(),
                        &scoped.workspace_id,
                        &session,
                    )
                    .await
                    .map_err(session_repository_error)?
                {
                    continue;
                }
                if !matches!(
                    self.runtime()
                        .coordination_reads()
                        .source_dispatch_observation(&session)
                        .await,
                    awaken_session_execution_contract::SessionSourceDispatchObservation::Settled
                ) {
                    return Ok(blocked);
                }
                matches!(action, SessionTerminalCleanupAction::Dispose { .. })
            } else {
                // Data retention permits Actor replacement, never completion
                // of this live Session or its original Run. Registry/physical/
                // Work admission remains the Worker-domain owner's prerequisite.
                // is_terminal already covers every disposition denying activity.
                if session.frozen_baseline().is_none()
                    || !matches!(
                        session.execution,
                        SessionExecutionState::Idle
                            | SessionExecutionState::Running
                            | SessionExecutionState::Rescheduling
                    )
                    || !matches!(
                        session.environment,
                        SessionEnvironmentState::Resident { .. }
                    )
                {
                    return Ok(blocked);
                }
                false
            };
            let projection = self
                .queries
                .frozen_session_projection(scoped.workspace_id, &session, false)
                .await?;
            if !self
                .runtime()
                .terminal_cleanup()
                .observe_terminal_cleanup_retention(&session.session_id, &projection, prepared)
                .await?
            {
                return Ok(blocked);
            }
        }
        Ok(crate::WorkerRealizationObservation {
            occupied,
            sources_retained: identity.is_some(),
        })
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
        self.mutations.repository().clone()
    }

    /// The independently constructed read service, shared with protocol roles.
    #[must_use]
    pub fn queries(&self) -> Arc<crate::SessionQueries> {
        self.queries.clone()
    }

    /// The selected input use-case service, shared directly with adapters.
    #[must_use]
    pub fn inputs(&self) -> Arc<crate::SessionInputs> {
        self.inputs.clone()
    }

    #[must_use]
    pub fn runtime_incarnation(&self) -> &str {
        &self.runtime_incarnation
    }

    /// Stable logical identity paired with the process-unique Runtime
    /// incarnation in Session realization leases.
    #[must_use]
    pub fn local_realization_owner(&self) -> &str {
        &self.configuration.local_realization_owner
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
                .projection
                .resolve_exact_for_session(
                    &binding.environment_id,
                    binding.revision,
                    published_backend_ref,
                    mcp_targets,
                )
                .await
        } else {
            self.environments
                .projection
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

    #[must_use]
    pub fn work_assignments(&self) -> Arc<crate::SessionWorkAssignments> {
        self.work_assignments.clone()
    }

    pub fn environment_binding_sink(&self) -> Arc<dyn SessionEnvironmentBindingSink> {
        self.environment_binding_sink.clone()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn replace_environment_source(&mut self, source: impl Into<SessionEnvironments>) {
        let environments = source.into();
        crate::construction::reassemble(self, |assembly| assembly.environments = environments);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_managed_list_price_provider(
        &mut self,
        provider: Arc<dyn awaken_session_budget_contract::ManagedListPriceProvider>,
    ) {
        crate::construction::reassemble(self, |assembly| assembly.prices = Some(provider));
    }

    /// The frozen dispatch, Worker roster and placement compiler for this process.
    pub fn execution_admission(&self) -> &SessionExecutionAdmission {
        &self.execution_admission
    }

    /// Bind the one executable-projection reconciliation operation used by
    /// request and autonomous Session boundaries. Duplicate installation is an
    /// error instead of allowing two freshness authorities to overlap.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_executable_projection_refresh(
        &mut self,
        refresh: Arc<awaken_session_application_contract::ExecutableProjectionRefresh>,
    ) -> Result<(), &'static str> {
        if self.fixture_assembly.refresh.is_some() {
            return Err("executable projection refresh is already installed");
        }
        crate::construction::reassemble(self, |assembly| assembly.refresh = Some(refresh));
        Ok(())
    }

    /// Advance the injected executable projections, or no-op in an AllInOne
    /// composition where the authoritative catalogs are process-local.
    pub async fn refresh_executable_projections(
        &self,
    ) -> Result<awaken_session_application_contract::ExecutableProjectionAdmission, String> {
        self.executable_projections.refresh().await
    }

    #[must_use]
    pub fn realizations(&self) -> Arc<crate::SessionRealizations> {
        self.realizations.clone()
    }

    #[must_use]
    pub fn terminal_cleanups(&self) -> Arc<crate::SessionTerminalCleanups> {
        self.terminal_cleanups.clone()
    }

    #[must_use]
    pub fn continuations(&self) -> Arc<crate::SessionContinuations> {
        self.continuations.clone()
    }

    #[must_use]
    pub fn activities(&self) -> Arc<crate::SessionActivities> {
        self.activities.clone()
    }

    #[must_use]
    pub fn termination_commands(&self) -> Arc<crate::SessionTerminationCommands> {
        self.termination_commands.clone()
    }

    #[must_use]
    pub fn update_commands(&self) -> Arc<crate::SessionUpdateCommands> {
        self.update_commands.clone()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_credential_source(
        &mut self,
        source: Arc<dyn awaken_credential_contract::SessionCredentialSource>,
    ) {
        crate::construction::reassemble(self, |assembly| assembly.credential_source = Some(source));
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_credential_material_ingress(
        &mut self,
        ingress: Arc<dyn awaken_credential_contract::CredentialMaterialIngress>,
    ) {
        crate::construction::reassemble(self, |assembly| {
            assembly.credential_material_ingress = Some(ingress)
        });
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_resource_purge_scheduler(
        &mut self,
        scheduler: Arc<dyn awaken_resource_contract::ResourcePurgeScheduler>,
    ) {
        crate::construction::reassemble(self, |assembly| {
            assembly.resource_purge_scheduler = Some(scheduler)
        });
    }

    /// Install the Resource authorities used to publish the Session's durable
    /// active+pending retention set. These authorities are paired so logical
    /// File ids and physical blob identities can never be sourced from
    /// different Resource components.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_resource_reference_authority(
        &mut self,
        references: Arc<dyn awaken_resource_contract::ResourceReferenceIndex>,
        files: Arc<dyn awaken_resource_contract::FileCatalog>,
    ) {
        crate::construction::reassemble(self, |assembly| {
            assembly.resource_references = Some(references);
            assembly.resource_files = Some(files);
        });
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_config_source(
        &mut self,
        source: Arc<dyn awaken_executable_agent_contract::ExecutableAgentProfileSource>,
    ) {
        crate::construction::reassemble(self, |assembly| assembly.profiles = Some(source));
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_model_publication_resolver(
        &mut self,
        resolver: Arc<dyn awaken_session_configuration_contract::SessionModelPublicationResolver>,
    ) {
        crate::construction::reassemble(self, |assembly| assembly.models = Some(resolver));
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
        let root_snapshot = root_profile
            .filter(|profile| profile.source_revision > 0)
            .map(|profile| {
                self.queries
                    .executable_snapshot_at_revision(
                        workspace_id,
                        root_agent_id,
                        profile.source_revision,
                    )
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
        if !self.queries.has_agent_profile_source() {
            return Err(RunError::bad_request(
                "budget_price_roster_unavailable: Agent publication source is unavailable",
            ));
        }
        for delegate in &root_profile.delegates {
            let agent_id = &delegate.agent_id;
            let revision = delegate.source_revision.ok_or_else(|| {
                RunError::bad_request(format!(
                    "budget_price_roster_unavailable: Agent `{agent_id}` has no exact publication revision"
                ))
            })?;
            let snapshot = self
                .queries
                .executable_snapshot_at_revision(workspace_id, agent_id, revision)
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

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_resource_registry(
        &mut self,
        catalog: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    ) {
        crate::construction::reassemble(self, |assembly| {
            assembly.resource_registry = Some(catalog)
        });
    }
}

fn claim_once(fence: &AtomicBool) -> bool {
    !fence.swap(true, Ordering::AcqRel)
}

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
                        | SessionEnvironmentReceiptError::Rejected(
                            awaken_session_state_contract::SessionEnvironmentRejection::MaterializationPending
                        )
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
                        SessionEnvironmentEffectAuthorization::admit_unowned(intent)
                            .map_err(Self::receipt_error)?,
                    ));
                }
                Err(error) => return Err(session_repository_error(error)),
            }
        } else {
            None
        };
        for attempt in 0..ROOT_CAS_ATTEMPTS {
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
            match crate::SessionMutations::commit_observed_snapshot(
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
                Ok(SessionMutationResult::Conflict { .. }) if attempt + 1 < ROOT_CAS_ATTEMPTS => {}
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
