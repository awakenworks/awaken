// @formal surface requirement=control.agent_profiles.immutable_profile
// @formal reason: The configuration service delegates opaque tool settings to its required
// @formal reason: extension compiler before authoring or publication effects; it never reads Web fields.
//! Pure tool-settings compilation at generic configuration boundaries.
use crate::ValidationIssue;
use awaken_agent_config::AgentConfig;
use awaken_tool_contract::tool_configuration::compile_toolset_configurations;
pub(crate) fn compile_tool_settings(
    compiler: &dyn awaken_tool_contract::ToolConfigurationCompiler,
    config: &AgentConfig,
) -> Result<AgentConfig, ValidationIssue> {
    let mut canonical = config.clone();
    let clients = config
        .client_tools
        .iter()
        .filter(|tool| tool.kind == awaken_runtime_contract::resolved::ToolKind::ClientExecuted)
        .map(|tool| awaken_agent_contract::ClientToolDescriptor {
            name: tool.id.clone(),
            description: tool.description.clone(),
            input_schema: tool.model_parameters(),
        })
        .collect::<Vec<_>>();
    canonical.toolsets = compile_toolset_configurations(compiler, &config.toolsets, &clients)
        .map_err(|error| ValidationIssue {
            path: error.path,
            code: Some(error.code),
            message: error.message,
        })?;
    Ok(canonical)
}
