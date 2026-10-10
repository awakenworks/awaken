//! Session Environment resolution through the Coordinator application port.

use super::*;

impl ManagedState {
    pub(crate) async fn deployment_environment(
        &self,
        environment_id: &str,
    ) -> Result<
        Option<awaken_environment_contract::EnvItem>,
        awaken_executable_environment_contract::ExecutableEnvironmentRegistrationError,
    > {
        self.roles
            .deployments
            .deployment_environment(environment_id)
            .await
    }

    pub(crate) fn deployment_agent_unavailable(&self, workspace_id: &str, agent_id: &str) -> bool {
        self.roles
            .deployments
            .deployment_agent_unavailable(workspace_id, agent_id)
    }

    pub(crate) fn deployment_unavailable_delegate(
        &self,
        workspace_id: &str,
        agent_id: &str,
    ) -> Option<String> {
        self.roles
            .deployments
            .deployment_unavailable_delegate(workspace_id, agent_id)
    }
}
