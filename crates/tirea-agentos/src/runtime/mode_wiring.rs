use crate::composition::{
    AgentOsWiringError, RegistryBundle, SystemWiring, ToolBehaviorBundle, WiringContext,
};
use crate::extensions::mode::{ModePlugin, SwitchModeTool, MODE_PLUGIN_ID};
use std::sync::Arc;

const MODE_BUNDLE_ID: &str = "agent_mode";

pub(crate) struct ModeSystemWiring;

impl ModeSystemWiring {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl SystemWiring for ModeSystemWiring {
    fn id(&self) -> &str {
        "mode"
    }

    fn reserved_behavior_ids(&self) -> &[&'static str] {
        &[MODE_PLUGIN_ID]
    }

    fn wire(
        &self,
        ctx: &WiringContext<'_>,
    ) -> Result<Vec<Arc<dyn RegistryBundle>>, AgentOsWiringError> {
        // Only wire mode tools when the agent has modes configured.
        if ctx.agent_definition.modes.is_empty() {
            return Ok(Vec::new());
        }

        let reserved = self.reserved_behavior_ids();
        if let Some(existing) = ctx
            .resolved_behaviors
            .iter()
            .map(|p| p.id())
            .find(|id| reserved.contains(id))
        {
            return Err(AgentOsWiringError::BehaviorAlreadyInstalled(
                existing.to_string(),
            ));
        }

        let switch_tool = Arc::new(SwitchModeTool::new());

        let bundle = ToolBehaviorBundle::new(MODE_BUNDLE_ID)
            .with_tool(switch_tool)
            .with_behavior(Arc::new(ModePlugin::new()));

        Ok(vec![Arc::new(bundle)])
    }
}
