// @formal surface requirement=runtime.builtin_tools.tool_registration
// @formal reason: One Web-owned pure kernel parses, validates, composes and admits execution policy;
// @formal reason: the adjacent native tables cover typed and opaque inputs without effectful dependencies.
//! The narrow extension-owned compiler for a neutral tool's opaque settings.

use awaken_agent_contract::{ClientToolDescriptor, ToolsetSource};
use serde_json::Value;

/// Explicit authoring facts. They carry no provider, credential or Runtime state.
pub struct ToolConfigurationInput<'a> {
    pub source: &'a ToolsetSource,
    pub tool_name: &'a str,
    pub configuration: &'a Value,
    pub client_tools: &'a [ClientToolDescriptor],
}

/// Stable extension diagnostic relative to the supplied configuration value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolConfigurationError {
    pub path: String,
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for ToolConfigurationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} [{}]: {}", self.path, self.code, self.message)
    }
}
impl std::error::Error for ToolConfigurationError {}

/// Required at application construction. An extension recognizes the settings
/// it owns and preserves other tools' opaque values. Compilation is pure and
/// returns the canonical value in the same fallible call that validates it.
pub trait ToolConfigurationCompiler: Send + Sync {
    fn compile(&self, input: ToolConfigurationInput<'_>) -> Result<Value, ToolConfigurationError>;
}

/// Compile every opaque override without changing policy or provider axes.
/// Work on a clone so a rejected later entry cannot expose partial normalization.
pub fn compile_toolset_configurations(
    compiler: &dyn ToolConfigurationCompiler,
    toolsets: &[awaken_agent_contract::ToolsetPolicy],
    client_tools: &[ClientToolDescriptor],
) -> Result<Vec<awaken_agent_contract::ToolsetPolicy>, ToolConfigurationError> {
    let mut compiled = toolsets.to_vec();
    for (source_index, toolset) in compiled.iter_mut().enumerate() {
        for (override_index, entry) in toolset.overrides.iter_mut().enumerate() {
            if let Some(value) = &entry.configuration {
                let value = compiler
                    .compile(ToolConfigurationInput {
                        source: &toolset.source,
                        tool_name: &entry.name,
                        configuration: value,
                        client_tools,
                    })
                    .map_err(|mut error| {
                        let prefix = format!(
                            "toolsets[{source_index}].overrides[{override_index}].configuration"
                        );
                        error.path = if error.path.is_empty() {
                            prefix
                        } else {
                            format!("{prefix}.{}", error.path)
                        };
                        error
                    })?;
                entry.configuration = Some(value);
            }
        }
    }
    Ok(compiled)
}
