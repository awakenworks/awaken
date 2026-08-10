//! Canonical Coordinator application component assembly.
//!
//! Process composition supplies already-selected Runtime, Resources, storage,
//! identity, and transport adapters. This module owns the one Coordinator router
//! and the lifecycle supervisors attached to the exact states mounted in it.

use std::sync::Arc;

use awaken_authz_enforce::ApplicationAccessStore;
use awaken_deployment_application::DeploymentApplication;
use awaken_deployment_contract::DeploymentRepository;
use awaken_environment_execution_application::EnvironmentExecutionApplication;
use awaken_executable_agent_contract::ExecutableAgentRegistrationSource;
use awaken_protocol_managed::ModelDirectory;
use awaken_protocol_managed::{ManagedRateLimiter, ManagedState};
use awaken_resource_contract::ResourceCatalog;
use awaken_session_contract::DreamProcessStore;
use awaken_session_contract::ManagedSessionRepository;
use awaken_worker_transport_security::WorkerRequestAuthenticator;
use axum::Router;

use crate::{SharedHost, WorkerTransportBuildError};

/// Coordinator-owned ports and already-built sibling components.
///
/// There is no Control ConfigService, authoring repository, Resource content
/// database, or `WorkerNode` here. Local and distributed deployments adapt those
/// boundaries before invoking this builder; the injected `SharedHost` is the
/// Coordinator's neutral Runtime port adapter.
pub struct CoordinatorDependencies {
    pub host: Arc<SharedHost>,
    pub managed_state: Arc<ManagedState>,
    pub resource_catalog: Arc<dyn ResourceCatalog>,
    /// Resources-owned public API, assembled from its application component.
    pub resource_management_router: Router,
    pub application_access: Arc<ApplicationAccessStore>,
    pub model_directory: Arc<dyn ModelDirectory>,
    pub dream_process_store: Arc<dyn DreamProcessStore>,
    pub worker_authenticator: Arc<dyn WorkerRequestAuthenticator>,
    /// Coordinator-owned Worker identity/incarnation authority. The process
    /// composition opens one durable adapter and injects that exact instance.
    pub worker_directory: Arc<dyn crate::WorkerDirectory>,
    /// The one restored Deployment aggregate. The process composition creates
    /// it before sibling components so an AllInOne Control Agent archive can
    /// invoke the exact same state mounted and scheduled by Coordinator.
    pub deployment_application: Arc<DeploymentApplication>,
    pub executable_agents: Arc<dyn ExecutableAgentRegistrationSource>,
    pub rate_limiter: Arc<ManagedRateLimiter>,
    pub environments: Arc<EnvironmentExecutionApplication>,
    pub sessions: Arc<dyn ManagedSessionRepository>,
    pub default_workspace: String,
    /// Authenticated executable Agent and Environment registration routes.
    /// Process composition installs projection refresh around the final
    /// Runtime-admitting surface, not around these transport-only routes.
    pub private_router: Router,
}

/// Complete Coordinator application surface.
pub struct CoordinatorComponent {
    /// User, Session, Deployment, and authenticated Worker transport surface.
    pub router: Router,
    /// Management-to-Coordinator service surface. Process composition must bind
    /// this router to the private listener and never merge it into `router`.
    pub private_router: Router,
    /// Coordinator-owned management commands before the process-level audit/IAM
    /// edge is applied.
    pub management_router: Router,
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

/// Build the one authoritative Coordinator component.
pub async fn build_coordinator_component(
    dependencies: CoordinatorDependencies,
) -> Result<CoordinatorComponent, CoordinatorBuildError> {
    let CoordinatorDependencies {
        host,
        managed_state,
        resource_catalog,
        resource_management_router,
        application_access,
        model_directory,
        dream_process_store,
        worker_authenticator,
        worker_directory,
        deployment_application,
        executable_agents,
        rate_limiter,
        environments,
        sessions,
        default_workspace,
        private_router,
    } = dependencies;

    deployment_application.bind_executable_agents(executable_agents);

    // The canonical supervisor owns durable resource, MCP, and WorkQueue
    // recovery as background work. Component construction must expose readiness
    // without awaiting an external sandbox timeout for every persisted Session.
    let _ = managed_state
        .session_application()
        .spawn_lifecycle_supervisor();
    deployment_application.bind_launcher(Arc::new(
        awaken_protocol_managed::LocalDeploymentSessionLauncher::new(managed_state.clone())
            .with_rate_limiter(rate_limiter),
    ));

    let environment_warmups = awaken_run_ingress_http::worker_environment_warmup_router(
        environments.clone(),
        worker_directory.clone(),
        worker_authenticator.clone(),
    );
    let (data, dream_application) = crate::mount_with_managed_application_access_models_and_dreams(
        host,
        managed_state,
        resource_catalog,
        application_access.clone(),
        model_directory,
        dream_process_store,
        crate::ManagedRoutingExtensions {
            resource_management_router,
            worker_authenticator,
            worker_directory,
        },
    )?;
    let data = data.merge(environment_warmups);
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
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
        loop {
            interval.tick().await;
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
    });

    Ok(CoordinatorComponent {
        router: data,
        private_router,
        management_router,
    })
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use awaken_deployment_contract::DeploymentLifecycleFact;
    use awaken_deployment_contract::{
        DeploymentRecord, DeploymentRepositoryError, DeploymentRunRecord, DeploymentWriteOutcome,
        ScheduledRunClaimOutcome,
    };

    use super::*;

    struct FailingDeploymentRepository;

    #[async_trait]
    impl DeploymentRepository for FailingDeploymentRepository {
        async fn deployments(&self) -> Result<Vec<DeploymentRecord>, DeploymentRepositoryError> {
            Err(DeploymentRepositoryError::Storage("offline".into()))
        }

        async fn deployment_runs(
            &self,
        ) -> Result<Vec<DeploymentRunRecord>, DeploymentRepositoryError> {
            unreachable!("restore stops after the first failed authority read")
        }

        async fn write_deployment(
            &self,
            _record: DeploymentRecord,
            _expected_revision: Option<u64>,
            _scheduled_limit: usize,
            _lifecycle: Option<DeploymentLifecycleFact>,
        ) -> Result<DeploymentWriteOutcome, DeploymentRepositoryError> {
            unreachable!("restore is read-only")
        }

        async fn upsert_deployment_run(
            &self,
            _record: DeploymentRunRecord,
            _lifecycle: Option<DeploymentLifecycleFact>,
        ) -> Result<(), DeploymentRepositoryError> {
            unreachable!("restore is read-only")
        }

        async fn claim_scheduled_run(
            &self,
            _claim_id: &str,
            _expected_deployment_revision: u64,
            _deployment: DeploymentRecord,
            _run: DeploymentRunRecord,
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
