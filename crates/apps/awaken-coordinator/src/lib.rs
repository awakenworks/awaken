// @formal entrypoint authz.application_protocol_is_scoped_by_application_guard_not_service_guard
// @formal entrypoint executable_projection.failed_refresh_never_enters_handler
//! `awaken-coordinator` — the Coordinator application and protocol data plane.
//!
//! It consumes the Session executor only through the [`ExecutionPorts`] a
//! composition root selects from it, and mounts public protocol adapters over
//! them. Each adapter is a thin translator from its wire
//! vocabulary to the host's neutral operations; because every adapter keys by the
//! same Thread id and drives the same coordinator, a Run started through one
//! protocol can be resumed or observed through another on the *same Thread*.
//!
//! This crate is the Coordinator owner: its canonical
//! [`build_coordinator_component`] assembles Deployment/Session scheduling and
//! the session surface + protocol adapters
//! (`mount` / `mount_with_managed`, under `test-support`), injected exact
//! published-model materialization and executable-directory ports, the inert
//! no-model placeholder, workspace path addressing, and the Worker
//! role helper (the hand is now the separate `awaken-sandbox` execution-plane
//! binary). Its sibling **authoring / authz plane** lives in
//! `awaken-control`; it owns model-catalog publication resolution and neither
//! crate depends on the other. `awaken-cli` is the
//! process entry point that joins them into one management router and the
//! co-located root that builds the Session executor (`awaken-session-executor`)
//! and selects these ports from it; this crate links no executor, realization
//! or Sandbox implementation.

pub mod admin;
pub mod application_access;
pub mod application_access_store;
mod artifact_publication;
mod browser_cors;
pub mod console;
pub mod control_service_boundary;
mod coordinator_component;
mod coordinator_persistence;
pub mod credential_execution;
mod credential_rollout_delivery;
mod session_credential_rollout;
#[cfg(test)]
mod session_credential_rollout_tests;
mod session_dispatch_reads;
mod session_routes;
#[cfg(test)]
mod test_session_fixture;
pub use credential_rollout_delivery::{
    HttpManagedCredentialRolloutTarget, credential_rollout_router_with_authenticator,
};
pub use session_credential_rollout::{
    SessionCredentialRolloutAdoption, SessionProjectionPublication,
};
pub mod data_subject_boundary;
mod execution_ports;
mod extraction_references;
#[cfg(test)]
mod inference_publication_tests;
mod managed_lifecycle;
pub mod mcp_export;
mod org_admission;
pub use managed_lifecycle::{
    ManagedLifecycleCompositionError, PreparedManagedLifecycleDelivery,
    prepare_managed_lifecycle_delivery,
};
mod runtime_authority;
pub mod webhooks;
#[cfg(feature = "container-k8s")]
pub mod worker_create_broker;
pub mod worker_fleet;
mod worker_memory;
pub mod worker_observation_boundary;
pub mod worker_placement;
mod worker_registry;
pub mod workspace_path;

pub use artifact_publication::ClaimFencedArtifactPublisher;
pub use browser_cors::AiSdkBrowserCors;
pub use coordinator_component::{
    CoordinatorBuildError, CoordinatorComponent, CoordinatorDependencies,
    build_coordinator_component, register_application_access_retention,
    restore_deployment_application,
};
#[cfg(any(test, feature = "test-support"))]
pub use coordinator_persistence::open_durable_runtime_authority;
pub use coordinator_persistence::{
    CoordinatorPersistence, migrate_postgres_schema as migrate_postgres_coordinator_schema,
    migrate_postgres_schema_with_pool as migrate_postgres_coordinator_schema_with_pool,
    open as open_coordinator_persistence, open_existing as open_existing_coordinator_persistence,
};
pub use execution_ports::{
    ColocatedExecution, ColocatedExecutionInputs, ExecutionPorts, InferenceSelection,
    PreparedColocatedExecution,
};
pub use extraction_references::ReferenceIndexedMemoryExtractions;
pub use runtime_authority::postgres_local_commit;

use awaken_protocol_managed::awaken_extensions;
use std::sync::Arc;

mod dream;

use awaken_protocol_managed::{ManagedState, router};
use awaken_runtime_contract::runs::Runs;
use axum::Router;

// This data plane depends on each authoritative owner directly.
pub use awaken_ext_skills::{SkillContext, SkillSpec, parse_skill_md};
pub use awaken_protocol_managed::{ResourcesRouterInput, models_router, resources_router};

pub use awaken_worker_registry::{WorkerDirectory, WorkerObservationSource};
pub use worker_registry::WorkerDirectoryHandle;
#[cfg(any(test, feature = "test-support"))]
pub use worker_registry::test_directory as test_worker_directory;

/// A Scenario composition's Managed adapter together with the one Session
/// application it narrows and the execution ports its root selected. The
/// composition root that built the application hands it to every other
/// consumer (the lifecycle supervisor, Worker transport, Dreams) from here;
/// the adapter holds only its roles.
#[cfg(feature = "test-support")]
#[derive(Clone)]
pub struct ScenarioManagedState {
    pub execution: ExecutionPorts,
    pub managed: Arc<ManagedState>,
    pub sessions: Arc<awaken_session_application::SessionApplication>,
    /// Original Environment source selected by the same scenario composition.
    pub environment_warmups:
        Arc<dyn awaken_session_configuration_contract::EnvironmentWarmupSource>,
    /// The platform-managed Workspace scope-less requests resolve to.
    pub local_workspace: String,
    /// The executor's clock: the Worker transport and Dreams judge time on it.
    pub clock: Arc<dyn awaken_time::WallClock>,
    /// The exact Registry selected before this scenario's Session application.
    pub worker_directory: Arc<dyn awaken_worker_registry::WorkerDirectory>,
}

// The **worker** lifecycle moved to the production `awaken-worker` crate. In the
// current deployment it consumes only the publication-pinned model candidate
// through the injected `CredentialInferenceMaterializer`; its
// `NoModelConfiguredExecutor` is only an inert construction placeholder and is
// never a materialization fallback.
// The `awaken` binary's Worker role delegates to `awaken_worker::run`. The
// test-only echo-draining worker (for the worker-pool e2e) lives in
// `awaken-scenario-host::run_echo_worker`.

/// The scenario data plane over a caller-assembled Managed state: the
/// composition root that built the Session executor selected its execution
/// ports into `managed_state`.
#[cfg(feature = "test-support")]
pub async fn mount_with_managed(
    managed_state: ScenarioManagedState,
    resources: awaken_resource_application::ResourcesApplication,
) -> Router {
    mount_with_managed_over(
        managed_state,
        resources,
        ApplicationAccessMount::ExplicitlyUnguardedTest,
        awaken_service_lifecycle::ServiceLifecycle::new(),
        None,
    )
    .await
    .0
}

#[cfg(feature = "test-support")]
async fn scenario_dream_process_store(
    storage_dir: Option<&std::path::Path>,
) -> Arc<dyn awaken_session_dream_contract::DreamProcessStore> {
    match storage_dir {
        Some(root) => {
            std::fs::create_dir_all(root).expect("create durable Dream repository directory");
            Arc::new(
                awaken_session_store::SqliteManagedSessionRepository::open(
                    &root.join("sessions.db").to_string_lossy(),
                )
                .await
                .expect("open durable Dream process repository"),
            )
        }
        None => Arc::new(awaken_dream_application::InMemoryDreamProcessStore::default()),
    }
}

#[cfg(feature = "test-support")]
async fn with_scenario_session_lifecycle(
    router: Router,
    managed_state: ScenarioManagedState,
    lifecycle: awaken_service_lifecycle::ServiceLifecycle,
    dreams: &Arc<awaken_dream_application::DreamApplication>,
) -> Router {
    prepare_managed_lifecycle_delivery(&managed_state.managed, &managed_state.sessions, None, None)
        .map(|prepared| prepared.start(&lifecycle))
        .expect("scenario Managed lifecycle delivery binds before traffic");
    let dispatch_work = managed_state.execution.dispatch_lifecycle;
    coordinator_component::start_runtime_services(dispatch_work.as_ref(), dreams)
        .await
        .expect("scenario dispatch composition binds before traffic");
    coordinator_component::register_session_lifecycle(&lifecycle, managed_state.sessions);
    coordinator_component::register_runtime_background_drain(&lifecycle, dispatch_work);
    // The Router owns the same process-lifecycle handle as the Scenario surface.
    // Dropping the test server drops the composition; the Tokio runtime then
    // tears down its registered tasks just as process shutdown does.
    router.layer(axum::Extension(lifecycle))
}

#[cfg(feature = "test-support")]
fn with_scenario_worker_transport(
    public: Router,
    worker_private: Router,
    remote_worker_required: bool,
) -> Router {
    // Production keeps the Worker transport on its private listener. The
    // single-listener Scenario surface exposes that exact router only for the
    // coordinator-only deployment axis whose local pool is disabled; ordinary
    // Scenario servers retain the same isolation instead of publishing an
    // unused private API.
    if remote_worker_required {
        public.merge(worker_private)
    } else {
        public
    }
}

/// Assemble the data plane with the same secret-free Resource Registry used by
/// the Managed Session ACL. Authorization remains an outer middleware concern;
/// this only shares resource identity/configuration/lifecycle truth.
#[cfg(feature = "test-support")]
pub async fn mount_with_managed_and_resource_registry(
    managed_state: ScenarioManagedState,
    resources: awaken_resource_application::ResourcesApplication,
) -> Router {
    mount_with_managed_and_resource_registry_and_dreams(managed_state, resources, None)
        .await
        .0
}

/// Test-support decoration of the existing Dream execution port. The
/// Coordinator still constructs the sole production executor and Dream state.
#[cfg(feature = "test-support")]
pub type ScenarioDreamExecutorDecorator = Arc<
    dyn Fn(
            Arc<dyn awaken_dream_application::DreamExecutor>,
        ) -> Arc<dyn awaken_dream_application::DreamExecutor>
        + Send
        + Sync,
>;

/// Scenario/embedder variant that returns the exact Dream aggregate mounted in
/// the data router. Production callers use the fuller application-access variant
/// below; deterministic hosts use this handle to mount the Awaken-only policy
/// projection without constructing a second scheduler or state authority.
///
/// Compile-matrix decision rule: when `test-support` is absent, neither this
/// scenario entry point nor its ephemeral dependencies may enter the product
/// build; when it is present, both must compile as one path. The corresponding
/// gates are exercised by no-feature product builds and test-support scenario
/// builds rather than by a second runtime implementation.
#[cfg(feature = "test-support")]
pub async fn mount_with_managed_and_resource_registry_and_dreams(
    managed_state: ScenarioManagedState,
    resources: awaken_resource_application::ResourcesApplication,
    dream_executor_decorator: Option<ScenarioDreamExecutorDecorator>,
) -> (Router, Arc<awaken_dream_application::DreamApplication>) {
    mount_with_managed_and_resource_registry_and_dreams_on_lifecycle(
        managed_state,
        resources,
        awaken_service_lifecycle::ServiceLifecycle::new(),
        dream_executor_decorator,
    )
    .await
}

/// Scenario composition with an outer-owned lifecycle. Process-level scenario
/// hosts use this entry point so graceful HTTP shutdown can join the exact
/// Session supervisors and Runtime background work mounted in the Router.
#[cfg(feature = "test-support")]
pub async fn mount_with_managed_and_resource_registry_and_dreams_on_lifecycle(
    managed_state: ScenarioManagedState,
    resources: awaken_resource_application::ResourcesApplication,
    lifecycle: awaken_service_lifecycle::ServiceLifecycle,
    dream_executor_decorator: Option<ScenarioDreamExecutorDecorator>,
) -> (Router, Arc<awaken_dream_application::DreamApplication>) {
    let local_workspace = managed_state.local_workspace.clone();
    let (data, dreams) = mount_with_managed_over(
        managed_state,
        resources,
        ApplicationAccessMount::ExplicitlyUnguardedTest,
        lifecycle,
        dream_executor_decorator,
    )
    .await;
    // The bare scenario/test mount has no separate management edge. Keep the
    // Awaken policy authoring projection reachable here so deterministic SDK and
    // Console E2E can exercise it. Production startup mounts this exact
    // router on `CoordinatorComponent::management_router`, behind IAM + audit.
    let router = data.merge(awaken_protocol_awaken::dream_policy_router(dreams.clone()));
    (with_local_workspace_scope(router, local_workspace), dreams)
}

/// Test-support data plane with application credentials enforced on browser-facing
/// AI SDK and AG-UI routes. Product startup uses the full dependency constructor.
#[cfg(feature = "test-support")]
pub async fn mount_with_managed_and_application_access(
    managed_state: ScenarioManagedState,
    resources: awaken_resource_application::ResourcesApplication,
    application_access: Arc<application_access_store::ApplicationAccessStore>,
) -> Router {
    mount_with_managed_over(
        managed_state,
        resources,
        ApplicationAccessMount::Guarded(application_access),
        awaken_service_lifecycle::ServiceLifecycle::new(),
        None,
    )
    .await
    .0
}

/// Test-support data plane with a live executable model directory.
#[cfg(feature = "test-support")]
pub async fn mount_with_managed_and_application_access_and_models(
    managed_state: ScenarioManagedState,
    resources: awaken_resource_application::ResourcesApplication,
    application_access: Arc<application_access_store::ApplicationAccessStore>,
    model_inventory: Arc<dyn awaken_executable_agent_contract::ExecutableAgentInventorySource>,
) -> Router {
    let resource_authorities = resources.authorities();
    let resource_registry = resource_authorities.resource_registry();
    let lifecycle = awaken_service_lifecycle::ServiceLifecycle::new();
    let execution = managed_state.execution.clone();
    let remote_worker_required = !execution.dispatch.topology().claim_pool.runs_local_pool();
    let dream_process_store = scenario_dream_process_store(execution.storage_dir.as_deref()).await;
    // The Worker transport judges assertions on the Scenario executor's clock.
    let worker_clock = managed_state.clock.clone();
    let local_workspace = managed_state.local_workspace.clone();
    let (resource_management_router, memory_stores) =
        resource_management_router_from_application(&resources, managed_state.sessions.queries());
    let supervised_state = managed_state.clone();
    let session_application = managed_state.sessions;
    let worker_file_application = resources.files();
    let worker_skill_bundles = resources.skill_bundle_source();
    let (managed, data, application, worker_private, dreams) = mount_with_managed_over_and_models(
        execution,
        managed_state.managed,
        ApplicationAccessMount::Guarded(application_access),
        ManagedApplicationServices {
            session_application,
            environment_warmups: managed_state.environment_warmups,
            resource_registry,
            model_inventory: Some(model_inventory),
            dream_process_store,
            executable_projection_refresh: None,
            #[cfg(feature = "test-support")]
            dream_executor_decorator: None,
        },
        ManagedRoutingExtensions {
            resource_management_router,
            resource_authorities,
            memory_stores,
            worker_file_application,
            worker_skill_bundles,
            worker_authenticator: Arc::new(
                awaken_worker_transport_security::HeaderWorkerAuthenticator,
            ),
            worker_placement_policy: None,
            worker_execution_bindings: None,
            worker_execution_stops: None,
            worker_create_broker_router: None,
            credential_deliveries: None,
            repository_transport_authorizer: None,
            worker_directory: managed_state.worker_directory.clone(),
            ai_sdk_browser_cors: AiSdkBrowserCors::default(),
            worker_clock,
            org_admission: None,
        },
        local_workspace,
    )
    .await
    .expect("test-support Worker transport must assemble");
    let public = with_scenario_worker_transport(
        managed.merge(data).merge(application),
        worker_private,
        remote_worker_required,
    );
    with_scenario_session_lifecycle(public, supervised_state, lifecycle, &dreams).await
}

/// Router-owned services that must move together into the managed data plane.
pub struct ManagedRoutingExtensions {
    pub resource_authorities: awaken_resource_application::ResourceAuthorities,
    pub resource_management_router: Router,
    pub memory_stores: Arc<dyn awaken_resource_contract::MemoryStoreApplicationService>,
    pub worker_file_application: Arc<dyn awaken_resource_contract::FileApplicationService>,
    pub worker_skill_bundles: Arc<
        dyn awaken_session_configuration_contract::SkillBundleSource<awaken_dispatch::RunClaim>,
    >,
    pub worker_authenticator: Arc<dyn awaken_worker_transport_security::WorkerRequestAuthenticator>,
    pub worker_placement_policy: Option<Arc<dyn awaken_worker_contract::PlacementPolicy>>,
    pub worker_execution_bindings: Option<Arc<dyn awaken_dispatch::WorkerExecutionBindingSource>>,
    pub worker_execution_stops: Option<Arc<dyn awaken_dispatch::WorkerExecutionStopSource>>,
    pub worker_create_broker_router: Option<Router>,
    pub credential_deliveries:
        Option<Arc<dyn awaken_credential_contract::AttemptCredentialDeliverySource>>,
    pub repository_transport_authorizer:
        Option<Arc<dyn awaken_resource_worker_http::RepositoryTransportAuthorizer>>,
    pub worker_directory: Arc<dyn awaken_worker_registry::WorkerDirectory>,
    /// Exact browser origins admitted only to the AI SDK application router.
    pub ai_sdk_browser_cors: AiSdkBrowserCors,
    /// The process wall clock the Worker transport judges leases, registry
    /// freshness and signed assertions with.
    pub worker_clock: Arc<dyn awaken_time::WallClock>,
    /// The Org request limiter the AI SDK, AG-UI and A2A entries admit through
    /// (the same one the Managed entries use); `None` mounts no limiter.
    pub org_admission: Option<Arc<dyn awaken_tenancy::org_admission::OrgRequestAdmission>>,
}

/// Application-layer authorities mounted together by the one managed data-plane
/// composition. Keeping this dependency cluster explicit prevents production
/// and deterministic hosts from growing parallel assembly signatures.
pub struct ManagedApplicationServices {
    /// Required executable Environment reads selected during composition.
    pub environment_warmups:
        Arc<dyn awaken_session_configuration_contract::EnvironmentWarmupSource>,
    pub session_application: Arc<awaken_session_application::SessionApplication>,
    pub resource_registry: Arc<dyn awaken_resource_contract::ResourceRegistry>,
    pub model_inventory:
        Option<Arc<dyn awaken_executable_agent_contract::ExecutableAgentInventorySource>>,
    pub dream_process_store: Arc<dyn awaken_session_dream_contract::DreamProcessStore>,
    pub executable_projection_refresh:
        Option<Arc<awaken_session_application_contract::ExecutableProjectionRefresh>>,
    #[cfg(feature = "test-support")]
    pub dream_executor_decorator: Option<ScenarioDreamExecutorDecorator>,
}

enum ApplicationAccessMount {
    Guarded(Arc<application_access_store::ApplicationAccessStore>),
    #[cfg(feature = "test-support")]
    ExplicitlyUnguardedTest,
}

struct RefreshingExecutableAgentInventory {
    source: Arc<dyn awaken_executable_agent_contract::ExecutableAgentInventorySource>,
    refresh: Arc<awaken_session_application_contract::ExecutableProjectionRefresh>,
}

#[async_trait::async_trait]
impl awaken_executable_agent_contract::ExecutableAgentInventorySource
    for RefreshingExecutableAgentInventory
{
    async fn current_registrations(
        &self,
        workspace_id: &str,
    ) -> Result<
        Vec<awaken_executable_agent_contract::ExecutableAgentRegistration>,
        awaken_executable_agent_contract::ExecutableAgentRegistrationError,
    > {
        let admission = self.refresh.refresh().await.map_err(|error| {
            awaken_executable_agent_contract::ExecutableAgentRegistrationError::Unavailable(error)
        })?;
        admission
            .enter(|| self.source.current_registrations(workspace_id))
            .await
    }
}

struct RefreshingEnvironmentWarmups {
    source: Arc<dyn awaken_session_configuration_contract::EnvironmentWarmupSource>,
    refresh: Arc<awaken_session_application_contract::ExecutableProjectionRefresh>,
}

#[async_trait::async_trait]
impl awaken_session_configuration_contract::EnvironmentWarmupSource
    for RefreshingEnvironmentWarmups
{
    async fn current_environment_warmups(
        &self,
    ) -> Result<Vec<awaken_session_configuration_contract::EnvironmentSnapshot>, String> {
        let admission = self.refresh.refresh().await?;
        admission
            .enter(|| self.source.current_environment_warmups())
            .await
    }
}

#[cfg(test)]
mod executable_projection_refresh_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    struct ToggleRefresh {
        fail: AtomicBool,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl awaken_session_application_contract::ExecutableAgentProjectionRefresh for ToggleRefresh {
        async fn refresh_agent_projection(&self) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                Err("projection unavailable".into())
            } else {
                Ok(())
            }
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_application_contract::ExecutableEnvironmentProjectionRefresh for ToggleRefresh {
        async fn refresh_environment_projection(&self) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                Err("projection unavailable".into())
            } else {
                Ok(())
            }
        }
    }

    #[derive(Default)]
    struct Inventory(AtomicUsize);

    #[async_trait::async_trait]
    impl awaken_executable_agent_contract::ExecutableAgentInventorySource for Inventory {
        async fn current_registrations(
            &self,
            _workspace_id: &str,
        ) -> Result<
            Vec<awaken_executable_agent_contract::ExecutableAgentRegistration>,
            awaken_executable_agent_contract::ExecutableAgentRegistrationError,
        > {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }
    }

    #[derive(Default)]
    struct Warmups(AtomicUsize);

    #[async_trait::async_trait]
    impl awaken_session_configuration_contract::EnvironmentWarmupSource for Warmups {
        async fn current_environment_warmups(
            &self,
        ) -> Result<Vec<awaken_session_configuration_contract::EnvironmentSnapshot>, String>
        {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn warmup_reads_keep_the_selected_allocation_without_a_protocol_adapter() {
        // W1 no refresh -> use the exact injected Arc, including its effects.
        // This port is sufficient; no ManagedState or concrete Environment App
        // is constructed or retained to answer a Worker warmup read.
        let selected = Arc::new(Warmups::default());
        let source: Arc<dyn awaken_session_configuration_contract::EnvironmentWarmupSource> =
            selected.clone();
        let reads = environment_warmup_reads(source.clone(), None);
        assert!(Arc::ptr_eq(&reads, &source));
        assert!(
            reads
                .current_environment_warmups()
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(selected.0.load(Ordering::SeqCst), 1);
    }

    struct UnavailableWarmups(AtomicUsize);

    #[async_trait::async_trait]
    impl awaken_session_configuration_contract::EnvironmentWarmupSource for UnavailableWarmups {
        async fn current_environment_warmups(
            &self,
        ) -> Result<Vec<awaken_session_configuration_contract::EnvironmentSnapshot>, String>
        {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err("selected Environment source unavailable".into())
        }
    }

    #[tokio::test]
    async fn refreshed_warmup_reads_preserve_selected_source_errors_without_fallback() {
        // W2 refresh succeeds, selected catalog fails -> preserve its failure.
        // Repeated reads enter the same source; no empty-demand fallback/cache.
        let selected = Arc::new(UnavailableWarmups(AtomicUsize::new(0)));
        let refresh = Arc::new(ToggleRefresh::default());
        let pair = Arc::new(
            awaken_session_application_contract::ExecutableProjectionRefresh::new(
                refresh.clone(),
                refresh.clone(),
            ),
        );
        let reads = environment_warmup_reads(selected.clone(), Some(pair));
        for expected in 1..=2 {
            assert_eq!(
                reads.current_environment_warmups().await.unwrap_err(),
                "selected Environment source unavailable"
            );
            assert_eq!(selected.0.load(Ordering::SeqCst), expected);
            assert_eq!(refresh.calls.load(Ordering::SeqCst), expected * 2);
        }
    }

    #[tokio::test]
    async fn failed_refresh_never_enters_shared_inventory_or_warmup_handlers() {
        // Causes: C1 executable refresh succeeds/fails; C2 Agent inventory is
        // consumed by Models/Dream; C3 Environment warmups are consumed by the
        // Worker. Effects: E1 failure reaches each consumer as unavailable and
        // its source is not read; E2 recovery delegates once. Constraints: K1
        // adapters own no cursor/cache; K2 Models and Dream share one wrapped
        // inventory. Decision table: D1 !C1+C2=>E1; D2 !C1+C3=>E1;
        // D3 C1+C2+C3=>E2. Each rule requires one Agent attempt and, only after
        // it succeeds, one Environment attempt per handler entry so a false
        // pass cannot come from bypassing the prerequisite altogether.
        let refresh = Arc::new(ToggleRefresh::default());
        let pair = Arc::new(
            awaken_session_application_contract::ExecutableProjectionRefresh::new(
                refresh.clone(),
                refresh.clone(),
            ),
        );
        let inventory = Arc::new(Inventory::default());
        let warmups = Arc::new(Warmups::default());
        let refreshing_inventory =
            executable_agent_inventory_reads(Some(inventory.clone()), Some(pair.clone())).unwrap();
        let refreshing_warmups = environment_warmup_reads(warmups.clone(), Some(pair));
        refresh.fail.store(true, Ordering::SeqCst);
        assert!(
            refreshing_inventory
                .current_registrations("workspace")
                .await
                .is_err(),
            "D1/E1"
        );
        assert!(
            refreshing_warmups
                .current_environment_warmups()
                .await
                .is_err(),
            "D2/E1"
        );
        assert_eq!(inventory.0.load(Ordering::SeqCst), 0, "D1/E1");
        assert_eq!(warmups.0.load(Ordering::SeqCst), 0, "D2/E1");
        assert_eq!(refresh.calls.load(Ordering::SeqCst), 2, "D1+D2 gate calls");

        refresh.fail.store(false, Ordering::SeqCst);
        refreshing_inventory
            .current_registrations("workspace")
            .await
            .unwrap();
        refreshing_warmups
            .current_environment_warmups()
            .await
            .unwrap();
        assert_eq!(inventory.0.load(Ordering::SeqCst), 1, "D3/E2");
        assert_eq!(warmups.0.load(Ordering::SeqCst), 1, "D3/E2");
        assert_eq!(refresh.calls.load(Ordering::SeqCst), 6, "D3 gate calls");
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WorkerTransportBuildError {
    #[error("resolve durable Worker dispatch authority: {0}")]
    Dispatch(String),
    #[error("open durable Worker checkpoint authority at {path}: {error}")]
    CheckpointOpen {
        path: std::path::PathBuf,
        error: String,
    },
    #[error(
        "registered Worker transport requires durable stream checkpoints from Postgres or storage_dir"
    )]
    MissingCheckpointAuthority,
    #[error("registered Worker artifact transport requires the Resources File application")]
    MissingFileApplication,
}

fn worker_checkpoint_authority(
    dispatch: &awaken_dispatch::AnyCoordinatorDispatch,
    store_dir: Option<&std::path::Path>,
) -> Result<
    Arc<dyn awaken_agent_contract::stream::checkpoint::StreamCheckpointStore>,
    WorkerTransportBuildError,
> {
    if let Some(checkpoint) = dispatch.stream_checkpoint_store() {
        return Ok(checkpoint);
    }
    if let Some(root) = store_dir {
        let path = root.join("worker-stream-checkpoints");
        return awaken_store_fs::FsStreamCheckpointStore::open(&path)
            .map(|store| {
                Arc::new(store)
                    as Arc<dyn awaken_agent_contract::stream::checkpoint::StreamCheckpointStore>
            })
            .map_err(|error| WorkerTransportBuildError::CheckpointOpen {
                path,
                error: error.to_string(),
            });
    }
    #[cfg(feature = "test-support")]
    {
        Ok(Arc::new(
            awaken_store_inmem::MemoryStreamCheckpointStore::new(),
        ))
    }
    #[cfg(not(feature = "test-support"))]
    {
        Err(WorkerTransportBuildError::MissingCheckpointAuthority)
    }
}

#[cfg(test)]
mod worker_checkpoint_authority_tests {
    use super::*;
    use awaken_dispatch::{AnyCoordinatorDispatch, MemoryDispatchStore};

    #[test]
    fn checkpoint_authority_selection_is_fail_closed() {
        // Cause/effect graph: C1 the dispatch store owns a paired checkpoint ->
        // E1 reuse it; otherwise C2 storage_dir exists and opens -> E2 durable FS;
        // C2 exists but cannot open -> E3 startup error; with neither, C3 explicit
        // test-support build -> E4 volatile test authority, otherwise E5 startup
        // refusal. Product startup cannot reach E4.
        //
        // | Rule | C1 paired | C2 dir/open | C3 test build | Effect |
        // | R1   | yes       | -           | -             | E1     |
        // | R2   | no        | yes/yes     | -             | E2     |
        // | R3   | no        | yes/no      | -             | E3     |
        // | R4   | no        | no          | yes           | E4     |
        // | R5   | no        | no          | no            | E5     |
        // R1 is covered by the PostgreSQL active/active acceptance test.
        let dispatch = AnyCoordinatorDispatch::from_durable_dispatch(Arc::new(
            MemoryDispatchStore::new(awaken_time::system_clock()),
        ));
        let root = tempfile::tempdir().expect("R2 root");
        assert!(
            worker_checkpoint_authority(&dispatch, Some(root.path())).is_ok(),
            "R2"
        );

        let invalid = tempfile::NamedTempFile::new().expect("R3 invalid root");
        assert!(
            matches!(
                worker_checkpoint_authority(&dispatch, Some(invalid.path())),
                Err(WorkerTransportBuildError::CheckpointOpen { .. })
            ),
            "R3"
        );

        #[cfg(feature = "test-support")]
        assert!(worker_checkpoint_authority(&dispatch, None).is_ok(), "R4");
        #[cfg(not(feature = "test-support"))]
        assert!(
            matches!(
                worker_checkpoint_authority(&dispatch, None),
                Err(WorkerTransportBuildError::MissingCheckpointAuthority)
            ),
            "R5"
        );
    }
}

/// Production data plane with the live model directory and the exact Dream state
/// mounted in that same Router. Process startup uses the returned state for
/// periodic policies instead of constructing a second scheduler.
pub async fn mount_with_managed_application_access_models_and_dreams(
    execution: ExecutionPorts,
    managed_state: Arc<ManagedState>,
    application_access: Arc<application_access_store::ApplicationAccessStore>,
    applications: ManagedApplicationServices,
    routing: ManagedRoutingExtensions,
    local_workspace: String,
) -> Result<
    (
        Router,
        Router,
        Router,
        Router,
        Arc<awaken_dream_application::DreamApplication>,
    ),
    WorkerTransportBuildError,
> {
    mount_with_managed_over_and_models(
        execution,
        managed_state,
        ApplicationAccessMount::Guarded(application_access),
        applications,
        routing,
        local_workspace,
    )
    .await
}

#[cfg(feature = "test-support")]
async fn mount_with_managed_over(
    managed_state: ScenarioManagedState,
    resources: awaken_resource_application::ResourcesApplication,
    application_access: ApplicationAccessMount,
    lifecycle: awaken_service_lifecycle::ServiceLifecycle,
    dream_executor_decorator: Option<ScenarioDreamExecutorDecorator>,
) -> (Router, Arc<awaken_dream_application::DreamApplication>) {
    let resource_authorities = resources.authorities();
    let resource_registry = resource_authorities.resource_registry();
    let execution = managed_state.execution.clone();
    let remote_worker_required = !execution.dispatch.topology().claim_pool.runs_local_pool();
    // A scenario that opts into the same durable storage root as production
    // must not silently retain an in-memory Dream authority. Sessions and
    // Dreams share the concrete sessions.db aggregate in product startup; keep
    // crash/recovery fixtures on that same ownership boundary.
    let dream_process_store = scenario_dream_process_store(execution.storage_dir.as_deref()).await;
    // The Worker transport judges assertions on the Scenario executor's clock.
    let worker_clock = managed_state.clock.clone();
    let local_workspace = managed_state.local_workspace.clone();
    let supervised_state = managed_state.clone();
    let session_application = managed_state.sessions;
    let (resource_management_router, memory_stores) =
        resource_management_router_from_application(&resources, session_application.queries());
    let worker_file_application = resources.files();
    let worker_skill_bundles = resources.skill_bundle_source();
    let (managed, public, application, worker_private, dreams) =
        mount_with_managed_over_and_models(
            execution,
            managed_state.managed,
            application_access,
            ManagedApplicationServices {
                session_application,
                environment_warmups: managed_state.environment_warmups,
                resource_registry,
                model_inventory: None,
                dream_process_store,
                executable_projection_refresh: None,
                dream_executor_decorator,
            },
            ManagedRoutingExtensions {
                resource_management_router,
                resource_authorities,
                memory_stores,
                worker_file_application,
                worker_skill_bundles,
                worker_authenticator: Arc::new(
                    awaken_worker_transport_security::HeaderWorkerAuthenticator,
                ),
                worker_placement_policy: None,
                worker_execution_bindings: None,
                worker_execution_stops: None,
                worker_create_broker_router: None,
                credential_deliveries: None,
                repository_transport_authorizer: None,
                worker_directory: managed_state.worker_directory.clone(),
                ai_sdk_browser_cors: AiSdkBrowserCors::default(),
                worker_clock,
                org_admission: None,
            },
            local_workspace,
        )
        .await
        .expect("test-support Worker transport must assemble");
    let public = with_scenario_worker_transport(
        managed.merge(public).merge(application),
        worker_private,
        remote_worker_required,
    );
    (
        with_scenario_session_lifecycle(public, supervised_state, lifecycle, &dreams).await,
        dreams,
    )
}

fn executable_agent_inventory_reads(
    inventory: Option<Arc<dyn awaken_executable_agent_contract::ExecutableAgentInventorySource>>,
    refresh: Option<Arc<awaken_session_application_contract::ExecutableProjectionRefresh>>,
) -> Option<Arc<dyn awaken_executable_agent_contract::ExecutableAgentInventorySource>> {
    inventory.map(|source| match refresh {
        Some(refresh) => Arc::new(RefreshingExecutableAgentInventory { source, refresh })
            as Arc<dyn awaken_executable_agent_contract::ExecutableAgentInventorySource>,
        None => source,
    })
}

fn environment_warmup_reads(
    source: Arc<dyn awaken_session_configuration_contract::EnvironmentWarmupSource>,
    refresh: Option<Arc<awaken_session_application_contract::ExecutableProjectionRefresh>>,
) -> Arc<dyn awaken_session_configuration_contract::EnvironmentWarmupSource> {
    match refresh {
        Some(refresh) => Arc::new(RefreshingEnvironmentWarmups { source, refresh }),
        None => source,
    }
}

/// The Coordinator-derived reads a registered Worker pulls over its private
/// transport: current Environment warm demand, and the original Session
/// sources a draining Worker must report retained.
///
/// The warmup source is the exact Environment execution application already
/// used by Session admission; the retention subjects come from the same
/// Worker occupancy read that decides retirement. Neither adapter owns a
/// second catalog, WorkQueue, cache, report or receipt state.
fn worker_pulled_reads(
    environment_warmups: Arc<dyn awaken_session_configuration_contract::EnvironmentWarmupSource>,
    refresh: Option<Arc<awaken_session_application_contract::ExecutableProjectionRefresh>>,
    session_application: &Arc<awaken_session_application::SessionApplication>,
    directory: &Arc<dyn awaken_worker_registry::WorkerDirectory>,
    authenticator: &Arc<dyn awaken_worker_transport_security::WorkerRequestAuthenticator>,
    clock: &Arc<dyn awaken_time::WallClock>,
) -> (Router, Router) {
    let warmup_source = environment_warmup_reads(environment_warmups, refresh);
    (
        awaken_dispatch_http::worker_environment_warmup_router(
            warmup_source,
            directory.clone(),
            authenticator.clone(),
            clock.clone(),
        ),
        awaken_dispatch_http::worker_source_retention_router(
            session_application.worker_occupancy(),
            directory.clone(),
            authenticator.clone(),
            clock.clone(),
        ),
    )
}

fn registered_worker_transport_with_broker(
    dispatch: Router,
    resources: Router,
    commit: Arc<awaken_dispatch_http::ClaimedCommitHttpService>,
    environment_warmups: Router,
    source_retention: Router,
    task_attention: Router,
    broker: Option<Router>,
) -> Router {
    let transport = awaken_dispatch_http::registered_worker_transport_router_with_services(
        dispatch, resources, commit,
    )
    .merge(environment_warmups)
    .merge(source_retention)
    .merge(task_attention);
    match broker {
        Some(broker) => transport.merge(broker),
        None => transport,
    }
}

/// The read-only Awaken projections beside a Managed Session: its execution
/// evidence and its content-capture decision (ADR-0050 D8).
fn session_read_extensions() -> Router<Arc<ManagedState>> {
    awaken_extensions::session_execution_evidence_router(
        awaken_protocol_managed::get_session_execution_evidence,
    )
    .merge(awaken_extensions::session_content_capture_router(
        awaken_protocol_managed::get_session_content_capture,
    ))
}

async fn mount_with_managed_over_and_models(
    execution: ExecutionPorts,
    managed_state: Arc<ManagedState>,
    application_access: ApplicationAccessMount,
    applications: ManagedApplicationServices,
    routing: ManagedRoutingExtensions,
    local_workspace: String,
) -> Result<
    (
        Router,
        Router,
        Router,
        Router,
        Arc<awaken_dream_application::DreamApplication>,
    ),
    WorkerTransportBuildError,
> {
    let ManagedApplicationServices {
        environment_warmups,
        session_application,
        resource_registry,
        model_inventory,
        dream_process_store,
        executable_projection_refresh,
        #[cfg(feature = "test-support")]
        dream_executor_decorator,
    } = applications;
    let ManagedRoutingExtensions {
        resource_authorities,
        resource_management_router,
        memory_stores,
        worker_file_application,
        worker_skill_bundles,
        worker_authenticator,
        worker_placement_policy,
        worker_execution_bindings,
        worker_execution_stops,
        worker_create_broker_router,
        credential_deliveries,
        repository_transport_authorizer,
        worker_directory,
        ai_sdk_browser_cors,
        worker_clock,
        org_admission,
    } = routing;
    let worker_observations: Arc<dyn awaken_worker_contract::WorkerIncarnationSource> =
        worker_directory.clone();
    let preparation: Arc<dyn awaken_session_execution_contract::SessionPreparationPlacement> =
        execution.dispatch.preparation_placement();
    session_application
        .execution_admission()
        .verify_worker_admission(&worker_observations, &preparation)
        .map_err(|error| WorkerTransportBuildError::Dispatch(error.into()))?;
    let (environment_warmups, source_retention) = worker_pulled_reads(
        environment_warmups,
        executable_projection_refresh.clone(),
        &session_application,
        &worker_directory,
        &worker_authenticator,
        &worker_clock,
    );
    // A registered Worker delivers its Task observations to the same Session
    // Run application every other Run admission uses, and reads its held
    // Tasks' tenure from the same committed reads its claims recover from.
    let task_attention = awaken_dispatch_http::worker_task_control_router(
        execution.session_runs.clone(),
        execution.worker_recovery.clone(),
        worker_directory.clone(),
        worker_authenticator.clone(),
        worker_clock.clone(),
    );
    let model_inventory =
        executable_agent_inventory_reads(model_inventory, executable_projection_refresh);
    let dream_worker: Arc<dyn awaken_dream_application::DreamExecutor> =
        Arc::new(dream::BuiltInDreamAgent::new(
            session_application.clone(),
            &resource_authorities,
            resource_registry.clone(),
            memory_stores,
            worker_file_application.clone(),
        ));
    #[cfg(feature = "test-support")]
    let dream_worker = match dream_executor_decorator {
        Some(decorate) => decorate(dream_worker),
        None => dream_worker,
    };
    let dream_application = Arc::new(
        awaken_dream_application::DreamApplication::with_store(
            dream_worker,
            dream_process_store,
            worker_clock.clone(),
        )
        .await
        .expect("load durable Dream jobs"),
    );
    dream_application.bind_session_source(managed_state.clone());
    struct DreamModelReadiness {
        inventory: Arc<dyn awaken_executable_agent_contract::ExecutableAgentInventorySource>,
        sessions: Arc<awaken_session_application::SessionApplication>,
    }
    #[async_trait::async_trait]
    impl awaken_dream_application::DreamModelReadiness for DreamModelReadiness {
        async fn is_ready(&self, workspace_id: &str, model_id: &str) -> Result<bool, String> {
            if !dream_model_can_consume_local_inputs(model_id)? {
                return Ok(false);
            }
            let current = awaken_executable_agent_contract::current_model_references(
                self.inventory.as_ref(),
                workspace_id,
            )
            .await
            .map_err(|error| error.to_string())?;
            if current.iter().any(|reference| reference == model_id) {
                return Ok(true);
            }
            // Force the same parser + publication resolver used by ordinary
            // Session model overrides. The sentinel can never equal a real
            // requested model, so this is resolution-only and persists nothing.
            match self
                .sessions
                .creation_commands()
                .resolve_session_model_override(
                    workspace_id,
                    model_id,
                    "__awaken_dream_model_resolution_probe__",
                    Default::default(),
                )
                .await
            {
                Ok(_) => Ok(true),
                Err(error)
                    if error.kind == awaken_runtime_contract::runs::RunErrorKind::BadRequest =>
                {
                    Ok(false)
                }
                Err(error) => Err(error.to_string()),
            }
        }
    }
    if let Some(inventory) = model_inventory.clone() {
        dream_application.bind_model_readiness(Arc::new(DreamModelReadiness {
            inventory,
            sessions: session_application.clone(),
        }));
    }
    let dreams = awaken_protocol_managed::dreams_router(dream_application.clone());
    let resource_manifests = resource_manifest_routes(managed_state.clone());
    let session_extensions = session_routes::session_extensions(
        managed_state.clone(),
        session_application.clone(),
        execution.coordinator_dispatch.clone(),
    )?;
    let managed = router(managed_state.clone())
        .merge(
            awaken_extensions::session_event_admission_router(
                awaken_protocol_managed::close_event_admission,
            )
            .with_state(managed_state.clone()),
        )
        .merge(dreams)
        .merge(awaken_protocol_awaken::live_inbox_router(
            session_application.inputs(),
        ))
        .merge(resource_manifests)
        .merge(session_extensions);
    // One protocol-neutral Run application serves all three wire adapters, so
    // they share the executor with no per-protocol execution path. The root
    // already bound it back into the executor as its background submission.
    let session_runs = execution.session_runs.clone();
    let admitted_runs: Arc<dyn Runs> = session_runs.clone();
    let (ai_sdk, ag_ui) = application_protocol_routers(
        &session_runs,
        application_access,
        &ai_sdk_browser_cors,
        org_admission.as_ref(),
    );
    let application = ai_sdk.merge(ag_ui);
    // Inside the local Workspace scope this composition stamps below.
    let a2a = org_admission::admitted(
        awaken_protocol_a2a::router_with_storage_root(
            admitted_runs,
            execution.storage_dir.as_deref(),
            worker_clock.clone(),
        ),
        org_admission.as_ref(),
        org_admission::A2A,
    );
    // A coordinator-only Host owns both dispatch and committed Thread truth but
    // deliberately has no execution pool. Start its one environment-free repair
    // loop before exposing the Worker transport; local-pool embeddings already
    // perform the same maintenance inside that pool.
    if let Some(source) = worker_execution_stops.as_ref() {
        let maintenance = awaken_dispatch::WorkerEvidenceReleaseAdapter::new(source.clone());
        execution
            .dispatch
            .bind_worker_evidence(worker_directory.clone(), Arc::new(maintenance))
            .map_err(|error| WorkerTransportBuildError::Dispatch(error.to_string()))?;
    }
    // The durable-ingress operations surface (slice E): ADR-0009 follow-on verbs
    // (supersede / reconcile / manual quarantine + GC) over the same shared host.
    let durable_ops = awaken_dispatch_http::durable_ops_router_with_session_admission(
        execution.durable_runs.clone(),
        session_runs.clone(),
        session_runs,
        worker_clock.clone(),
    );
    // The Worker-facing cross-node seam: a dispatch-store-isolated Worker claims/settles runs
    // over the dispatch transport and pushes committed facts to the commit ingest.
    let dispatch = execution.coordinator_dispatch.clone();
    let checkpoint =
        worker_checkpoint_authority(dispatch.as_ref(), execution.storage_dir.as_deref())?;
    let dispatch_router = awaken_dispatch_http::registered_dispatch_router(
        awaken_dispatch_http::RegisteredDispatchDependencies {
            execution_bindings: worker_execution_bindings,
            execution_stops: worker_execution_stops,
            dispatch: dispatch.clone() as Arc<dyn awaken_dispatch_http::WorkerDispatchAuthority>,
            checkpoint,
            directory: worker_directory.clone(),
            policy: worker_placement_policy
                .unwrap_or_else(|| worker_placement::shared_worker_placement_policy()),
            realizations: session_application.realizations(),
            terminal_cleanup: session_application.terminal_cleanups(),
            session_preparation: session_application.realizations(),
            session_environment_bindings: session_application.environment_binding_sink(),
            coordination: session_application.coordination(),
            session_work: session_application.work_assignments(),
            authenticator: worker_authenticator.clone(),
            recovery: execution.worker_recovery.clone(),
            completion: execution.worker_completion.clone(),
            terminal_observer: execution.worker_settlement.clone(),
            stream_sink: execution.worker_stream.clone(),
            credential_deliveries,
            clock: worker_clock.clone(),
            lease_policy: Arc::new(
                awaken_worker_transport_security::FixedWorkerLeasePolicy::default(),
            ),
            registry_ttl_ms: awaken_dispatch_http::WORKER_REGISTRY_TTL_MS,
            max_attempts: awaken_dispatch_http::WORKER_DISPATCH_MAX_ATTEMPTS,
        },
    );
    let file_content = awaken_resource_worker_http::worker_file_content_router(Arc::new(
        awaken_resource_worker_http::WorkerFileContentService::new(
            execution.worker_file_content.clone(),
            dispatch.clone() as Arc<dyn awaken_dispatch_http::WorkerDispatchAuthority>,
            worker_authenticator.clone(),
            worker_directory.clone(),
            worker_clock.clone(),
        )
        .with_session_file_authorization(session_application.queries())
        .with_session_control(session_application.resource_effect_authorization())
        .with_recovery(execution.worker_recovery.clone()),
    ));
    let artifact_publication =
        awaken_resource_worker_http::worker_artifact_publication_router(Arc::new(
            awaken_resource_worker_http::WorkerArtifactPublicationService::new(
                worker_file_application,
                dispatch.clone() as Arc<dyn awaken_dispatch_http::WorkerDispatchAuthority>,
                worker_authenticator.clone(),
                worker_directory.clone(),
                worker_clock.clone(),
            )
            .with_terminal_preparation_control(session_application.terminal_cleanups())
            .with_checkpoint_artifact_control(session_application.resource_effect_authorization()),
        ));
    let memory = worker_memory::memory_router(
        &resource_authorities,
        resource_registry.clone(),
        dispatch.clone() as Arc<dyn awaken_dispatch_http::WorkerDispatchAuthority>,
        session_application.resource_effect_authorization(),
        worker_authenticator.clone(),
        worker_directory.clone(),
        worker_clock.clone(),
    );
    let repository_service = awaken_resource_worker_http::WorkerRepositoryBindingService::new(
        resource_registry,
        dispatch.clone() as Arc<dyn awaken_dispatch_http::WorkerDispatchAuthority>,
        worker_authenticator.clone(),
        worker_directory.clone(),
        worker_clock.clone(),
    )
    .with_materialization_control(session_application.resource_effect_authorization())
    .with_publication_control(session_application.terminal_cleanups());
    let repository_service = match repository_transport_authorizer {
        Some(authorizer) => repository_service.with_transport_authorizer(authorizer),
        None => repository_service,
    };
    let repositories =
        awaken_resource_worker_http::worker_repository_binding_router(Arc::new(repository_service));
    let resource_worker = file_content
        .merge(artifact_publication)
        .merge(memory)
        .merge(repositories)
        .merge(awaken_resource_worker_http::worker_skill_bundle_router(
            Arc::new(
                awaken_resource_worker_http::WorkerSkillBundleService::new(
                    worker_skill_bundles,
                    dispatch.clone() as Arc<dyn awaken_dispatch_http::WorkerDispatchAuthority>,
                    worker_authenticator.clone(),
                    worker_directory.clone(),
                    worker_clock.clone(),
                )
                .with_session_control(session_application.resource_effect_authorization()),
            ),
        ));
    let commit = Arc::new(awaken_dispatch_http::ClaimedCommitHttpService::new(
        Arc::new(
            awaken_dispatch::ClaimedCommitService::with_coordinator_resolver(
                dispatch as Arc<dyn awaken_dispatch_http::WorkerDispatchAuthority>,
                execution.commit_owners.clone(),
            ),
        ),
        worker_directory.clone(),
        worker_authenticator,
        worker_clock.clone(),
    ));
    let worker_transport = registered_worker_transport_with_broker(
        dispatch_router,
        resource_worker,
        commit,
        environment_warmups,
        source_retention,
        task_attention,
        worker_create_broker_router,
    );
    // The Models API (`/v1/models`) over the deployment's model directory.
    let models = model_inventory.map_or_else(
        || models_router(std::sync::Arc::new(Vec::new())),
        awaken_protocol_managed::models_router_with_inventory,
    );
    let managed = managed.merge(resource_management_router).merge(models);
    let managed = awaken_protocol_managed::with_managed_response_context(managed);
    // Fleet readiness is a deployment-wide read, so it joins the
    // service-authenticated product surface rather than the Managed data
    // plane, whose beta-header gate would reject a plain fleet question.
    let router = a2a.merge(durable_ops).merge(crate::worker_fleet::router(
        worker_directory.clone(),
        worker_clock.clone(),
    ));
    let router = with_local_workspace_scope(router, local_workspace.clone());
    let worker_transport = with_local_workspace_scope(worker_transport, local_workspace);
    Ok((
        managed,
        router,
        application,
        worker_transport,
        dream_application,
    ))
}

/// Dream's ordinary auxiliary Session consumes local Memory/File mounts. Native
/// and ACP backends share that Session environment; an outbound A2A reference
/// denotes a complete remote agent and is intentionally executor-only, so it
/// cannot receive those local authorities.
fn dream_model_can_consume_local_inputs(model_id: &str) -> Result<bool, String> {
    let selection = awaken_config_service::parse_managed_model_id(model_id)
        .map_err(|error| error.to_string())?;
    Ok(!matches!(
        selection,
        awaken_agent_config::ModelSelection::Pinned(ref binding)
            if matches!(
                binding.backend(),
                awaken_runtime_contract::resolved::Backend::Remote(_)
            )
    ))
}

#[cfg(test)]
mod dream_model_runtime_tests {
    #[test]
    fn dream_runtime_admission_matches_the_local_input_boundary() {
        for model in [
            "claude-sonnet-5",
            "deepseek-v4;provider=deepseek;api=anthropic_messages",
            "qwen/qwen3;provider=anyrouter;api=open_ai_chat;executor=acp:opencode",
            "executor=acp:claude",
            "profile=dream-primary",
        ] {
            assert!(
                super::dream_model_can_consume_local_inputs(model).unwrap(),
                "{model}"
            );
        }
        assert!(
            !super::dream_model_can_consume_local_inputs(
                "executor=a2a:https://third-party.example/agent"
            )
            .unwrap()
        );
        assert!(super::dream_model_can_consume_local_inputs("executor=a2a:").is_err());
    }
}

/// The browser application protocols (AI SDK and AG-UI) over one Session Run
/// application, kept apart so the caller merges them: Org admission inside the
/// application guard, which stamps the authenticated Workspace scope it meters,
/// and CORS outside the AI SDK guard only.
fn application_protocol_routers(
    session_runs: &Arc<awaken_session_application::SessionRunApplication>,
    application_access: ApplicationAccessMount,
    ai_sdk_browser_cors: &AiSdkBrowserCors,
    org_admission: Option<&Arc<dyn awaken_tenancy::org_admission::OrgRequestAdmission>>,
) -> (Router, Router) {
    // A guarded mount resolves every thread through a grant binding to an
    // existing Session. The unguarded test mount has no grant, so a first Run
    // creates the Session the grant would have bound.
    let session_runs = match &application_access {
        ApplicationAccessMount::Guarded(_) => session_runs.clone(),
        #[cfg(feature = "test-support")]
        ApplicationAccessMount::ExplicitlyUnguardedTest => {
            Arc::new(session_runs.as_ref().clone().with_first_run_creation())
        }
    };
    let ai_sdk = org_admission::admitted(
        awaken_protocol_ai_sdk::router_with_owner_scope(session_runs.clone()),
        org_admission,
        org_admission::AI_SDK,
    );
    let ag_ui = org_admission::admitted(
        awaken_protocol_ag_ui::router_with_owner_scope(session_runs),
        org_admission,
        org_admission::AG_UI,
    );
    let (ai_sdk, ag_ui) = match application_access {
        ApplicationAccessMount::Guarded(application_access) => {
            let application_authenticator: Arc<
                dyn awaken_authz_enforce::ApplicationAccessAuthenticator,
            > = application_access;
            (
                ai_sdk.layer(axum::middleware::from_fn_with_state(
                    application_authenticator.clone(),
                    awaken_authz_enforce::application_guard,
                )),
                ag_ui.layer(axum::middleware::from_fn_with_state(
                    application_authenticator,
                    awaken_authz_enforce::application_guard,
                )),
            )
        }
        #[cfg(feature = "test-support")]
        ApplicationAccessMount::ExplicitlyUnguardedTest => (ai_sdk, ag_ui),
    };
    // CORS wraps the already guarded AI SDK router so a matching browser
    // preflight is answered before bearer classification. It is deliberately
    // applied before merging AG-UI, keeping every sibling protocol closed.
    (ai_sdk_browser_cors.apply(ai_sdk), ag_ui)
}

fn with_local_workspace_scope(router: Router, local_workspace: String) -> Router {
    router.layer(axum::middleware::from_fn(
        move |mut request: axum::extract::Request, next: axum::middleware::Next| {
            let local_workspace = local_workspace.clone();
            async move {
                // A scope-less request is the local/single-tenant mode. Resolve
                // that mode once at the process edge so every adapter sees
                // the same platform-provisioned Workspace. Authenticated/cloud
                // edges already stamped a scope, which must remain authoritative.
                if request
                    .extensions()
                    .get::<awaken_runtime_contract::runs::scope::WorkspaceScope>()
                    .is_none()
                {
                    request.extensions_mut().insert(
                        awaken_runtime_contract::runs::scope::WorkspaceScope(local_workspace),
                    );
                }
                next.run(request).await
            }
        },
    ))
}

#[cfg(feature = "test-support")]
fn resource_management_router_from_application(
    resources: &awaken_resource_application::ResourcesApplication,
    sessions: Arc<dyn awaken_session_application::SessionRootReads>,
) -> (
    Router,
    Arc<dyn awaken_resource_contract::MemoryStoreApplicationService>,
) {
    let stores = resources.memory_stores();
    (
        resources_router(
            ResourcesRouterInput::new(
                resources,
                Arc::new(awaken_resource_contract::UnavailableRepositoryCredentialValidator),
            )
            .with_session_reads(sessions),
        ),
        stores,
    )
}

fn resource_manifest_routes(state: Arc<awaken_protocol_managed::ManagedState>) -> axum::Router {
    awaken_extensions::session_resource_manifest_router(
        awaken_protocol_managed::replace_resource_manifest,
        awaken_protocol_managed::get_resource_manifest,
    )
    .with_state(state)
}
