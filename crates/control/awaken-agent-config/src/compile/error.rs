/// A compilation failure, before anything is published (the design's Failure
/// Rules: reject, never partially publish).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CompileError {
    #[error("agent {agent} references unknown tool {tool:?}")]
    UnknownTool { agent: String, tool: String },
    #[error("serialize config: {0}")]
    Serialize(String),
    #[error("invalid resolution manifest: {0}")]
    InvalidResolution(String),
    /// The config's model is still `ModelSelection::Auto` (ADR-0052 D5): compile is
    /// pure and cannot reach the provider catalog, so an `Auto` binding must be
    /// resolved to a concrete one *before* compile (publish does this). Fail-closed.
    #[error("agent {agent} has an unresolved (auto) model binding; resolve it before compiling")]
    UnresolvedModel { agent: String },
    /// A tool override (ADR-0053) is inconsistent: its `target` names no selected tool
    /// (and is not an MCP id), or its `alias` collides with another tool's model-facing
    /// id. Fail-closed — a presentation that would show the model a phantom or duplicate
    /// tool is rejected at compile, not silently dropped.
    #[error("agent {agent} has an invalid tool override: {reason}")]
    InvalidToolOverride { agent: String, reason: String },
    #[error("agent {agent} has an invalid tool-exposure policy: {reason}")]
    InvalidToolExposure { agent: String, reason: String },
    #[error("agent {agent} has invalid tool-discovery settings: {reason}")]
    InvalidToolDiscovery { agent: String, reason: String },
    #[error("agent {agent} has an invalid tool recovery policy: {reason}")]
    InvalidToolRecovery { agent: String, reason: String },
    /// A Managed-Agents integration union cannot be normalized into an executable
    /// binding. Reject it at publish instead of preserving UI-only configuration.
    #[error("agent {agent} has an invalid {axis} binding: {reason}")]
    InvalidBinding {
        agent: String,
        axis: &'static str,
        reason: String,
    },
    /// The config declares a capability its execution kind cannot honor (ADR-0057 D2):
    /// an A2A (remote) agent owns its own skills/MCP on the far side, so declaring them
    /// locally is a silent no-op at runtime — rejected at publish instead. `axis` is the
    /// unsupported capability (`"skills"` / `"mcp_servers"`).
    #[error("agent {agent} is a remote (a2a) agent and cannot honor local {axis}")]
    UnsupportedCapability { agent: String, axis: &'static str },
    #[error("agent {agent} has inconsistent published model candidates: {reason}")]
    InvalidResolvedModels { agent: String, reason: String },
}

impl CompileError {
    /// The config field this error is about — the domain saying which part failed, so the
    /// validate surface can route the issue to the right section instead of re-deriving it
    /// from the message string. `""` means the whole config.
    #[must_use]
    pub fn field_path(&self) -> &'static str {
        match self {
            CompileError::UnknownTool { .. } => "tools",
            CompileError::UnresolvedModel { .. } => "model",
            CompileError::InvalidToolOverride { .. } => "tool_overrides",
            CompileError::InvalidToolExposure { .. } => "tool_exposure",
            CompileError::InvalidToolDiscovery { .. } => "tool_discovery",
            CompileError::InvalidToolRecovery { .. } => "recovery_policies",
            CompileError::InvalidBinding { axis, .. } => axis,
            CompileError::UnsupportedCapability { axis, .. } => axis,
            CompileError::InvalidResolvedModels { .. } => "model",
            CompileError::Serialize(_) | CompileError::InvalidResolution(_) => "",
        }
    }
}
