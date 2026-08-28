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
            skills: self.host.skills.managed_ids_in(workspace),
            delegates: self.host.delegate_ids_in(workspace),
        }
    }
}
