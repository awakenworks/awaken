//! Canonical Coordinator application component assembly.
//!
//! Process composition supplies already-selected Runtime, Resources, storage,
//! identity, and transport adapters. This module owns the one Coordinator router
//! and the lifecycle supervisors attached to the exact states mounted in it.

use std::sync::Arc;

use awaken_authz_enforce::ApplicationAccessStore;
use awaken_deployment_contract::DeploymentRepository;
use awaken_executable_agent_contract::ExecutableAgentRegistrationSource;
use awaken_ext_memory::DreamRepository;
use awaken_managed_routers::ModelDirectory;
use awaken_protocol_managed::{
    DeploymentState, EnvironmentExecutionState, ManagedRateLimiter, ManagedState,
};
use awaken_resource_contract::ResourceCatalog;
use awaken_session_contract::ManagedSessionRepository;
use awaken_worker_transport_security::WorkerRequestAuthenticator;
use axum::Router;

use crate::SharedHost;

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
    pub dream_repository: Arc<dyn DreamRepository>,
    pub worker_authenticator: Arc<dyn WorkerRequestAuthenticator>,
    pub deployment_repository: Arc<dyn DeploymentRepository>,
    pub executable_agents: Arc<dyn ExecutableAgentRegistrationSource>,
    pub rate_limiter: Arc<ManagedRateLimiter>,
    pub environments: Arc<EnvironmentExecutionState>,
    pub sessions: Arc<dyn ManagedSessionRepository>,
    pub default_workspace: String,
    /// Authenticated executable-Agent registration routes, including any
    /// process-selected projection refresh middleware.
    pub registration_router: Router,
}

/// Complete Coordinator application surface.
pub struct CoordinatorComponent {
    pub router: Router,
    /// Coordinator-owned management commands before the process-level audit/IAM
    /// edge is applied.
    pub management_router: Router,
}

#[derive(Debug, thiserror::Error)]
pub enum CoordinatorBuildError {
    #[error("restore Deployment state: {0}")]
    DeploymentRestore(String),
}

async fn restore_deployment_state(
    repository: Arc<dyn DeploymentRepository>,
) -> Result<Arc<DeploymentState>, CoordinatorBuildError> {
    DeploymentState::with_repository(repository)
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
        dream_repository,
        worker_authenticator,
        deployment_repository,
        executable_agents,
        rate_limiter,
        environments,
        sessions,
        default_workspace,
        registration_router,
    } = dependencies;

    let deployment_state = restore_deployment_state(deployment_repository).await?;
    deployment_state.bind_rate_limiter(rate_limiter);
    deployment_state.bind_executable_agents(executable_agents);

    let reconciled_resource_activations = managed_state.reconcile_resource_activations().await;
    if reconciled_resource_activations > 0 {
        eprintln!(
            "reconciled {reconciled_resource_activations} durable Session resource activation(s)"
        );
    }
    let reconciled_mcp_attachments = managed_state.reconcile_mcp_attachments().await;
    if reconciled_mcp_attachments > 0 {
        eprintln!("reconciled {reconciled_mcp_attachments} durable Session MCP projection(s)");
    }
    let _ = managed_state.spawn_realization_lease_supervisor();
    deployment_state.bind_launcher(Arc::new(
        awaken_protocol_managed::LocalDeploymentSessionLauncher::new(managed_state.clone()),
    ));

    let (data, dream_state) = crate::mount_with_managed_application_access_models_and_dreams(
        host,
        managed_state,
        resource_catalog,
        application_access.clone(),
        model_directory,
        dream_repository,
        resource_management_router,
        worker_authenticator,
    );
    let data = data.merge(registration_router);
    let management_router = awaken_protocol_managed::deployments_router(deployment_state.clone())
        .merge(awaken_protocol_managed::environment_work_router(
            environments,
        ))
        .merge(crate::application_access::router(
            application_access,
            sessions,
            default_workspace,
        ));

    // One timer drives the exact DeploymentState and DreamState mounted above;
    // no scheduler may reconstruct either aggregate beside this component.
    let scheduled_deployments = deployment_state;
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
            if let Err(error) = dream_state.tick_policies(now_ms).await {
                eprintln!("scheduled Dream policy tick failed: {error}");
            }
        }
    });

    Ok(CoordinatorComponent {
        router: data,
        management_router,
    })
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use awaken_deployment_contract::{
        DeploymentRecord, DeploymentRepositoryError, DeploymentRunRecord,
    };
    use awaken_session_contract::ManagedLifecycleFact;

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

        async fn upsert_deployment(
            &self,
            _record: DeploymentRecord,
            _lifecycle: Option<ManagedLifecycleFact>,
        ) -> Result<(), DeploymentRepositoryError> {
            unreachable!("restore is read-only")
        }

        async fn upsert_deployment_run(
            &self,
            _record: DeploymentRunRecord,
            _lifecycle: Option<ManagedLifecycleFact>,
        ) -> Result<(), DeploymentRepositoryError> {
            unreachable!("restore is read-only")
        }

        async fn claim_scheduled_run(
            &self,
            _claim_id: &str,
            _deployment: DeploymentRecord,
            _run: DeploymentRunRecord,
            _lifecycle: ManagedLifecycleFact,
        ) -> Result<bool, DeploymentRepositoryError> {
            unreachable!("restore never claims scheduled work")
        }
    }

    #[tokio::test]
    async fn deployment_restore_failure_stops_component_construction() {
        // Cause/effect decision table:
        // C1 Deployment repository restores -> the component may bind routes and
        // supervisors (covered by CLI role-surface tests); C2 its first authority
        // read fails -> return CoordinatorBuildError and create no router, launcher,
        // scheduler, or parallel in-memory DeploymentState.
        let error = restore_deployment_state(Arc::new(FailingDeploymentRepository))
            .await
            .err()
            .expect("C2 must fail before component construction");
        assert!(error.to_string().contains("offline"), "C2 exact cause");
    }
}
