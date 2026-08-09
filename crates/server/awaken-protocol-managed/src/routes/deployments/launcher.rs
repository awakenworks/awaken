//! Managed Session adapter for the Deployment application's narrow launch port.

use std::sync::Arc;

use awaken_deployment_application::{
    DeploymentLaunch, DeploymentLaunchOutcome, DeploymentRunFailure, DeploymentSessionLauncher,
};

pub struct LocalDeploymentSessionLauncher {
    state: Arc<crate::ManagedState>,
    rate_limiter: Option<Arc<crate::ManagedRateLimiter>>,
}

impl LocalDeploymentSessionLauncher {
    #[must_use]
    pub fn new(state: Arc<crate::ManagedState>) -> Self {
        Self {
            state,
            rate_limiter: None,
        }
    }

    #[must_use]
    pub fn with_rate_limiter(mut self, limiter: Arc<crate::ManagedRateLimiter>) -> Self {
        self.rate_limiter = Some(limiter);
        self
    }
}

fn failed(error: crate::types::deployment::RunError) -> DeploymentLaunchOutcome {
    match serde_json::to_value(error)
        .ok()
        .and_then(|value| serde_json::from_value::<DeploymentRunFailure>(value).ok())
    {
        Some(error) => DeploymentLaunchOutcome::Failed { error },
        None => DeploymentLaunchOutcome::Unavailable {
            message: "cannot project Managed Session launch error".into(),
        },
    }
}

#[async_trait::async_trait]
impl DeploymentSessionLauncher for LocalDeploymentSessionLauncher {
    async fn launch(&self, request: DeploymentLaunch) -> DeploymentLaunchOutcome {
        use crate::types::deployment::RunError;

        if request.deployment_run_id.trim().is_empty() {
            return failed(RunError::SessionCreationRejectedError {
                message: "deployment_run_id is required for Session launch".into(),
            });
        }
        if self
            .rate_limiter
            .as_ref()
            .is_some_and(|limiter| !limiter.admit_internal_session_create())
        {
            return failed(RunError::SessionRateLimitedError {
                message: "organization Session creation rate limit exceeded".into(),
            });
        }
        if request.environment_id != "env_local" {
            match self
                .state
                .deployment_environment(&request.environment_id)
                .await
            {
                Ok(Some(_)) => {}
                Ok(None) => {
                    return failed(RunError::EnvironmentNotFoundError {
                        message: format!(
                            "environment `{}` no longer exists",
                            request.environment_id
                        ),
                    });
                }
                Err(error) => {
                    return DeploymentLaunchOutcome::Unavailable {
                        message: format!(
                            "environment catalog is unavailable while resolving `{}`: {error}",
                            request.environment_id
                        ),
                    };
                }
            }
        }
        if self
            .state
            .deployment_agent_unavailable(&request.workspace_id, &request.agent.id)
        {
            return failed(RunError::AgentArchivedError {
                message: format!("agent `{}` is archived", request.agent.id),
            });
        }
        if let Some(delegate) = self
            .state
            .deployment_unavailable_delegate(&request.workspace_id, &request.agent.id)
        {
            return failed(RunError::AgentArchivedError {
                message: format!("subagent `{delegate}` is archived"),
            });
        }
        let initial_events = match request
            .initial_events
            .iter()
            .cloned()
            .map(serde_json::from_value)
            .collect::<Result<Vec<crate::types::deployment::DeploymentInitialEvent>, _>>()
        {
            Ok(events) => events.into_iter().map(Into::into).collect(),
            Err(error) => {
                return failed(RunError::SessionCreationRejectedError {
                    message: format!("stored Deployment initial Event is invalid: {error}"),
                });
            }
        };
        let resources = match request
            .resources
            .iter()
            .cloned()
            .map(serde_json::from_value)
            .collect::<Result<Vec<crate::types::resource::ResourceInput>, _>>()
        {
            Ok(resources) => resources,
            Err(error) => {
                return failed(RunError::SessionCreationRejectedError {
                    message: format!("stored Deployment Resource is invalid: {error}"),
                });
            }
        };
        let launch_fingerprint = awaken_session_contract::stable_fingerprint(&request);
        let mut metadata = request.metadata;
        metadata.insert(
            "awaken.deployment_id".to_string(),
            request.deployment_id.clone(),
        );
        metadata.insert(
            "awaken.deployment_run_id".to_string(),
            request.deployment_run_id.clone(),
        );
        let create = crate::types::SessionCreateParams {
            agent: crate::types::AgentRef::Object(crate::types::AgentRefObject {
                id: request.agent.id,
                kind: Some(crate::types::AgentRefKind::Agent),
                version: u32::try_from(request.agent.version).ok(),
                system: None,
                tools: None,
                mcp_servers: None,
                skills: None,
                model: None,
            }),
            initial_events,
            application_contribution_required: false,
            environment_id: Some(request.environment_id),
            title: None,
            metadata,
            mcp_servers: Vec::new(),
            vault_ids: request.vault_ids,
            resources,
        };
        let session = match self
            .state
            .create_deployment_session_with_initial_events(
                &request.deployment_run_id,
                &launch_fingerprint,
                create,
                request.workspace_id,
            )
            .await
        {
            Ok(session) => session,
            Err(error) => {
                return failed(match error {
                    crate::StateError::VaultNotFound(id) => RunError::VaultNotFoundError {
                        message: format!("vault `{id}` not found"),
                    },
                    crate::StateError::Run(error) if error.code == "mcp_egress_blocked" => {
                        RunError::McpEgressBlockedError {
                            message: error.message,
                        }
                    }
                    error => RunError::SessionCreationRejectedError {
                        message: error.to_string(),
                    },
                });
            }
        };
        DeploymentLaunchOutcome::Created {
            session_id: session.id,
        }
    }
}
