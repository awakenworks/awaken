// @formal surface requirement=runtime.agent_loop.runtime_execution
// @formal reason: Static owner: the Session application's capability role served by RunCapabilities; it projects
// @formal reason: the offered tools, Skills and delegates without granting any execution authority.
//! The Session application's `CapabilityRuntime` role and the capability
//! projection it advertises, served by [`RunCapabilities`].

use awaken_session_configuration_contract::{AgentCapabilities, BuiltinTool, CustomTool};

use super::RunCapabilities;

impl RunCapabilities {
    fn capabilities_for_workspace(&self, workspace: &str) -> AgentCapabilities {
        AgentCapabilities {
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
            skills: self.tools.skills.managed_ids_in(workspace),
            delegates: self.delegate_ids_in(workspace),
        }
    }
}

#[async_trait::async_trait]
impl awaken_session_execution_contract::CapabilityRuntime for RunCapabilities {
    /// Advertise the host's provisioned surface on the created session: its built-in
    /// tools (folded into the agent toolset by the adapter), client tools, offered
    /// skills, and delegate roster. (MCP servers and file resources are not advertised
    /// — the local host wires no MCP capability and has no Files-API resource yet.)
    fn capabilities(&self) -> AgentCapabilities {
        self.capabilities_for_workspace(&self.local_workspace)
    }

    fn capabilities_for(&self, thread: &str) -> AgentCapabilities {
        let workspace = self.thread_workspace(thread);
        let mut capabilities = self.capabilities_for_workspace(&workspace);
        // An unreadable registry advertises no delegate it cannot read.
        if let Some(delegate_ids) = self.thread_delegate_ids(thread).ok().flatten() {
            capabilities.delegates = delegate_ids;
        }
        capabilities
    }
}
