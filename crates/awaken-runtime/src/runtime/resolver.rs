//! Agent resolution: dynamic lookup of agent config + execution environment.

use crate::agent::config::AgentConfig;
use crate::error::RuntimeError;

use super::ExecutionEnv;

/// A fully resolved agent: config + execution environment.
///
/// Produced by `AgentResolver::resolve()`. Contains live references
/// (LlmExecutor, tools, plugins) — not serializable.
pub struct ResolvedAgent {
    pub config: AgentConfig,
    pub env: ExecutionEnv,
}

impl std::fmt::Debug for ResolvedAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedAgent")
            .field("agent_id", &self.config.id)
            .finish_non_exhaustive()
    }
}

/// Resolves an agent by ID, producing a ready-to-execute config + environment.
///
/// Implementations look up `AgentSpec` from a registry, resolve the model → provider
/// chain to obtain `LlmExecutor`, filter tools, install plugins, and build the
/// `ExecutionEnv`. The loop runner calls this at startup and at step boundaries
/// when handoff is detected (via `ActiveAgentKey`).
pub trait AgentResolver: Send + Sync {
    fn resolve(&self, agent_id: &str) -> Result<ResolvedAgent, RuntimeError>;

    /// List known agent IDs for discovery endpoints.
    ///
    /// Implementations that cannot enumerate agents may return an empty list.
    fn agent_ids(&self) -> Vec<String> {
        Vec::new()
    }
}
