//! Application-defined typed tools, independent of the builtin extensions.

pub use awaken_tool_contract::tool::{RawTool, Tool, ToolConcurrency, ToolError, ToolOutput};
pub use awaken_tool_contract::typed::{Erased, erase, erase_for};

use crate::{ConfigurationError, Runtime};
use awaken_runtime_contract::resolved::{ToolDescriptor, ToolSchemaError};
use std::sync::Arc;

/// One tool definition supplies both advertised schema and executable behavior.
#[derive(Clone)]
pub struct ToolRegistration {
    descriptor: ToolDescriptor,
    executable: Arc<dyn RawTool>,
}

impl ToolRegistration {
    /// `namespace` is the descriptor content owner, not an authorization grant.
    pub fn new<T: Tool + 'static>(namespace: &str, tool: T) -> Result<Self, ToolSchemaError> {
        Ok(Self {
            descriptor: ToolDescriptor::try_for_tool::<T>(namespace)?,
            executable: erase(tool),
        })
    }

    pub fn descriptor(&self) -> &ToolDescriptor {
        &self.descriptor
    }

    pub fn executable(&self) -> Arc<dyn RawTool> {
        self.executable.clone()
    }
}

impl ToolRegistration {
    /// Install the paired implementation, retaining duplicate-ID diagnostics.
    pub fn install(&self, runtime: Runtime) -> Result<Runtime, ConfigurationError> {
        runtime.try_with_tool(self.executable())
    }
}
