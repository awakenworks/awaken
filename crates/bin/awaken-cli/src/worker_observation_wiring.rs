//! One role-aware startup for the Coordinator Worker-observation authority.

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
        let (coordinator_url, token_source) = deployment
            .executable_agent_registration
            .control_credentials()?;
        Ok(Self {
            source: Arc::new(
                awaken_coordinator::worker_observation_boundary::HttpWorkerObservationSource::with_token_source(
                    coordinator_url,
                    token_source,
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
                let authenticator = deployment
                    .executable_agent_registration
                    .coordinator_authenticator()?;
                let source: Arc<dyn awaken_coordinator::WorkerObservationSource> =
                    directory.clone();
                Ok(Self {
                    private_router:
                        awaken_coordinator::worker_observation_boundary::router_with_authenticator(
                            source.clone(),
                            authenticator,
                        ),
                    source,
                })
            }
            Role::Control | Role::Worker => {
                unreachable!("runtime observation wiring belongs to AllInOne or Coordinator")
            }
        }
    }
}
