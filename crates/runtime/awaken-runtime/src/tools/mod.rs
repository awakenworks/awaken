//! Application-defined typed tools, independent of the builtin extensions.

mod erasure;
pub use awaken_tool_contract::tool::{Tool, ToolError, ToolOutput};
pub use erasure::{Erased, erase, erase_for};

use crate::{ConfigurationError, Runtime};
use awaken_runtime_contract::resolved::{ToolDescriptor, ToolSchemaError};
use awaken_tool_contract::tool::RawTool;
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

impl Runtime {
    /// Register one typed tool and return its generated Snapshot descriptor.
    /// Schema failure is reported before mutating the executable registry.
    pub fn register_tool<T: Tool + 'static>(
        &mut self,
        namespace: &str,
        tool: T,
    ) -> Result<ToolDescriptor, awaken_runtime_contract::execution::Error> {
        use awaken_runtime_contract::diagnostic::{RuntimeDiagnostic, RuntimeErrorKind};
        self.validate_configuration()?;
        let registration = ToolRegistration::new(namespace, tool).map_err(|error| {
            RuntimeDiagnostic::new(
                RuntimeErrorKind::InvalidConfiguration,
                "invalid_tool_schema",
                error,
            )
        })?;
        if !self.tools.insert(registration.executable()) {
            return Err(ConfigurationError::DuplicateTool {
                id: registration.descriptor.id.clone(),
            }
            .into());
        }
        Ok(registration.descriptor)
    }

    /// Register the executable paired with the descriptor added to the Snapshot.
    pub fn with_registered_tool(
        self,
        registration: &ToolRegistration,
    ) -> Result<Self, ConfigurationError> {
        self.try_with_tool(registration.executable())
    }
}
