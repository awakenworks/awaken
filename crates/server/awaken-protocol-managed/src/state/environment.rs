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
        self.application
            .deployment_environment(environment_id)
            .await
    }

    pub(crate) fn deployment_agent_unavailable(&self, workspace_id: &str, agent_id: &str) -> bool {
        self.application
            .deployment_agent_unavailable(workspace_id, agent_id)
    }

    pub(crate) fn deployment_unavailable_delegate(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<String> {
        self.application
            .deployment_unavailable_delegate(workspace_id, agent_id)
    }

    /// Resolve one exact executable Environment snapshot for a new Session.
    ///
    /// An explicit Session selection follows the current Environment revision;
    /// an Agent publication binding follows its exact immutable revision. Both
    /// paths apply the current availability deny overlay in the execution
    /// catalog before the Session baseline is committed.
    pub(crate) async fn resolve_session_environment(
        &self,
        requested_environment_id: &str,
        published_environment: Option<
            &awaken_executable_agent_contract::ExecutableAgentEnvironment,
        >,
        published_backend_ref: Option<&str>,
        mcp_targets: &[awaken_session_contract::McpTarget],
    ) -> Result<(String, awaken_session_contract::EnvironmentSnapshot), StateError> {
        let resolved = self
            .application
            .resolve_session_environment(
                Some(requested_environment_id),
                published_environment,
                published_backend_ref,
                mcp_targets,
            )
            .await
            .map_err(StateError::Run)?;
        let environment_id = resolved.snapshot.environment_id.clone();
        Ok((environment_id, resolved.snapshot))
    }
}
