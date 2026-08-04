//! Session Environment resolution through the Coordinator application port.

use super::*;

impl ManagedState {
    pub(crate) async fn deployment_environment(
        &self,
        environment_id: &str,
    ) -> Result<
        Option<crate::env_registry::EnvItem>,
        awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError,
    > {
        self.environments.get(environment_id).await
    }

    pub(crate) fn deployment_agent_unavailable(&self, workspace_id: &str, agent_id: &str) -> bool {
        self.config_source
            .as_ref()
            .is_some_and(|source| source.agent_unavailable_in(workspace_id, agent_id))
    }

    pub(crate) fn deployment_unavailable_delegate(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<String> {
        self.config_source
            .as_ref()
            .and_then(|source| source.unavailable_delegate_in(workspace_id, agent_id))
    }

    /// Resolve one exact executable Environment snapshot for a new Session.
    ///
    /// An explicit Session selection follows the current Environment revision;
    /// an Agent publication binding follows its exact immutable revision. Both
    /// paths apply the current availability deny overlay in the execution
    /// catalog before the Session baseline is committed.
    pub(crate) async fn resolve_session_environment(
        &self,
        requested_environment_id: Option<&str>,
        published_environment: Option<
            &awaken_executable_agent_contract::ExecutableAgentEnvironment,
        >,
        published_backend_ref: Option<&str>,
        mcp_targets: &[awaken_session_contract::McpTarget],
    ) -> Result<(String, awaken_session_contract::EnvironmentSnapshot, bool), StateError> {
        let environment_id = requested_environment_id
            .map(str::to_owned)
            .or_else(|| published_environment.map(|binding| binding.environment_id.clone()))
            .unwrap_or_else(|| "env_local".to_string());
        let snapshot = match (requested_environment_id, published_environment) {
            (None, Some(binding)) => {
                self.environments
                    .resolve_exact_for_session(
                        &binding.environment_id,
                        binding.revision,
                        published_backend_ref,
                        mcp_targets,
                    )
                    .await
            }
            _ => {
                self.environments
                    .resolve_current_for_session(
                        &environment_id,
                        published_backend_ref,
                        mcp_targets,
                    )
                    .await
            }
        }
        .map_err(|error| StateError::Run(RunError::unavailable(error.to_string())))?
        .ok_or_else(|| {
            StateError::Run(RunError::bad_request(format!(
                "environment `{environment_id}` is unavailable"
            )))
        })?;
        super::sandbox_provisioning::validate_sandbox_provisioning_runtime(
            snapshot.snapshot.sandbox_provisioning,
            published_backend_ref,
        )
        .map_err(StateError::Run)?;
        Ok((environment_id, snapshot.snapshot, snapshot.self_hosted))
    }
}
