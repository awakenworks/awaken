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
    SessionRepositoryError,
};
use awaken_time::WallClockExt;
/// Invoke the Runtime's pure provider-effective Sandbox-layout gate after the
/// placement's idle-retention check. It performs no durable mutation or
/// external realization and is the one seam used before create, live
/// Resource and final root persistence boundaries.
pub(crate) fn validate_session_sandbox_layout(
    placement: &awaken_session_execution_contract::SessionPlacementRoles,
    session_id: &str,
    layout: &awaken_session_configuration_contract::SessionSandboxLayout,
) -> Result<(), RunError> {
    validate_environment_continuation_topology(
        awaken_session_state_contract::SessionPlacementClass::new(
            layout.runtime_placement,
            layout.environment.self_hosted,
        ),
        &layout.environment.idle_retention,
    )?;
    placement.validate_session_sandbox_layout(session_id, layout)
}

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
    use super::{SessionExecutionPlacement, runtime_placement_for_topology};
    use awaken_session_state_contract::SessionPlacementClass;
    use awaken_session_types::SessionRuntimePlacement;

    /// @obligation runtime.backend_owned_environment_is_materialized_only_by_the_claimed_worker
    /// @obligation session_environment.owner_topology_is_exact_and_frozen
    /// @obligation session_run.execution_requires_dispatch_work_and_realization_owner
    #[kani::proof]
    fn environment_owner_topology_is_exact_and_frozen() {
        // Build partition: cfg(kani) without test-support reads the defining
        // type owner directly; ambient unit-test imports are not a proof input.
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
/// fn selected_role(
///     application: &SessionApplication,
/// ) -> std::sync::Arc<dyn DeploymentSessionEnvironment> {
///     application.creation_commands()
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
/// fn selected_role(application: &SessionApplication) -> std::sync::Arc<dyn SessionEventBatches> {
///     application.event_batches()
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
/// fn selected(application: &SessionApplication) -> std::sync::Arc<dyn SessionObservation> {
///     application.observations()
/// }
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
/// async fn existing_role(outcomes: &dyn OutcomeRuntime, executor: &dyn awaken_session_execution_contract::PreparedSessionRunAdmission) {
///     let _ = outcomes.define_outcome("thread", "goal", "rubric", 3, executor).await;
///     let _ = outcomes.continue_outcome("thread", executor).await;
/// }
/// ```
///
pub struct SessionApplication {
    clock: Arc<dyn awaken_time::WallClock>,
    configuration: SessionApplicationConfiguration,
    credentials: Arc<crate::SessionCredentials>,
    work_assignments: Arc<crate::SessionWorkAssignments>,
    worker_occupancy: Arc<crate::SessionWorkerOccupancy>,
    repository_participants: Arc<crate::SessionRepositoryParticipants>,
    mutations: Arc<crate::SessionMutations>,
    lifecycle: Arc<crate::SessionLifecycle>,
    queries: Arc<crate::SessionQueries>,
    inputs: Arc<crate::SessionInputs>,
    resource_effect_authorization: Arc<crate::SessionResourceEffectAuthorization>,
    environment_binding_sink: Arc<dyn SessionEnvironmentBindingSink>,
    realizations: Arc<crate::SessionRealizations>,
    terminal_cleanups: Arc<crate::SessionTerminalCleanups>,
    continuations: Arc<crate::SessionContinuations>,
    activities: Arc<crate::SessionActivities>,
    termination_commands: Arc<crate::SessionTerminationCommands>,
    update_commands: Arc<crate::SessionUpdateCommands>,
    resource_reconciliation: Arc<crate::SessionResourceReconciliation>,
    resource_manifests: Arc<crate::SessionResourceManifests>,
    creation_commands: Arc<crate::SessionCreationCommands>,
    run_admissions: Arc<crate::SessionRunAdmissions>,
    observations: Arc<crate::SessionObservations>,
    coordination: Arc<crate::SessionCoordination>,
    event_batches: Arc<crate::SessionEventBatchCommands>,
    outcome_reconciliation: Arc<crate::SessionOutcomeReconciliation>,
    /// One complete set of immutable execution observations.
    execution_admission: Arc<SessionExecutionAdmission>,
    runtime_incarnation: String,
    recovery: Arc<crate::SessionRecovery>,
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

    #[cfg(test)]
    #[must_use]
    pub(crate) fn session_repository(&self) -> &dyn SessionRepository {
        self.mutations.repository().as_ref()
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

    #[must_use]
    pub fn worker_occupancy(&self) -> Arc<crate::SessionWorkerOccupancy> {
        self.worker_occupancy.clone()
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

    #[must_use]
    pub fn resource_reconciliation(&self) -> Arc<crate::SessionResourceReconciliation> {
        self.resource_reconciliation.clone()
    }

    #[must_use]
    pub fn resource_manifests(&self) -> Arc<crate::SessionResourceManifests> {
        self.resource_manifests.clone()
    }

    #[must_use]
    pub fn repository_participants(&self) -> Arc<crate::SessionRepositoryParticipants> {
        self.repository_participants.clone()
    }

    #[must_use]
    pub fn creation_commands(&self) -> Arc<crate::SessionCreationCommands> {
        self.creation_commands.clone()
    }

    #[must_use]
    pub fn run_admissions(&self) -> Arc<crate::SessionRunAdmissions> {
        self.run_admissions.clone()
    }

    #[must_use]
    pub fn observations(&self) -> Arc<crate::SessionObservations> {
        self.observations.clone()
    }

    #[must_use]
    pub fn coordination(&self) -> Arc<crate::SessionCoordination> {
        self.coordination.clone()
    }

    #[must_use]
    pub fn event_batches(&self) -> Arc<crate::SessionEventBatchCommands> {
        self.event_batches.clone()
    }

    #[must_use]
    pub fn recovery(&self) -> Arc<crate::SessionRecovery> {
        self.recovery.clone()
    }

    #[must_use]
    pub fn outcome_reconciliation(&self) -> Arc<crate::SessionOutcomeReconciliation> {
        self.outcome_reconciliation.clone()
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
