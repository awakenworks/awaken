// @formal surface requirement=runtime.builtin_tools.tool_registration
// @formal reason: Session compiles effective opaque tool settings through its required compiler
// @formal reason: before persistence or effects; it does not interpret extension-owned fields.
//! Neutral compilation for effective Session tool configuration.
use awaken_executable_agent_contract::ExecutableAgentSessionProfile;
use awaken_runtime_contract::runs::RunError;
use awaken_session_configuration_contract::SessionToolConfiguration;
use awaken_tool_contract::tool_configuration::compile_toolset_configurations;
/// Apply the Profile's original inheritance rule to already selected Runtime
/// defaults. An empty Profile client roster clears host clients; it never
/// falls back. Protocol validation, authored replacement and compilation stay
/// at their caller's existing boundaries.
pub(super) fn inherit_profile_tools(
    mut capability_tools: SessionToolConfiguration,
    profile: Option<&ExecutableAgentSessionProfile>,
) -> SessionToolConfiguration {
    if let Some(profile) = profile {
        if !profile.toolsets.is_empty() {
            capability_tools.toolsets = profile.toolsets.clone();
        }
        capability_tools.client_tools = profile.client_tools.clone();
    }
    capability_tools
}

pub(super) fn compile_tool_settings(
    compiler: &dyn awaken_tool_contract::ToolConfigurationCompiler,
    mut tools: SessionToolConfiguration,
) -> Result<SessionToolConfiguration, RunError> {
    tools.toolsets = compile_toolset_configurations(compiler, &tools.toolsets, &tools.client_tools)
        .map_err(|error| RunError::bad_request_classified(error.code.clone(), error.to_string()))?;
    Ok(tools)
}

#[cfg(test)]
mod tests;
