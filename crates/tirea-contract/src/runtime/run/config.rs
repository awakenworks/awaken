use super::options::RunPolicy;
use crate::runtime::extensions::Extensions;

/// Layered runtime configuration accessible to plugins and tools.
///
/// Bridges build-time static config with runtime mutable state. Carries
/// plugin-specific typed extensions via [`Extensions`] TypeMap.
///
/// Created during resolve, mutable until execution starts, then frozen
/// as `Arc<AgentRunConfig>` for the duration of the run.
#[derive(Default)]
pub struct AgentRunConfig {
    policy: RunPolicy,
    agent_id: String,
    model: String,
    extensions: Extensions,
}

impl AgentRunConfig {
    pub fn new(policy: RunPolicy) -> Self {
        Self {
            policy,
            agent_id: String::new(),
            model: String::new(),
            extensions: Extensions::new(),
        }
    }

    pub fn policy(&self) -> &RunPolicy {
        &self.policy
    }

    pub fn policy_mut(&mut self) -> &mut RunPolicy {
        &mut self.policy
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub fn set_agent_id(&mut self, id: &str) {
        self.agent_id = id.to_string();
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn set_model(&mut self, model: &str) {
        self.model = model.to_string();
    }

    pub fn extensions(&self) -> &Extensions {
        &self.extensions
    }

    pub fn extensions_mut(&mut self) -> &mut Extensions {
        &mut self.extensions
    }
}
