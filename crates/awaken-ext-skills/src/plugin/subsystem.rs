use std::sync::Arc;

use crate::registry::SkillRegistry;
use crate::{SKILL_ACTIVATE_TOOL_ID, SKILL_LOAD_RESOURCE_TOOL_ID, SKILL_SCRIPT_TOOL_ID};

use super::{ActiveSkillInstructionsPlugin, SkillDiscoveryPlugin};

/// High-level facade for wiring skills into an agent.
#[derive(Clone)]
pub struct SkillSubsystem {
    registry: Arc<dyn SkillRegistry>,
}

impl std::fmt::Debug for SkillSubsystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillSubsystem").finish_non_exhaustive()
    }
}

/// Errors returned when wiring the skills subsystem into an agent.
#[derive(Debug, thiserror::Error)]
pub enum SkillSubsystemError {
    #[error("tool id already registered: {0}")]
    ToolIdConflict(String),
}

impl SkillSubsystem {
    pub fn new(registry: Arc<dyn SkillRegistry>) -> Self {
        Self { registry }
    }

    pub fn registry(&self) -> &Arc<dyn SkillRegistry> {
        &self.registry
    }

    /// Build the discovery plugin (injects skills catalog before inference).
    pub fn discovery_plugin(&self) -> SkillDiscoveryPlugin {
        SkillDiscoveryPlugin::new(self.registry.clone())
    }

    /// Build the active instructions plugin (injects active skill instructions).
    pub fn active_instructions_plugin(&self) -> ActiveSkillInstructionsPlugin {
        ActiveSkillInstructionsPlugin::new(self.registry.clone())
    }

    /// Construct the skills tools map.
    pub fn tools(
        &self,
    ) -> std::collections::HashMap<String, Arc<dyn awaken_contract::contract::tool::Tool>> {
        let mut out: std::collections::HashMap<
            String,
            Arc<dyn awaken_contract::contract::tool::Tool>,
        > = std::collections::HashMap::new();
        let _ = self.extend_tools(&mut out);
        out
    }

    /// Add skills tools to an existing tool map.
    pub fn extend_tools(
        &self,
        tools: &mut std::collections::HashMap<
            String,
            Arc<dyn awaken_contract::contract::tool::Tool>,
        >,
    ) -> Result<(), SkillSubsystemError> {
        use crate::tools;
        use awaken_contract::contract::tool::Tool;

        let registry = self.registry.clone();
        let tool_defs: Vec<Arc<dyn Tool>> = vec![
            Arc::new(tools::SkillActivateTool::new(registry.clone())),
            Arc::new(tools::LoadSkillResourceTool::new(registry.clone())),
            Arc::new(tools::SkillScriptTool::new(registry)),
        ];

        for t in tool_defs {
            let id = t.descriptor().id.clone();
            if tools.contains_key(&id) {
                return Err(SkillSubsystemError::ToolIdConflict(id));
            }
            tools.insert(id, t);
        }

        // Ensure expected IDs remain present as a cheap invariant check.
        debug_assert!(tools.contains_key(SKILL_ACTIVATE_TOOL_ID));
        debug_assert!(tools.contains_key(SKILL_LOAD_RESOURCE_TOOL_ID));
        debug_assert!(tools.contains_key(SKILL_SCRIPT_TOOL_ID));

        Ok(())
    }
}
