//! Role-specific composition of the Deployment-to-Session launch boundary.

use std::sync::Arc;

use awaken_protocol_managed::{
    DeploymentState, LocalDeploymentSessionLauncher, ManagedState, deployment_session_launch_router,
};
use axum::Router;

use crate::config::{DeploymentSessionLaunchConfig, Role};

pub(crate) fn bind_control(
    deployments: Arc<DeploymentState>,
    config: &DeploymentSessionLaunchConfig,
) -> Result<(), String> {
    let (coordinator_url, token) = config.control_credentials()?;
    let launcher = awaken_server::HttpDeploymentSessionLauncher::new(coordinator_url, token)?;
    deployments.bind_launcher(Arc::new(launcher));
    Ok(())
}

pub(crate) fn bind_runtime(
    role: Role,
    deployments: Arc<DeploymentState>,
    sessions: Arc<ManagedState>,
    config: Option<&DeploymentSessionLaunchConfig>,
) -> Result<Router, String> {
    let launcher = Arc::new(LocalDeploymentSessionLauncher::new(sessions));
    if role == Role::Coordinator {
        let token = config
            .ok_or_else(|| {
                "Coordinator Deployment Session launch configuration is absent".to_owned()
            })?
            .coordinator_token()?;
        deployment_session_launch_router(launcher, token)
    } else {
        deployments.bind_launcher(launcher);
        Ok(Router::new())
    }
}
