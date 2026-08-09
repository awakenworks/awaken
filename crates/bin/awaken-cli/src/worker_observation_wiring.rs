//! One role-aware composition for the Coordinator Worker-observation authority.

use std::sync::Arc;

use axum::Router;

use crate::config::{ResolvedDeployment, Role};

pub(super) struct WorkerObservationWiring {
    pub(super) source: Arc<dyn awaken_coordinator::WorkerObservationSource>,
    pub(super) private_router: Router,
}

impl WorkerObservationWiring {
    pub(super) fn local(directory: awaken_coordinator::WorkerDirectoryHandle) -> Self {
        let source: Arc<dyn awaken_coordinator::WorkerObservationSource> = directory;
        Self {
            source,
            private_router: Router::new(),
        }
    }

    pub(super) fn control(deployment: &ResolvedDeployment) -> Result<Self, String> {
        let (coordinator_url, token) = deployment
            .executable_agent_registration
            .control_credentials()?;
        Ok(Self {
            source: Arc::new(
                awaken_coordinator::worker_observation_boundary::HttpWorkerObservationSource::new(
                    coordinator_url,
                    token,
                )?,
            ),
            private_router: Router::new(),
        })
    }

    pub(super) fn runtime(
        role: Role,
        deployment: &ResolvedDeployment,
        directory: awaken_coordinator::WorkerDirectoryHandle,
    ) -> Result<Self, String> {
        match role {
            Role::AllInOne => Ok(Self::local(directory)),
            Role::Coordinator => {
                let token = deployment
                    .executable_agent_registration
                    .coordinator_token()?;
                let source: Arc<dyn awaken_coordinator::WorkerObservationSource> =
                    directory.clone();
                Ok(Self {
                    private_router: awaken_coordinator::worker_observation_boundary::router(
                        source.clone(),
                        token,
                    )?,
                    source,
                })
            }
            Role::Control | Role::Worker => {
                unreachable!("runtime observation wiring belongs to AllInOne or Coordinator")
            }
        }
    }
}
