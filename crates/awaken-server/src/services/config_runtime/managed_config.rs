use awaken_contract::{
    AgentSpec, McpServerSpec, ModelBindingSpec, ProviderSpec, SkillSpec, ToolSpec,
};

use super::{
    ConfigRuntimeError, ConfigRuntimeManager, NS_AGENTS, NS_MCP_SERVERS, NS_MODELS, NS_PROVIDERS,
    NS_SKILLS, NS_TOOLS, deserialize_namespace, fingerprint_config,
};

pub(crate) struct ManagedConfigSnapshot {
    pub(crate) providers: Vec<ProviderSpec>,
    pub(crate) models: Vec<ModelBindingSpec>,
    pub(crate) agents: Vec<AgentSpec>,
    pub(crate) mcp_servers: Vec<McpServerSpec>,
    pub(crate) tools: Vec<ToolSpec>,
    pub(crate) skills: Vec<SkillSpec>,
    pub(crate) fingerprint: u64,
}

impl ConfigRuntimeManager {
    pub(crate) async fn load_managed_config(
        &self,
    ) -> Result<ManagedConfigSnapshot, ConfigRuntimeError> {
        let provider_values = self.load_namespace_entries(NS_PROVIDERS).await?;
        let model_values = self.load_namespace_entries(NS_MODELS).await?;
        let agent_values = self.load_namespace_entries(NS_AGENTS).await?;
        let mcp_values = self.load_namespace_entries(NS_MCP_SERVERS).await?;
        let tool_values = self.load_namespace_entries(NS_TOOLS).await?;
        let skill_values = self.load_namespace_entries(NS_SKILLS).await?;

        let fingerprint = fingerprint_config(&[
            (NS_PROVIDERS, &provider_values),
            (NS_MODELS, &model_values),
            (NS_AGENTS, &agent_values),
            (NS_MCP_SERVERS, &mcp_values),
            (NS_TOOLS, &tool_values),
            (NS_SKILLS, &skill_values),
        ])?;

        Ok(ManagedConfigSnapshot {
            providers: deserialize_namespace(&provider_values)?,
            models: deserialize_namespace(&model_values)?,
            agents: deserialize_namespace(&agent_values)?,
            mcp_servers: deserialize_namespace(&mcp_values)?,
            tools: deserialize_namespace(&tool_values)?,
            skills: deserialize_namespace(&skill_values)?,
            fingerprint,
        })
    }
}
