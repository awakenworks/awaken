//! Agent resolution: dynamic lookup of agent config + execution environment.

use crate::agent::config::AgentConfig;
use crate::error::StateError;

use super::ExecutionEnv;

/// A fully resolved agent: config + execution environment.
///
/// Produced by `AgentResolver::resolve()`. Contains live references
/// (LlmExecutor, tools, plugins) — not serializable.
pub struct ResolvedAgent {
    pub config: AgentConfig,
    pub env: ExecutionEnv,
}

/// Resolves an agent by ID, producing a ready-to-execute config + environment.
///
/// Implementations look up `AgentSpec` from a registry, resolve the model → provider
/// chain to obtain `LlmExecutor`, filter tools, install plugins, and build the
/// `ExecutionEnv`. The loop runner calls this at startup and at step boundaries
/// when handoff is detected (via `ActiveAgentKey`).
pub trait AgentResolver: Send + Sync {
    fn resolve(&self, agent_id: &str) -> Result<ResolvedAgent, StateError>;
}
