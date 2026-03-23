//! Resolution: `agent_id` + `RegistrySet` -> `ResolvedRun` / `ResolvedAgent`.

mod error;
mod pipeline;

use std::collections::HashMap;
use std::sync::Arc;

use crate::phase::ExecutionEnv;
use crate::plugins::Plugin;
use awaken_contract::contract::executor::LlmExecutor;
use awaken_contract::contract::tool::Tool;
use awaken_contract::registry_spec::AgentSpec;

pub use error::ResolveError;
pub(crate) use pipeline::RegistrySetResolver;
pub(crate) use pipeline::inject_default_plugins;

// ---------------------------------------------------------------------------
// ResolvedRun
// ---------------------------------------------------------------------------

/// Fully resolved agent run — holds live references, not serializable.
///
/// Produced by [`resolve`] from a [`RegistrySet`] and an agent ID.
/// Passed to the loop runner as the single runtime configuration.
impl std::fmt::Debug for ResolvedRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedRun")
            .field("spec", &self.spec)
            .field("model_name", &self.model_name)
            .field("tools", &self.tools.keys().collect::<Vec<_>>())
            .field("plugins_count", &self.plugins.len())
            .finish_non_exhaustive()
    }
}

struct ResolvedRun {
    /// The source agent definition.
    pub spec: AgentSpec,
    /// Resolved LLM executor.
    pub executor: Arc<dyn LlmExecutor>,
    /// Actual model name for API calls.
    pub model_name: String,
    /// Resolved tools (after allow/exclude filtering).
    pub tools: HashMap<String, Arc<dyn Tool>>,
    /// Resolved plugins.
    pub plugins: Vec<Arc<dyn Plugin>>,
    /// Execution environment built from resolved plugins.
    pub env: ExecutionEnv,
}
