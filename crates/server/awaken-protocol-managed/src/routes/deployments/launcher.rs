//! Managed Session adapter for the Deployment application's narrow launch port.

use std::sync::Arc;

use awaken_deployment_application::{
    DeploymentLaunch, DeploymentLaunchOutcome, DeploymentSessionLauncher,
};

pub struct ManagedDeploymentSessionLauncher {
    state: Arc<crate::ManagedState>,
    rate_limiter: Option<Arc<dyn crate::ManagedRequestLimiter>>,
}

impl ManagedDeploymentSessionLauncher {
    #[must_use]
    pub fn new(state: Arc<crate::ManagedState>) -> Self {
        Self {
            state,
            rate_limiter: None,
        }
    }

    #[must_use]
    pub fn with_rate_limiter(mut self, limiter: Arc<dyn crate::ManagedRequestLimiter>) -> Self {
        self.rate_limiter = Some(limiter);
        self
    }
}

fn failed(error: crate::types::deployment::RunError) -> DeploymentLaunchOutcome {
    DeploymentLaunchOutcome::Failed {
        error: super::application_run_failure(error),
    }
}

#[async_trait::async_trait]
impl DeploymentSessionLauncher for ManagedDeploymentSessionLauncher {
    async fn launch(&self, request: DeploymentLaunch) -> DeploymentLaunchOutcome {
        use crate::types::deployment::RunError;

        if request.deployment_run_id.trim().is_empty() {
            return failed(RunError::SessionCreationRejectedError {
                message: "deployment_run_id is required for Session launch".into(),
            });
        }
        let agent_version = match u32::try_from(request.agent.version) {
            Ok(version) => version,
            Err(_) => {
                return failed(RunError::SessionCreationRejectedError {
                    message: format!(
                        "stored Deployment Agent version {} exceeds the Managed Session protocol range",
                        request.agent.version
                    ),
                });
            }
        };
        if let Some(limiter) = &self.rate_limiter {
            match limiter
                .admit(crate::ManagedRateLimitRequest {
                    workspace_id: request.workspace_id.clone(),
                    operation: crate::ManagedOperation::Create,
                    resource: "sessions",
                    operation_id: Some(request.deployment_run_id.clone()),
                    source: crate::ManagedRequestSource::Deployment,
                })
                .await
            {
                Ok(decision) if !decision.allowed => {
                    return failed(RunError::SessionRateLimitedError {
                        message: "organization Session creation rate limit exceeded".into(),
                    });
                }
                Err(error) => {
                    return DeploymentLaunchOutcome::Unavailable {
                        message: error.to_string(),
                    };
                }
                Ok(_) => {}
            }
        }
        if request.environment_id != awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID {
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
        let launch_fingerprint = awaken_session_contract::stable_fingerprint(&request);
        let initial_events = request
            .initial_events
            .into_iter()
            .map(super::inbound_event)
            .collect();
        let resources = request
            .resources
            .into_iter()
            .map(super::wire_resource)
            .collect();
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
            agent: crate::types::AgentRef::Object(Box::new(crate::types::AgentRefObject::Agent {
                id: request.agent.id,
                version: Some(agent_version.into()),
            })),
            budget: request
                .budget_max_list_cost_minor
                .map(crate::types::BudgetLimit::from_minor),
            initial_events,
            environment_id: request.environment_id,
            title: None,
            metadata,
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
