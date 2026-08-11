use super::*;

impl ManagedHost {
    pub(super) fn capabilities_for_workspace(&self, workspace: &str) -> AgentCapabilities {
        AgentCapabilities {
            builtin_tools: self
                .host
                .builtin_tools()
                .into_iter()
                .map(|(name, ask)| BuiltinTool { name, ask })
                .collect(),
            custom_tools: self
                .host
                .custom_tools()
                .into_iter()
                .map(|descriptor| CustomTool {
                    name: descriptor.id,
                    description: descriptor.description,
                    input_schema: descriptor.parameters,
                })
                .collect(),
            skills: self.host.skills.ids_in(workspace),
            delegates: self.host.delegate_ids_in(workspace),
        }
    }
}
