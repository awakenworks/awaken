// @formal surface requirement=runtime.agent_loop.runtime_execution
// @formal reason: Static owner: the Session application's capability role served by RunCapabilities; it projects
// @formal reason: the offered tools, Skills and delegates without granting any execution authority.
//! The Session application's `CapabilityRuntime` role and the capability
//! projection it advertises, served by [`RunCapabilities`].

use awaken_session_configuration_contract::{AgentCapabilities, BuiltinTool, CustomTool};

use awaken_runtime_contract::runs::RunError;

use super::RunCapabilities;

impl RunCapabilities {
    fn capabilities_for_workspace(&self, workspace: &str) -> Result<AgentCapabilities, RunError> {
        Ok(AgentCapabilities {
            builtin_tools: self
                .builtin_tools()
                .into_iter()
                .map(|(name, ask)| BuiltinTool { name, ask })
                .collect(),
            custom_tools: self
                .custom_tools()
                .into_iter()
                .map(|descriptor| {
                    let input_schema = descriptor.model_parameters();
                    CustomTool {
                        name: descriptor.id,
                        description: descriptor.description,
                        input_schema,
                    }
                })
                .collect(),
            // Managed capabilities advertise only ids that resolve to immutable
            // catalog versions. Host-static SkillSpec values belong to the
            // direct compatibility adapter and cannot truthfully satisfy a
            // Managed AgentSkillBinding.
            skills: self.tools.skills.managed_ids_in(workspace)?,
            delegates: self.delegate_ids_in(workspace),
        })
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::CapabilityRuntime for RunCapabilities {
    /// Advertise the host's provisioned surface on the created session: its built-in
    /// tools (folded into the agent toolset by the adapter), client tools, offered
    /// skills, and delegate roster. (MCP servers and file resources are not advertised
    /// — the local host wires no MCP capability and has no Files-API resource yet.)
    fn capabilities(&self) -> Result<AgentCapabilities, RunError> {
        self.capabilities_for_workspace(&self.local_workspace)
    }

    /// | Rule | registry | Session delegates bound | advertised delegates |
    /// |---|---|---|---|
    /// | C1 | readable | yes | the Session's own roster |
    /// | C2 | readable | no  | the Workspace default Agent's roster |
    /// | C3 | poisoned | -   | refused (`session_realization_lock_poisoned`) |
    /// | C4 | readable, Skill cache poisoned | - | refused (`session_executor_lock_poisoned`) |
    fn capabilities_for(&self, thread: &str) -> Result<AgentCapabilities, RunError> {
        let workspace = self.thread_workspace(thread)?;
        let mut capabilities = self.capabilities_for_workspace(&workspace)?;
        if let Some(delegate_ids) = self
            .thread_delegate_ids(thread)
            .map_err(crate::installed_session::registry_run_error)?
        {
            capabilities.delegates = delegate_ids;
        }
        Ok(capabilities)
    }

    /// The host-default surface this Host advertises for any Agent: its Hand
    /// and client-executed tools, plus delegation when the Agent delegates
    /// ([`crate::config::advertised_tools`], the same set a generated
    /// host-default snapshot carries).
    fn unpublished_agent_publication(
        &self,
        agent_id: &str,
        model_ref: &str,
        delegate_ids: &[String],
    ) -> Result<awaken_runtime_contract::ExecutableAgentSnapshot, RunError> {
        let delegates = delegate_ids.iter().cloned().collect();
        awaken_session_execution_contract::unpublished_agent_publication(
            agent_id,
            model_ref,
            crate::config::advertised_tools(&self.tools.client_tools, &delegates, &[]),
            delegate_ids,
        )
    }
}
