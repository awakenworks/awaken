//! Resolution errors.

/// Errors from the resolution process.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("agent not found: {0}")]
    AgentNotFound(String),
    #[error("model not found: {0}")]
    ModelNotFound(String),
    #[error("provider not found: {0}")]
    ProviderNotFound(String),
    #[error("plugin not found: {0}")]
    PluginNotFound(String),
    #[error("invalid config for plugin {plugin}: section \"{key}\" — {message}")]
    InvalidPluginConfig {
        plugin: String,
        key: String,
        message: String,
    },
    #[error("unsupported remote backend `{backend}` for delegate `{agent_id}`")]
    UnsupportedRemoteBackend { agent_id: String, backend: String },
    #[error(
        "invalid remote endpoint config for delegate `{agent_id}` backend `{backend}` — {message}"
    )]
    InvalidRemoteEndpointConfig {
        agent_id: String,
        backend: String,
        message: String,
    },
    #[error("non-local agent `{0}` cannot be resolved locally — use it as a delegate instead")]
    RemoteAgentNotDirectlyRunnable(String),
    #[error("tool ID conflict: \"{tool_id}\" registered by both {source_a} and {source_b}")]
    ToolIdConflict {
        tool_id: String,
        source_a: String,
        source_b: String,
    },
    #[error("env build error: {0}")]
    EnvBuild(#[from] awaken_contract::StateError),
}
