use crate::composition::{
    AgentOsWiringError, RegistryBundle, SkillsConfig, SystemWiring, ToolBehaviorBundle,
    WiringContext,
};
use crate::contracts::runtime::behavior::AgentBehavior;
use std::collections::HashMap;
use std::sync::Arc;
use tirea_extension_skills::{
    InMemorySkillRegistry, SkillDiscoveryPlugin, SkillRegistry, SkillSubsystem,
    SkillSubsystemError, SKILLS_BUNDLE_ID, SKILLS_DISCOVERY_PLUGIN_ID, SKILLS_PLUGIN_ID,
};

pub(crate) struct SkillsSystemWiring {
    registry: Arc<dyn SkillRegistry>,
    config: SkillsConfig,
}

impl SkillsSystemWiring {
    pub(crate) fn new(registry: Arc<dyn SkillRegistry>, config: SkillsConfig) -> Self {
        Self { registry, config }
    }

    fn freeze_registry(&self) -> Arc<dyn SkillRegistry> {
        let mut frozen = InMemorySkillRegistry::new();
        frozen.extend_upsert(self.registry.snapshot().into_values().collect());
        Arc::new(frozen) as Arc<dyn SkillRegistry>
    }

    fn build_plugins(&self, registry: Arc<dyn SkillRegistry>) -> Vec<Arc<dyn AgentBehavior>> {
        if !self.config.advertise_catalog {
            return Vec::new();
        }
        let discovery = SkillDiscoveryPlugin::new(registry).with_limits(
            self.config.discovery_max_entries,
            self.config.discovery_max_chars,
        );
        vec![Arc::new(discovery)]
    }
}

impl SystemWiring for SkillsSystemWiring {
    fn id(&self) -> &str {
        "skills"
    }

    fn reserved_behavior_ids(&self) -> &[&'static str] {
        &[SKILLS_PLUGIN_ID, SKILLS_DISCOVERY_PLUGIN_ID]
    }

    fn wire(
        &self,
        ctx: &WiringContext<'_>,
    ) -> Result<Vec<Arc<dyn RegistryBundle>>, AgentOsWiringError> {
        // Ensure no user-installed behavior collides with reserved skills IDs.
        let reserved = self.reserved_behavior_ids();
        if let Some(existing) = ctx
            .resolved_behaviors
            .iter()
            .map(|p| p.id())
            .find(|id| reserved.contains(id))
        {
            return Err(AgentOsWiringError::SkillsBehaviorAlreadyInstalled(
                existing.to_string(),
            ));
        }

        let frozen = self.freeze_registry();

        let mut subsystem = SkillSubsystem::new(frozen.clone());
        #[cfg(feature = "permission")]
        {
            use tirea_extension_permission::PermissionOverrideGranter;
            subsystem =
                subsystem.with_access_granter(std::sync::Arc::new(PermissionOverrideGranter));
        }
        let mut tool_defs = HashMap::new();
        subsystem
            .extend_tools(&mut tool_defs)
            .map_err(|e| match e {
                SkillSubsystemError::ToolIdConflict(id) => {
                    AgentOsWiringError::SkillsToolIdConflict(id)
                }
            })?;

        // Check tool conflicts with existing tools.
        for id in tool_defs.keys() {
            if ctx.existing_tools.contains_key(id) {
                return Err(AgentOsWiringError::SkillsToolIdConflict(id.clone()));
            }
        }

        let mut bundle = ToolBehaviorBundle::new(SKILLS_BUNDLE_ID).with_tools(tool_defs);
        for plugin in self.build_plugins(frozen) {
            bundle = bundle.with_behavior(plugin);
        }
        Ok(vec![Arc::new(bundle)])
    }
}
