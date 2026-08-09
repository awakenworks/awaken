//! Adapter from Deployment's narrow launch port to the canonical Managed Session
//! create command. HTTP admission and scheduling remain owned by the parent modules.

use std::sync::Arc;

use super::{DeploymentLaunch, DeploymentLaunchOutcome, DeploymentSessionLauncher};
use crate::types::deployment::RunError;

pub struct LocalDeploymentSessionLauncher(Arc<crate::ManagedState>);

impl LocalDeploymentSessionLauncher {
    #[must_use]
    pub fn new(state: Arc<crate::ManagedState>) -> Self {
        Self(state)
    }
}

#[async_trait::async_trait]
impl DeploymentSessionLauncher for LocalDeploymentSessionLauncher {
    async fn launch(&self, request: DeploymentLaunch) -> DeploymentLaunchOutcome {
        if request.deployment_run_id.trim().is_empty() {
            return DeploymentLaunchOutcome::Failed {
                error: RunError::SessionCreationRejectedError {
                    message: "deployment_run_id is required for Session launch".into(),
                },
            };
        }
        if request.environment_id != "env_local"
            && let Some(environment) = self.0.deployment_environment(&request.environment_id).await
            && environment.is_none()
        {
            return DeploymentLaunchOutcome::Failed {
                error: RunError::EnvironmentNotFoundError {
                    message: format!("environment `{}` no longer exists", request.environment_id),
                },
            };
        }
        if self
            .0
            .deployment_agent_unavailable(&request.workspace_id, &request.agent.id)
        {
            return DeploymentLaunchOutcome::Failed {
                error: RunError::AgentArchivedError {
                    message: format!("agent `{}` is archived", request.agent.id),
                },
            };
        }
        if let Some(delegate) = self
            .0
            .deployment_unavailable_delegate(&request.workspace_id, &request.agent.id)
        {
            return DeploymentLaunchOutcome::Failed {
                error: RunError::AgentArchivedError {
                    message: format!("subagent `{delegate}` is archived"),
                },
            };
        }
        // Admission already decoded the exact deployment-event subset. Lower it
        // into the ordinary Session create command so validation, persistence and
        // initial execution have one authority. A Deployment must never create an
        // empty Session and then drive a second best-effort send-events path.
        let launch_fingerprint = awaken_session_contract::stable_fingerprint(&request);
        let initial_events = request.initial_events.into_iter().map(Into::into).collect();
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
                version: Some(request.agent.version as u32),
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
            resources: request.resources,
        };
        let session = match self
            .0
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
                let error = match error {
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
                };
                return DeploymentLaunchOutcome::Failed { error };
            }
        };
        DeploymentLaunchOutcome::Created {
            session_id: session.id,
        }
    }
}
