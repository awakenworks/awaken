use std::sync::Arc;

use super::{team_tools, TeamPlugin, TeamStores, TEAM_PLUGIN_ID};
use crate::composition::{
    AgentOsWiringError, RegistryBundle, SystemWiring, ToolBehaviorBundle, WiringContext,
};
use crate::contracts::runtime::AgentBehavior;

/// Wires team collaboration tools and the TeamPlugin into every agent.
pub struct TeamWiring {
    stores: TeamStores,
}

impl TeamWiring {
    pub fn new(stores: TeamStores) -> Self {
        Self { stores }
    }
}

impl SystemWiring for TeamWiring {
    fn id(&self) -> &str {
        "team_collab"
    }

    fn reserved_behavior_ids(&self) -> &[&'static str] {
        &[TEAM_PLUGIN_ID]
    }

    fn wire(
        &self,
        _ctx: &WiringContext<'_>,
    ) -> Result<Vec<Arc<dyn RegistryBundle>>, AgentOsWiringError> {
        let tools = team_tools(&self.stores);
        let plugin: Arc<dyn AgentBehavior> = Arc::new(TeamPlugin::new(self.stores.clone()));

        let mut bundle = ToolBehaviorBundle::new(TEAM_PLUGIN_ID).with_behavior(plugin);
        for tool in tools {
            bundle = bundle.with_tool(tool);
        }

        Ok(vec![Arc::new(bundle)])
    }
}
