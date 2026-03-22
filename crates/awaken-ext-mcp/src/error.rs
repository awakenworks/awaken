//! Error types for the MCP extension crate.

use mcp::transport::McpTransportError;

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("server name must be non-empty")]
    EmptyServerName,

    #[error("duplicate server name: {0}")]
    DuplicateServerName(String),

    #[error("unknown mcp server: {0}")]
    UnknownServer(String),

    #[error("mcp server '{server_name}' does not support {capability}")]
    UnsupportedCapability {
        server_name: String,
        capability: &'static str,
    },

    #[error("invalid tool id component after sanitization: {0}")]
    InvalidToolIdComponent(String),

    #[error("tool id already registered: {0}")]
    ToolIdConflict(String),

    #[error("mcp transport error: {0}")]
    Transport(String),

    #[error("periodic refresh interval must be > 0")]
    InvalidRefreshInterval,

    #[error("periodic refresh loop is already running")]
    PeriodicRefreshAlreadyRunning,

    #[error("tokio runtime is required to start periodic refresh")]
    RuntimeUnavailable,
}

impl From<McpTransportError> for McpError {
    fn from(e: McpTransportError) -> Self {
        Self::Transport(e.to_string())
    }
}
