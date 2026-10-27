// @formal surface requirement=runtime.builtin_tools.tool_registration
// @formal reason: One Web-owned pure kernel parses, validates, composes and admits execution policy;
// @formal reason: the adjacent native tables cover typed and opaque inputs without effectful dependencies.
//! Web's implementation of the existing extension tool contract's compiler role.

use awaken_tool_contract::tool_configuration::{
    ToolConfigurationCompiler, ToolConfigurationError, ToolConfigurationInput,
};
use awaken_web_policy_contract::WebConfigurationContext;
use awaken_web_policy_contract::WebExecutionConfiguration;

/// Composition roots install this compiler in config and Session applications.
pub struct WebToolConfigurationCompiler;

impl ToolConfigurationCompiler for WebToolConfigurationCompiler {
    fn compile(
        &self,
        input: ToolConfigurationInput<'_>,
    ) -> Result<serde_json::Value, ToolConfigurationError> {
        if input.source != &awaken_agent_contract::ToolsetSource::Agent
            || !matches!(input.tool_name, "web_fetch" | "web_search")
        {
            return Ok(input.configuration.clone());
        }
        let clients = input
            .client_tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        WebExecutionConfiguration::compile_value_in(
            input.tool_name,
            input.configuration,
            WebConfigurationContext::Publication {
                client_tools: &clients,
            },
        )
        .map_err(|error| ToolConfigurationError {
            path: error.path,
            code: error.code.as_str().into(),
            message: error.message,
        })
    }
}
