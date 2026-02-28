use crate::{SkillDiscoveryPlugin, SkillRuntimePlugin, SKILLS_PLUGIN_ID};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tirea_contract::runtime::plugin::agent::{AgentBehavior, ReadOnlyContext};
use tirea_contract::runtime::plugin::phase::effect::PhaseOutput;
use tirea_contract::runtime::plugin::phase::{AnyPluginAction, Phase};
use tirea_state::TrackedPatch;

/// Single plugin wrapper that injects both:
/// - the skills catalog (discovery)
/// - activated skill instructions/materials (runtime)
///
/// This is a convenience so callers can register one plugin instead of two.
#[derive(Debug, Clone)]
pub struct SkillPlugin {
    discovery: SkillDiscoveryPlugin,
    runtime: SkillRuntimePlugin,
}

impl SkillPlugin {
    pub fn new(discovery: SkillDiscoveryPlugin) -> Self {
        Self {
            discovery,
            runtime: SkillRuntimePlugin::new(),
        }
    }

    pub fn with_runtime(mut self, runtime: SkillRuntimePlugin) -> Self {
        self.runtime = runtime;
        self
    }

    pub fn boxed(self) -> Arc<dyn AgentBehavior> {
        Arc::new(self)
    }

    pub fn into_agent(self) -> Arc<dyn AgentBehavior> {
        Arc::new(self)
    }
}

fn merge_output(target: &mut PhaseOutput, source: PhaseOutput) {
    target.effects.extend(source.effects);
    target.state_actions.extend(source.state_actions);
}

#[async_trait]
impl AgentBehavior for SkillPlugin {
    fn id(&self) -> &str {
        SKILLS_PLUGIN_ID
    }

    async fn before_inference(&self, ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        let mut merged = PhaseOutput::default();
        merge_output(
            &mut merged,
            AgentBehavior::before_inference(&self.discovery, ctx).await,
        );
        merge_output(
            &mut merged,
            AgentBehavior::before_inference(&self.runtime, ctx).await,
        );
        merged
    }

    async fn phase_actions(&self, phase: Phase, ctx: &ReadOnlyContext<'_>) -> Vec<AnyPluginAction> {
        let mut merged = AgentBehavior::phase_actions(&self.discovery, phase, ctx).await;
        merged.extend(AgentBehavior::phase_actions(&self.runtime, phase, ctx).await);
        merged
    }

    fn reduce_plugin_actions(
        &self,
        actions: Vec<AnyPluginAction>,
        base_snapshot: &Value,
    ) -> Result<Vec<TrackedPatch>, String> {
        let mut discovery_actions = Vec::new();
        let mut runtime_actions = Vec::new();
        let discovery_ids = self.discovery.behavior_ids();
        let runtime_ids = self.runtime.behavior_ids();

        for action in actions {
            let plugin_id = action.plugin_id();
            if discovery_ids.iter().any(|id| *id == plugin_id) {
                discovery_actions.push(action);
            } else if runtime_ids.iter().any(|id| *id == plugin_id) {
                runtime_actions.push(action);
            } else {
                return Err(format!(
                    "skill plugin cannot route action '{}' for plugin '{}'",
                    action.action_type_name(),
                    plugin_id
                ));
            }
        }

        let mut merged = Vec::new();
        merged.extend(AgentBehavior::reduce_plugin_actions(
            &self.discovery,
            discovery_actions,
            base_snapshot,
        )?);
        merged.extend(AgentBehavior::reduce_plugin_actions(
            &self.runtime,
            runtime_actions,
            base_snapshot,
        )?);
        Ok(merged)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FsSkill, InMemorySkillRegistry, Skill, SkillRegistry};
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;
    use tirea_contract::runtime::plugin::phase::effect::PhaseEffect;
    use tirea_contract::runtime::plugin::phase::Phase;
    use tirea_contract::RunConfig;
    use tirea_state::DocCell;

    #[tokio::test]
    async fn combined_plugin_injects_catalog_only() {
        let td = TempDir::new().unwrap();
        let root = td.path().join("skills");
        fs::create_dir_all(root.join("s1")).unwrap();
        fs::write(
            root.join("s1").join("SKILL.md"),
            "---\nname: s1\ndescription: ok\n---\nDo X\n",
        )
        .unwrap();

        let result = FsSkill::discover(root).unwrap();
        let skills: Vec<Arc<dyn Skill>> = FsSkill::into_arc_skills(result.skills);
        let registry: Arc<dyn SkillRegistry> = Arc::new(InMemorySkillRegistry::from_skills(skills));
        let discovery = SkillDiscoveryPlugin::new(registry);
        let plugin = SkillPlugin::new(discovery);

        let config = RunConfig::new();
        let doc = DocCell::new(json!({
            "skills": {
                "active": ["s1"],
                "instructions": {"s1": "Do X"},
                "references": {
                    "s1:references/a.md": {
                        "skill":"s1",
                        "path":"references/a.md",
                        "sha256":"x",
                        "truncated":false,
                        "content":"A",
                        "bytes":1
                    }
                },
                "scripts": {}
            }
        }));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);
        let output = AgentBehavior::before_inference(&plugin, &ctx).await;

        // Only discovery catalog is injected; runtime plugin no longer injects system context.
        let system_contexts: Vec<&str> = output
            .effects
            .iter()
            .filter_map(|e| match e {
                PhaseEffect::SystemContext(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(system_contexts.len(), 1);
        assert!(system_contexts[0].contains("<available_skills>"));
    }
}
