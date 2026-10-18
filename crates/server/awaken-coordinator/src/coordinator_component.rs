// @formal surface requirement=assembly.binary_roles.role_assembly
// @formal reason: This source implements the repository-controlled assembly product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Canonical Coordinator application and process services.
//!
//! Process startup supplies already-selected Runtime, Resources, storage,
//! identity, and transport adapters. This module owns the one Coordinator router
//! and the lifecycle supervisors attached to the exact states mounted in it.

use std::sync::Arc;

use awaken_deployment_application::DeploymentApplication;
use awaken_deployment_contract::DeploymentRepository;
use awaken_environment_application::execution::EnvironmentExecutionApplication;
use awaken_executable_agent_contract::{
    ExecutableAgentInventorySource, ExecutableAgentRegistrationSource,
};
use awaken_protocol_managed::ManagedState;
use awaken_resource_contract::ResourceRegistry;
use awaken_session_application::SessionApplication;
use awaken_session_dream_contract::DreamProcessStore;
use awaken_session_repository_contract::SessionRepository;
use awaken_worker_transport_security::WorkerRequestAuthenticator;
use axum::Router;

use crate::application_access_store::ApplicationAccessStore;
use crate::{ExecutorComponents, WorkerTransportBuildError};

const APPLICATION_ACCESS_RETENTION_MS: u64 = 24 * 60 * 60 * 1_000;
const APPLICATION_ACCESS_RETENTION_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(60);
const RUNTIME_BACKGROUND_DRAIN_SLICE: std::time::Duration = std::time::Duration::from_secs(1);
pub(crate) const APPLICATION_ACCESS_RETENTION_BATCH: u32 = 512;
pub(crate) const APPLICATION_ACCESS_RETENTION_MAX_BATCHES_PER_TICK: u32 = 256;
#[cfg(test)]
pub(crate) const APPLICATION_ACCESS_RETENTION_CAPACITY_PER_TICK: u32 =
    APPLICATION_ACCESS_RETENTION_BATCH * APPLICATION_ACCESS_RETENTION_MAX_BATCHES_PER_TICK;

/// Coordinator-owned ports and already-built sibling components.
///
/// There is no Control ConfigService, authoring repository, Resource content
/// database, or `WorkerNode` here. Local and distributed deployments adapt those
/// boundaries before invoking this builder; the injected `ExecutorComponents` is the
/// Coordinator's neutral Runtime port adapter.
pub struct CoordinatorDependencies {
    pub lifecycle_delivery: crate::PreparedManagedLifecycleDelivery,
    pub service_lifecycle: awaken_service_lifecycle::ServiceLifecycle,
    pub host: Arc<ExecutorComponents>,
    /// Canonical Session application shared by internal services and protocol
    /// projections. ManagedState may observe it but never owns its construction.
    pub session_application: Arc<SessionApplication>,
    pub managed_state: Arc<ManagedState>,
    pub resource_registry: Arc<dyn ResourceRegistry>,
    /// Resources-owned public API served by its application.
    pub resource_management_router: Router,
    pub resource_authorities: awaken_resource_application::ResourceAuthorities,
    /// Exact Resources MemoryStore service shared by HTTP and Dream.
    pub memory_stores: Arc<dyn awaken_resource_contract::MemoryStoreApplicationService>,
    pub worker_file_application: Arc<dyn awaken_resource_contract::FileApplicationService>,
    pub worker_skill_bundles: Arc<
        dyn awaken_session_configuration_contract::SkillBundleSource<awaken_dispatch::RunClaim>,
    >,
    pub application_access: Arc<ApplicationAccessStore>,
    /// Explicit route-scoped browser admission for the AI SDK ingress.
    pub ai_sdk_browser_cors: crate::AiSdkBrowserCors,
    pub model_inventory: Arc<dyn ExecutableAgentInventorySource>,
    pub dream_process_store: Arc<dyn DreamProcessStore>,
    pub worker_authenticator: Arc<dyn WorkerRequestAuthenticator>,
    pub worker_placement_policy: Option<Arc<dyn awaken_worker_contract::PlacementPolicy>>,
    /// Trusted provider composition, not Worker-authored manifest metadata.
    pub worker_execution_bindings:
        Option<Arc<dyn awaken_dispatch::WorkerExecutionBindingSource>>,
    pub worker_execution_stops: Option<Arc<dyn awaken_dispatch::WorkerExecutionStopSource>>,
    pub worker_relay_targets: Option<Arc<dyn awaken_worker_contract::WorkerRelayTargetObserver>>,
    /// The signed provider Create executor is mounted only on the private
    /// Worker router; Session and Dispatch retain their existing owners.
    pub worker_create_broker_router: Option<Router>,
    pub credential_deliveries:
        Option<Arc<dyn awaken_credential_contract::AttemptCredentialDeliverySource>>,
    pub repository_transport_authorizer:
        Option<Arc<dyn awaken_resource_worker_http::RepositoryTransportAuthorizer>>,
    /// Coordinator-owned Worker identity/incarnation authority. The process
    /// startup opens one durable adapter and injects that exact instance.
    pub worker_directory: Arc<dyn crate::WorkerDirectory>,
    /// The one restored Deployment aggregate. Process startup creates
    /// it before sibling components so an AllInOne Control Agent archive can
    /// invoke the exact same state mounted and scheduled by Coordinator.
    pub deployment_application: Arc<DeploymentApplication>,
    /// Managed protocol lowering for Deployment-authored Session inputs. The
    /// process edge selects this adapter; Coordinator only invokes the Deployment
    /// application's domain command.
    pub deployment_session_launcher:
        Arc<dyn awaken_deployment_application::DeploymentSessionLauncher>,
    pub executable_agents: Arc<dyn ExecutableAgentRegistrationSource>,
    /// One stateless process-composed refresh prerequisite shared by every
    /// application consumer of rebuildable executable projections.
    pub executable_projection_refresh:
        Option<Arc<awaken_session_application_contract::ExecutableProjectionRefresh>>,
    pub environments: Arc<EnvironmentExecutionApplication>,
    pub sessions: Arc<dyn SessionRepository>,
    pub default_workspace: String,
    /// Authenticated executable Agent and Environment registration routes.
    /// Application consumers refresh through the injected neutral port; these
    /// transport-only routes do not classify request paths.
    pub private_router: Router,
    /// Service-token verifier shared by private Control→Coordinator commands.
    /// AllInOne uses an in-process rollout target and leaves this absent.
    pub service_authenticator:
        Option<Arc<dyn awaken_service_auth_contract::ServiceRequestAuthenticator>>,
    /// Separate projected credential for the read-only Worker destination.
    /// The broader Coordinator private token is never handed to the Gateway.
    pub relay_destination_authenticator:
        Option<Arc<dyn awaken_service_auth_contract::ServiceRequestAuthenticator>>,
    /// The process's one wall clock, chosen by the binary that composes the
    /// Coordinator; every Coordinator-owned judgement of time reads it.
    pub clock: Arc<dyn awaken_time::WallClock>,
    /// The Org request limiter every runtime protocol entry admits through.
    pub org_admission: Arc<dyn awaken_tenancy::org_admission::OrgRequestAdmission>,
}

/// Complete Coordinator application surface.
pub struct CoordinatorComponent {
    /// Service-authenticated Session, Deployment, and Resources product surface.
    pub router: Router,
    /// Browser application protocols, already protected by the canonical
    /// application-token guard. Process composition must not wrap these routes
    /// in the service-token IAM edge because both credentials use the standard
    /// `Authorization` header and represent distinct authorities.
    pub application_router: Router,
    /// Official Managed data routes before the process IAM/audit/admission edge.
    pub managed_router: Router,
    /// Control-to-Coordinator and Worker-to-Coordinator service surface. Process
    /// the process binds it to the private listener and never merges it into
    /// `router`.
    pub private_router: Router,
    /// Coordinator-owned management commands before the process-level audit/IAM
    /// edge is applied.
    pub management_router: Router,
    /// WorkQueue-backed capability edge shared by the Managed data and Work
    /// routers. Process composition installs it outside management IAM so the
    /// per-Work bearer is classified before generic API-token authentication.
    pub work_session_access: Arc<awaken_protocol_managed::ManagedWorkSessionAccess>,
}

#[derive(Debug, thiserror::Error)]
pub enum CoordinatorBuildError {
    #[error("build dispatch pool: {0}")]
    DispatchPool(String),
    #[error("restore Deployment state: {0}")]
    DeploymentRestore(String),
    #[error("build registered Worker transport: {0}")]
    WorkerTransport(#[from] WorkerTransportBuildError),
}

/// `clock` is the Coordinator process's one wall clock.
pub async fn restore_deployment_application(
    repository: Arc<dyn DeploymentRepository>,
    clock: Arc<dyn awaken_time::WallClock>,
) -> Result<Arc<DeploymentApplication>, CoordinatorBuildError> {
    DeploymentApplication::from_repository(repository, clock)
        .await
        .map(Arc::new)
        .map_err(|error| CoordinatorBuildError::DeploymentRestore(error.to_string()))
}

/// Register the Coordinator's one Session lifecycle supervisor.
///
/// Production and deterministic Scenario compositions share this exact
/// registration point so restart recovery never depends on a protocol query
/// accidentally materializing Runtime state.
pub(crate) fn register_session_lifecycle(
    service_lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
    session_application: Arc<SessionApplication>,
) {
    service_lifecycle.spawn("coordinator-session-lifecycle", move |cancel| async move {
        session_application.run_lifecycle_supervisor(cancel).await
    });
}

/// Drain the exact Runtime Host's detached auxiliary work through the process's
/// one service lifecycle. Short slices let completed tasks be reaped while the
/// lifecycle's outer shared deadline remains the sole shutdown timeout owner.
pub(crate) fn register_runtime_background_drain(
    service_lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
    roles: awaken_session_executor::CoordinatorRoles,
) {
    service_lifecycle.spawn(
        "coordinator-runtime-background-drain",
        move |cancel| async move {
            cancel.cancelled().await;
            while !roles.drain(RUNTIME_BACKGROUND_DRAIN_SLICE).await {
                tokio::task::yield_now().await;
            }
            Ok(())
        },
    );
}

/// Start the existing Runtime/Dream services after application composition.
pub(crate) async fn start_runtime_services(
    roles: &awaken_session_executor::CoordinatorRoles,
    dreams: &Arc<awaken_dream_application::DreamApplication>,
) -> Result<(), CoordinatorBuildError> {
    roles
        .start_local_pool()
        .map_err(|error| CoordinatorBuildError::DispatchPool(error.to_string()))?;
    roles.start_terminal_reconciliation();
    dreams.resume_incomplete().await;
    Ok(())
}

/// Attach bounded application-credential retention to the Coordinator's one
/// service lifecycle. Cleanup failure is retried by this same task; it never
/// installs a local authorization fallback or a second maintenance scheduler.
pub fn register_application_access_retention(
    service_lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
    application_access: Arc<ApplicationAccessStore>,
) {
    service_lifecycle.spawn(
        "coordinator-application-access-retention",
        move |cancel| async move {
            let mut interval = tokio::time::interval(APPLICATION_ACCESS_RETENTION_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = interval.tick() => {}
                }
                let terminal_before =
                    awaken_time::WallClockExt::now_ms(application_access.clock().as_ref())
                        .saturating_sub(APPLICATION_ACCESS_RETENTION_MS);
                if let Err(error) =
                    drain_application_access_retention(&application_access, terminal_before).await
                {
                    eprintln!("application access retention retry remains pending: {error}");
                }
            }
            Ok(())
        },
    );
}

/// Drain one bounded retention budget through the same repository used for
/// authorization. A short batch proves the backlog is empty; full batches yield
/// to request work, and the hard cap prevents a stale backlog from monopolizing
/// the runtime. Repository failure returns immediately and the next lifecycle
/// tick resumes from durable truth without an authorization fallback.
pub(crate) async fn drain_application_access_retention(
    application_access: &ApplicationAccessStore,
    terminal_before_unix_ms: u64,
) -> Result<u64, crate::application_access_store::ApplicationAccessRepositoryError> {
    let mut total = 0_u64;
    for _ in 0..APPLICATION_ACCESS_RETENTION_MAX_BATCHES_PER_TICK {
        let deleted = application_access
            .delete_terminal_before(terminal_before_unix_ms, APPLICATION_ACCESS_RETENTION_BATCH)
            .await?;
        total += deleted;
        if deleted < u64::from(APPLICATION_ACCESS_RETENTION_BATCH) {
            break;
        }
        tokio::task::yield_now().await;
    }
    Ok(total)
}

/// Schedule the exact Deployment and Dream applications mounted by this component.
fn register_deployment_dream_scheduler(
    lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
    deployments: Arc<DeploymentApplication>,
    dreams: Arc<awaken_dream_application::DreamApplication>,
    clock: Arc<dyn awaken_time::WallClock>,
) {
    lifecycle.spawn(
        "coordinator-deployment-dream-scheduler",
        move |cancel| async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = interval.tick() => {}
                }
                let now_ms = awaken_time::WallClockExt::now_ms(&*clock);
                if let Err(error) = deployments.tick_and_launch(now_ms).await {
                    eprintln!("scheduled Deployment tick failed: {error}");
                }
                if let Err(error) = dreams.tick_policies(now_ms).await {
                    eprintln!("scheduled Dream policy tick failed: {error}");
                }
            }
            Ok(())
        },
    );
}

/// Build the one authoritative Coordinator component.
pub async fn build_coordinator_component(
    dependencies: CoordinatorDependencies,
) -> Result<CoordinatorComponent, CoordinatorBuildError> {
    let CoordinatorDependencies {
        service_lifecycle,
        lifecycle_delivery,
        host,
        session_application,
        managed_state,
        resource_registry,
        resource_management_router,
        resource_authorities,
        memory_stores,
        worker_file_application,
        worker_skill_bundles,
        application_access,
        ai_sdk_browser_cors,
        model_inventory,
        dream_process_store,
        worker_authenticator,
        worker_placement_policy,
        worker_execution_bindings,
        worker_execution_stops,
        worker_relay_targets,
        worker_create_broker_router,
        credential_deliveries,
        repository_transport_authorizer,
        worker_directory,
        deployment_application,
        deployment_session_launcher,
        executable_agents,
        executable_projection_refresh,
        environments,
        sessions,
        default_workspace,
        private_router,
        service_authenticator,
        relay_destination_authenticator,
        clock,
        org_admission,
    } = dependencies;

    deployment_application.bind_executable_agents(executable_agents);
    if let Some(refresh) = executable_projection_refresh.clone() {
        deployment_application.bind_executable_projection_refresh(refresh);
    }

    deployment_application.bind_launcher(deployment_session_launcher);

    let domain_observation = service_authenticator.as_ref().map(|authenticator| {
        crate::worker_observation_boundary::domain_router_with_authenticator(
            crate::worker_observation_boundary::DomainState {
                directory: worker_directory.clone(),
                host: host.clone(),
                environments: environments.clone(),
                sessions: session_application.clone(),
                relay_targets: worker_relay_targets.clone(),
                bindings: worker_execution_bindings.clone(),
                clock: clock.clone(),
                authenticator: authenticator.clone(),
                relay_authenticator: relay_destination_authenticator.clone(),
            },
        )
    });
    let (managed, data, application, worker_transport, dream_application) =
        crate::mount_with_managed_application_access_models_and_dreams(
            host.clone(),
            managed_state.clone(),
            application_access.clone(),
            crate::ManagedApplicationServices {
                #[cfg(feature = "test-support")]
                dream_executor_decorator: None,
                session_application: session_application.clone(),
                resource_registry,
                model_inventory: Some(model_inventory),
                dream_process_store,
                executable_projection_refresh,
            },
            crate::ManagedRoutingExtensions {
                resource_management_router,
                resource_authorities,
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
                worker_clock: clock.clone(),
                org_admission: Some(org_admission),
            },
        )
        .await?;
    let managed = crate::with_local_workspace_scope(managed, default_workspace.clone());
    let private_router = match service_authenticator {
        Some(authenticator) => private_router.merge(crate::session_credential_rollout::router(
            &session_application,
            &managed_state,
            authenticator,
        )),
        None => private_router,
    };
    let private_router = private_router.merge(worker_transport);
    let private_router = match domain_observation {
        Some(observation) => private_router.merge(observation),
        None => private_router,
    };
    let work_session_access =
        awaken_protocol_managed::ManagedWorkSessionAccess::new(environments.clone(), managed_state);
    let management_router = management_router(
        &deployment_application,
        environments,
        &dream_application,
        &application_access,
        sessions,
        default_workspace,
        &clock,
    );

    register_application_access_retention(&service_lifecycle, application_access);
    start_coordinator_services(
        &service_lifecycle,
        lifecycle_delivery,
        host,
        session_application,
        deployment_application,
        dream_application,
        clock,
    )
    .await?;

    Ok(CoordinatorComponent {
        router: data,
        application_router: application,
        managed_router: managed,
        private_router,
        management_router,
        work_session_access,
    })
}

/// Compose the four existing management surfaces over their exact applications.
/// Only Deployment/Environment receive the Managed response context; the local
/// Workspace scope encloses all four, in the same merge order as public startup.
fn management_router(
    deployments: &Arc<DeploymentApplication>,
    environments: Arc<EnvironmentExecutionApplication>,
    dreams: &Arc<awaken_dream_application::DreamApplication>,
    application_access: &Arc<ApplicationAccessStore>,
    sessions: Arc<dyn SessionRepository>,
    workspace: String,
    clock: &Arc<dyn awaken_time::WallClock>,
) -> Router {
    let managed = awaken_protocol_managed::with_managed_response_context(
        awaken_protocol_managed::deployments_router(deployments.clone()).merge(
            awaken_protocol_managed::environment_work_router(environments, clock.clone()),
        ),
    );
    crate::with_local_workspace_scope(
        managed
            .merge(awaken_protocol_awaken::dream_policy_router(dreams.clone()))
            .merge(crate::application_access::router(
                application_access.clone(),
                sessions,
            )),
        workspace,
    )
}

/// Start the exact already-built services only after every connection validates.
async fn start_coordinator_services(
    service_lifecycle: &awaken_service_lifecycle::ServiceLifecycle,
    lifecycle_delivery: crate::PreparedManagedLifecycleDelivery,
    host: Arc<ExecutorComponents>,
    session_application: Arc<SessionApplication>,
    deployment_application: Arc<DeploymentApplication>,
    dream_application: Arc<awaken_dream_application::DreamApplication>,
    clock: Arc<dyn awaken_time::WallClock>,
) -> Result<(), CoordinatorBuildError> {
    lifecycle_delivery.start(service_lifecycle);
    let roles = awaken_session_executor::CoordinatorRoles::of(&host);
    start_runtime_services(&roles, &dream_application).await?;
    register_session_lifecycle(service_lifecycle, session_application);
    register_runtime_background_drain(service_lifecycle, roles);

    // One timer drives the exact DeploymentApplication and DreamApplication mounted above;
    // no scheduler may reconstruct either aggregate beside this component.
    register_deployment_dream_scheduler(
        service_lifecycle,
        deployment_application,
        dream_application,
        clock,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use awaken_deployment_contract::DeploymentLifecycleFact;
    use awaken_deployment_contract::{
        DeploymentRepositoryError, DeploymentRunView, DeploymentView, DeploymentWriteOutcome,
        ScheduledRunClaimOutcome,
    };

    use super::*;

    struct FailingDeploymentRepository;

    #[async_trait]
    impl DeploymentRepository for FailingDeploymentRepository {
        async fn deployments(&self) -> Result<Vec<DeploymentView>, DeploymentRepositoryError> {
            Err(DeploymentRepositoryError::Storage("offline".into()))
        }

        async fn deployment_runs(
            &self,
        ) -> Result<Vec<DeploymentRunView>, DeploymentRepositoryError> {
            unreachable!("restore stops after the first failed authority read")
        }

        async fn write_deployment(
            &self,
            _record: DeploymentView,
            _expected_revision: Option<u64>,
            _scheduled_limit: usize,
            _lifecycle: Option<DeploymentLifecycleFact>,
        ) -> Result<DeploymentWriteOutcome, DeploymentRepositoryError> {
            unreachable!("restore is read-only")
        }

        async fn upsert_deployment_run(
            &self,
            _record: DeploymentRunView,
            _lifecycle: Option<DeploymentLifecycleFact>,
        ) -> Result<(), DeploymentRepositoryError> {
            unreachable!("restore is read-only")
        }

        async fn claim_scheduled_run(
            &self,
            _claim_id: &str,
            _expected_deployment_revision: u64,
            _deployment: DeploymentView,
            _run: DeploymentRunView,
            _lifecycle: DeploymentLifecycleFact,
        ) -> Result<ScheduledRunClaimOutcome, DeploymentRepositoryError> {
            unreachable!("restore never claims scheduled work")
        }
    }

    #[tokio::test]
    async fn deployment_restore_failure_stops_component_construction() {
        // Cause/effect decision table:
        // C1 Deployment repository restores -> the component may bind routes and
        // supervisors (covered by CLI role-surface tests); C2 its first authority
        // read fails -> return CoordinatorBuildError and create no router, launcher,
        // scheduler, or parallel in-memory Deployment aggregate.
        let error = restore_deployment_application(
            Arc::new(FailingDeploymentRepository),
            awaken_time::system_clock(),
        )
        .await
        .err()
        .expect("C2 must fail before component construction");
        assert!(error.to_string().contains("offline"), "C2 exact cause");
    }
}
