//! Canonical Coordinator application and process services.
//!
//! Process startup supplies already-selected Runtime, Resources, storage,
//! identity, and transport adapters. This module owns the one Coordinator router
//! and the lifecycle supervisors attached to the exact states mounted in it.

use std::sync::Arc;

use awaken_deployment_application::DeploymentApplication;
use awaken_deployment_contract::DeploymentRepository;
use awaken_environment_execution_application::EnvironmentExecutionApplication;
use awaken_executable_agent_contract::{
    ExecutableAgentInventorySource, ExecutableAgentRegistrationSource,
};
use awaken_protocol_managed::ManagedState;
use awaken_resource_contract::ResourceRegistry;
use awaken_session_application::SessionApplication;
use awaken_session_contract::DreamProcessStore;
use awaken_session_contract::ManagedSessionRepository;
use awaken_worker_transport_security::WorkerRequestAuthenticator;
use axum::Router;

use crate::application_access_store::ApplicationAccessStore;
use crate::{SharedHost, WorkerTransportBuildError};

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
/// boundaries before invoking this builder; the injected `SharedHost` is the
/// Coordinator's neutral Runtime port adapter.
pub struct CoordinatorDependencies {
    pub service_lifecycle: awaken_service_lifecycle::ServiceLifecycle,
    pub host: Arc<SharedHost>,
    /// Canonical Session application shared by internal services and protocol
    /// projections. ManagedState may observe it but never owns its construction.
    pub session_application: Arc<SessionApplication>,
    pub managed_state: Arc<ManagedState>,
    pub resource_registry: Arc<dyn ResourceRegistry>,
    /// Resources-owned public API served by its application.
    pub resource_management_router: Router,
    /// Exact Resources MemoryStore service shared by HTTP and Dream.
    pub memory_stores: Arc<dyn awaken_resource_contract::MemoryStoreApplicationService>,
    pub worker_file_application: Arc<dyn awaken_resource_contract::FileApplicationService>,
    pub worker_skill_bundles:
        Arc<dyn awaken_session_contract::SkillBundleSource<awaken_run_ingress::RunClaim>>,
    pub application_access: Arc<ApplicationAccessStore>,
    pub model_inventory: Arc<dyn ExecutableAgentInventorySource>,
    pub dream_process_store: Arc<dyn DreamProcessStore>,
    pub worker_authenticator: Arc<dyn WorkerRequestAuthenticator>,
    pub worker_placement_policy: Option<Arc<dyn awaken_worker_contract::PlacementPolicy>>,
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
        Option<Arc<dyn awaken_session_contract::ExecutableProjectionRefresh>>,
    pub environments: Arc<EnvironmentExecutionApplication>,
    pub sessions: Arc<dyn ManagedSessionRepository>,
    pub default_workspace: String,
    /// Authenticated executable Agent and Environment registration routes.
    /// Application consumers refresh through the injected neutral port; these
    /// transport-only routes do not classify request paths.
    pub private_router: Router,
    /// Service-token verifier shared by private Control→Coordinator commands.
    /// AllInOne uses an in-process rollout target and leaves this absent.
    pub service_authenticator:
        Option<Arc<dyn awaken_service_auth_contract::ServiceRequestAuthenticator>>,
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
    #[error("restore Deployment state: {0}")]
    DeploymentRestore(String),
    #[error("build registered Worker transport: {0}")]
    WorkerTransport(#[from] WorkerTransportBuildError),
}

pub async fn restore_deployment_application(
    repository: Arc<dyn DeploymentRepository>,
) -> Result<Arc<DeploymentApplication>, CoordinatorBuildError> {
    DeploymentApplication::from_repository(repository)
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
    host: Arc<SharedHost>,
) {
    service_lifecycle.spawn(
        "coordinator-runtime-background-drain",
        move |cancel| async move {
            cancel.cancelled().await;
            while !host.drain_runtime(RUNTIME_BACKGROUND_DRAIN_SLICE).await {
                tokio::task::yield_now().await;
            }
            Ok(())
        },
    );
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
                let terminal_before = crate::application_access_store::now_unix_millis()
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

/// Build the one authoritative Coordinator component.
pub async fn build_coordinator_component(
    dependencies: CoordinatorDependencies,
) -> Result<CoordinatorComponent, CoordinatorBuildError> {
    let CoordinatorDependencies {
        service_lifecycle,
        host,
        session_application,
        managed_state,
        resource_registry,
        resource_management_router,
        memory_stores,
        worker_file_application,
        worker_skill_bundles,
        application_access,
        model_inventory,
        dream_process_store,
        worker_authenticator,
        worker_placement_policy,
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
    } = dependencies;

    deployment_application.bind_executable_agents(executable_agents);
    if let Some(refresh) = executable_projection_refresh.clone() {
        deployment_application.bind_executable_projection_refresh(refresh);
    }

    // The canonical supervisor owns durable resource, MCP, and WorkQueue
    // recovery as background work. Component construction must expose readiness
    // without awaiting an external sandbox timeout for every persisted Session.
    register_session_lifecycle(&service_lifecycle, session_application.clone());
    register_runtime_background_drain(&service_lifecycle, host.clone());
    register_application_access_retention(&service_lifecycle, application_access.clone());
    deployment_application.bind_launcher(deployment_session_launcher);

    let (managed, data, application, worker_transport, dream_application) =
        crate::mount_with_managed_application_access_models_and_dreams(
            host,
            managed_state.clone(),
            application_access.clone(),
            crate::ManagedApplicationServices {
                session_application,
                resource_registry,
                model_inventory: Some(model_inventory),
                dream_process_store,
                executable_projection_refresh,
            },
            crate::ManagedRoutingExtensions {
                resource_management_router,
                memory_stores,
                worker_file_application,
                worker_skill_bundles,
                worker_authenticator,
                worker_placement_policy,
                repository_transport_authorizer,
                worker_directory,
            },
        )?;
    let private_router = match service_authenticator {
        Some(authenticator) => private_router.merge(
            awaken_protocol_managed::credential_rollout_router_with_authenticator(
                managed_state.clone(),
                authenticator,
            ),
        ),
        None => private_router,
    };
    let private_router = private_router.merge(worker_transport);
    let work_session_access =
        awaken_protocol_managed::ManagedWorkSessionAccess::new(environments.clone(), managed_state);
    let management_router =
        awaken_protocol_managed::deployments_router(deployment_application.clone())
            .merge(awaken_protocol_awaken::dream_policy_router(
                dream_application.clone(),
            ))
            .merge(awaken_protocol_managed::environment_work_router(
                environments,
            ))
            .merge(crate::application_access::router(
                application_access,
                sessions,
                default_workspace,
            ));

    // One timer drives the exact DeploymentApplication and DreamApplication mounted above;
    // no scheduler may reconstruct either aggregate beside this component.
    let scheduled_deployments = deployment_application;
    service_lifecycle.spawn(
        "coordinator-deployment-dream-scheduler",
        move |cancel| async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = interval.tick() => {}
                }
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_millis() as u64)
                    .unwrap_or_default();
                if let Err(error) = scheduled_deployments.tick_and_launch(now_ms).await {
                    eprintln!("scheduled Deployment tick failed: {error}");
                }
                if let Err(error) = dream_application.tick_policies(now_ms).await {
                    eprintln!("scheduled Dream policy tick failed: {error}");
                }
            }
            Ok(())
        },
    );

    Ok(CoordinatorComponent {
        router: data,
        application_router: application,
        managed_router: managed,
        private_router,
        management_router,
        work_session_access,
    })
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
        let error = restore_deployment_application(Arc::new(FailingDeploymentRepository))
            .await
            .err()
            .expect("C2 must fail before component construction");
        assert!(error.to_string().contains("offline"), "C2 exact cause");
    }
}
