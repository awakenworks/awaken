use super::agent_tools::{AGENT_RECOVERY_PLUGIN_ID, AGENT_TOOLS_PLUGIN_ID};
use crate::composition::{AgentOsWiringError, RegistryBundle};
use crate::contracts::runtime::behavior::AgentBehavior;
use crate::contracts::runtime::tool_call::Tool;
#[cfg(feature = "skills")]
use crate::extensions::skills::SKILLS_BUNDLE_ID;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Default)]
pub(super) struct ResolvedBehaviors {
    global: Vec<Arc<dyn AgentBehavior>>,
    agent_default: Vec<Arc<dyn AgentBehavior>>,
}

impl ResolvedBehaviors {
    pub(super) fn with_global(mut self, plugins: Vec<Arc<dyn AgentBehavior>>) -> Self {
        self.global.extend(plugins);
        self
    }

    pub(super) fn with_agent_default(mut self, plugins: Vec<Arc<dyn AgentBehavior>>) -> Self {
        self.agent_default.extend(plugins);
        self
    }

    pub(super) fn into_plugins(self) -> Result<Vec<Arc<dyn AgentBehavior>>, AgentOsWiringError> {
        let mut plugins = Vec::new();
        plugins.extend(self.global);
        plugins.extend(self.agent_default);
        ensure_unique_behavior_ids(&plugins)?;
        Ok(plugins)
    }
}

pub(super) fn ensure_unique_behavior_ids(
    plugins: &[Arc<dyn AgentBehavior>],
) -> Result<(), AgentOsWiringError> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for p in plugins {
        let id = p.id().to_string();
        if !seen.insert(id.clone()) {
            return Err(AgentOsWiringError::BehaviorAlreadyInstalled(id));
        }
    }
    Ok(())
}

pub(super) fn merge_wiring_bundles(
    bundles: &[Arc<dyn RegistryBundle>],
    tools: &mut HashMap<String, Arc<dyn Tool>>,
) -> Result<Vec<Arc<dyn AgentBehavior>>, AgentOsWiringError> {
    let mut plugins = Vec::new();
    for bundle in bundles {
        validate_wiring_bundle(bundle.as_ref())?;
        merge_wiring_bundle_tools(bundle.as_ref(), tools)?;
        let mut bundle_plugins = collect_wiring_bundle_behaviors(bundle.as_ref())?;
        plugins.append(&mut bundle_plugins);
    }
    ensure_unique_behavior_ids(&plugins)?;
    Ok(plugins)
}

pub(super) fn validate_wiring_bundle(
    bundle: &dyn RegistryBundle,
) -> Result<(), AgentOsWiringError> {
    let unsupported = [
        (
            !bundle.agent_definitions().is_empty(),
            "agent_definitions".to_string(),
        ),
        (
            !bundle.agent_registries().is_empty(),
            "agent_registries".to_string(),
        ),
        (
            !bundle.provider_definitions().is_empty(),
            "provider_definitions".to_string(),
        ),
        (
            !bundle.provider_registries().is_empty(),
            "provider_registries".to_string(),
        ),
        (
            !bundle.model_definitions().is_empty(),
            "model_definitions".to_string(),
        ),
        (
            !bundle.model_registries().is_empty(),
            "model_registries".to_string(),
        ),
    ];
    if let Some((_, kind)) = unsupported.into_iter().find(|(has, _)| *has) {
        return Err(AgentOsWiringError::BundleUnsupportedContribution {
            bundle_id: bundle.id().to_string(),
            kind,
        });
    }
    Ok(())
}

pub(super) fn merge_wiring_bundle_tools(
    bundle: &dyn RegistryBundle,
    tools: &mut HashMap<String, Arc<dyn Tool>>,
) -> Result<(), AgentOsWiringError> {
    let mut defs: Vec<(String, Arc<dyn Tool>)> = bundle.tool_definitions().into_iter().collect();
    defs.sort_by(|a, b| a.0.cmp(&b.0));
    for (id, tool) in defs {
        if tools.contains_key(&id) {
            return Err(wiring_tool_conflict(bundle.id(), id));
        }
        tools.insert(id, tool);
    }

    for reg in bundle.tool_registries() {
        let mut ids = reg.ids();
        ids.sort();
        for id in ids {
            let Some(tool) = reg.get(&id) else {
                continue;
            };
            if tools.contains_key(&id) {
                return Err(wiring_tool_conflict(bundle.id(), id));
            }
            tools.insert(id, tool);
        }
    }
    Ok(())
}

pub(super) fn collect_wiring_bundle_behaviors(
    bundle: &dyn RegistryBundle,
) -> Result<Vec<Arc<dyn AgentBehavior>>, AgentOsWiringError> {
    let mut out = Vec::new();

    let mut defs: Vec<(String, Arc<dyn AgentBehavior>)> =
        bundle.behavior_definitions().into_iter().collect();
    defs.sort_by(|a, b| a.0.cmp(&b.0));
    for (key, behavior) in defs {
        let behavior_id = behavior.id().to_string();
        if key != behavior_id {
            return Err(AgentOsWiringError::BundleBehaviorIdMismatch {
                bundle_id: bundle.id().to_string(),
                key,
                behavior_id,
            });
        }
        out.push(behavior);
    }

    for reg in bundle.behavior_registries() {
        let mut ids = reg.ids();
        ids.sort();
        for id in ids {
            let Some(behavior) = reg.get(&id) else {
                continue;
            };
            if id != behavior.id() {
                return Err(AgentOsWiringError::BundleBehaviorIdMismatch {
                    bundle_id: bundle.id().to_string(),
                    key: id,
                    behavior_id: behavior.id().to_string(),
                });
            }
            out.push(behavior);
        }
    }

    Ok(out)
}

pub(super) fn wiring_tool_conflict(bundle_id: &str, id: String) -> AgentOsWiringError {
    #[cfg(feature = "skills")]
    if bundle_id == SKILLS_BUNDLE_ID {
        return AgentOsWiringError::SkillsToolIdConflict(id);
    }
    if bundle_id == AGENT_TOOLS_PLUGIN_ID || bundle_id == AGENT_RECOVERY_PLUGIN_ID {
        return AgentOsWiringError::AgentToolIdConflict(id);
    }
    AgentOsWiringError::BundleToolIdConflict {
        bundle_id: bundle_id.to_string(),
        id,
    }
}
