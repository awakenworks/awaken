// @formal surface requirement=runtime.builtin_tools.tool_registration
// @formal reason: Session compiles effective opaque tool settings through its required compiler
// @formal reason: before persistence or effects; it does not interpret extension-owned fields.
//! Neutral compilation for effective Session tool configuration.
use awaken_session_configuration_contract::SessionToolConfiguration;
use awaken_session_types::RunError;
use awaken_tool_contract::tool_configuration::compile_toolset_configurations;
pub(super) fn compile_tool_settings(
    compiler: &dyn awaken_tool_contract::ToolConfigurationCompiler,
    mut tools: SessionToolConfiguration,
) -> Result<SessionToolConfiguration, RunError> {
    tools.toolsets = compile_toolset_configurations(compiler, &tools.toolsets, &tools.client_tools)
        .map_err(|error| RunError::bad_request_classified(error.code.clone(), error.to_string()))?;
    Ok(tools)
}
