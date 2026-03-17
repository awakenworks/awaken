use crate::composition::{
    AgentOsWiringError, RegistryBundle, SystemWiring, ToolBehaviorBundle, WiringContext,
};
use std::sync::Arc;
use tirea_extension_plan::{
    EnterPlanModeTool, ExitPlanModeTool, PlanModePlugin, PLAN_MODE_PLUGIN_ID,
};

const PLAN_BUNDLE_ID: &str = "plan_mode";

pub(crate) struct PlanSystemWiring;

impl PlanSystemWiring {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl SystemWiring for PlanSystemWiring {
    fn id(&self) -> &str {
        "plan"
    }

    fn reserved_behavior_ids(&self) -> &[&'static str] {
        &[PLAN_MODE_PLUGIN_ID]
    }

    fn wire(
        &self,
        ctx: &WiringContext<'_>,
    ) -> Result<Vec<Arc<dyn RegistryBundle>>, AgentOsWiringError> {
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

        let enter_tool = Arc::new(EnterPlanModeTool::new());
        let exit_tool = Arc::new(ExitPlanModeTool::new());

        let bundle = ToolBehaviorBundle::new(PLAN_BUNDLE_ID)
            .with_tool(enter_tool)
            .with_tool(exit_tool)
            .with_behavior(Arc::new(PlanModePlugin::new()));

        Ok(vec![Arc::new(bundle)])
    }
}
