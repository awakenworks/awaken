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

    #[error("server '{0}' is disabled")]
    ServerDisabled(String),

    #[error("server '{0}' is permanently failed")]
    ServerPermanentlyFailed(String),
}

impl From<McpTransportError> for McpError {
    fn from(e: McpTransportError) -> Self {
        Self::Transport(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_server_name_display() {
        let err = McpError::EmptyServerName;
        assert_eq!(err.to_string(), "server name must be non-empty");
    }

    #[test]
    fn duplicate_server_name_display() {
        let err = McpError::DuplicateServerName("my-server".to_string());
        assert_eq!(err.to_string(), "duplicate server name: my-server");
    }

    #[test]
    fn unknown_server_display() {
        let err = McpError::UnknownServer("missing".to_string());
        assert_eq!(err.to_string(), "unknown mcp server: missing");
    }

    #[test]
    fn unsupported_capability_display() {
        let err = McpError::UnsupportedCapability {
            server_name: "srv".to_string(),
            capability: "prompts",
        };
        assert_eq!(err.to_string(), "mcp server 'srv' does not support prompts");
    }

    #[test]
    fn invalid_tool_id_component_display() {
        let err = McpError::InvalidToolIdComponent("---".to_string());
        assert_eq!(
            err.to_string(),
            "invalid tool id component after sanitization: ---"
        );
    }

    #[test]
    fn tool_id_conflict_display() {
        let err = McpError::ToolIdConflict("mcp__srv__echo".to_string());
        assert_eq!(
            err.to_string(),
            "tool id already registered: mcp__srv__echo"
        );
    }

    #[test]
    fn transport_error_display() {
        let err = McpError::Transport("connection failed".to_string());
        assert_eq!(err.to_string(), "mcp transport error: connection failed");
    }

    #[test]
    fn invalid_refresh_interval_display() {
        let err = McpError::InvalidRefreshInterval;
        assert_eq!(err.to_string(), "periodic refresh interval must be > 0");
    }

    #[test]
    fn periodic_refresh_already_running_display() {
        let err = McpError::PeriodicRefreshAlreadyRunning;
        assert_eq!(err.to_string(), "periodic refresh loop is already running");
    }

    #[test]
    fn runtime_unavailable_display() {
        let err = McpError::RuntimeUnavailable;
        assert_eq!(
            err.to_string(),
            "tokio runtime is required to start periodic refresh"
        );
    }

    #[test]
    fn from_transport_error_conversion() {
        let transport_err = McpTransportError::TransportError("io fail".to_string());
        let mcp_err: McpError = transport_err.into();
        assert!(matches!(mcp_err, McpError::Transport(msg) if msg.contains("io fail")));
    }

    #[test]
    fn from_unknown_tool_error_conversion() {
        let transport_err = McpTransportError::UnknownTool("missing".to_string());
        let mcp_err: McpError = transport_err.into();
        assert!(matches!(mcp_err, McpError::Transport(msg) if msg.contains("missing")));
    }

    #[test]
    fn from_timeout_error_conversion() {
        let transport_err = McpTransportError::Timeout("30s".to_string());
        let mcp_err: McpError = transport_err.into();
        assert!(matches!(mcp_err, McpError::Transport(msg) if msg.contains("30s")));
    }

    #[test]
    fn server_disabled_display() {
        let err = McpError::ServerDisabled("srv".to_string());
        assert_eq!(err.to_string(), "server 'srv' is disabled");
    }

    #[test]
    fn server_permanently_failed_display() {
        let err = McpError::ServerPermanentlyFailed("srv".to_string());
        assert_eq!(err.to_string(), "server 'srv' is permanently failed");
    }
}
