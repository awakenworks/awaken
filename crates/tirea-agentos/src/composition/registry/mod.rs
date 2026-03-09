mod agent;
mod behavior;
mod model;
mod provider;
mod stop_policy;
mod tool;
pub mod traits;

use std::collections::HashMap;
use std::sync::Arc;

#[cfg(feature = "skills")]
use crate::extensions::skills::SkillRegistry;

pub use stop_policy::StopPolicyRegistry;
pub use traits::{AgentRegistry, BehaviorRegistry, ModelRegistry, ProviderRegistry, ToolRegistry};

pub use agent::{CompositeAgentRegistry, InMemoryAgentRegistry};
pub use behavior::{CompositeBehaviorRegistry, InMemoryBehaviorRegistry};
pub use model::{CompositeModelRegistry, InMemoryModelRegistry};
pub use provider::{CompositeProviderRegistry, InMemoryProviderRegistry};
pub use stop_policy::{
    CompositeStopPolicyRegistry, InMemoryStopPolicyRegistry, StopPolicyRegistryError,
};
pub use tool::{CompositeToolRegistry, InMemoryToolRegistry};

pub use traits::{
    AgentRegistryError, BehaviorRegistryError, ModelDefinition, ModelRegistryError,
    ProviderRegistryError, RegistryBundle, ToolRegistryError,
};

pub(crate) fn sorted_registry_ids<T>(entries: &HashMap<String, T>) -> Vec<String> {
    let mut ids: Vec<String> = entries.keys().cloned().collect();
    ids.sort();
    ids
}

/// Aggregated registry set used by [`crate::runtime::AgentOs`] after build-time composition.
#[derive(Clone)]
pub struct RegistrySet {
    pub agents: Arc<dyn AgentRegistry>,
    pub tools: Arc<dyn ToolRegistry>,
    pub behaviors: Arc<dyn BehaviorRegistry>,
    pub providers: Arc<dyn ProviderRegistry>,
    pub models: Arc<dyn ModelRegistry>,
    pub stop_policies: Arc<dyn StopPolicyRegistry>,
    #[cfg(feature = "skills")]
    pub skills: Option<Arc<dyn SkillRegistry>>,
}

impl RegistrySet {
    pub fn new(
        agents: Arc<dyn AgentRegistry>,
        tools: Arc<dyn ToolRegistry>,
        behaviors: Arc<dyn BehaviorRegistry>,
        providers: Arc<dyn ProviderRegistry>,
        models: Arc<dyn ModelRegistry>,
        stop_policies: Arc<dyn StopPolicyRegistry>,
        #[cfg(feature = "skills")] skills: Option<Arc<dyn SkillRegistry>>,
    ) -> Self {
        Self {
            agents,
            tools,
            behaviors,
            providers,
            models,
            stop_policies,
            #[cfg(feature = "skills")]
            skills,
        }
    }
}
