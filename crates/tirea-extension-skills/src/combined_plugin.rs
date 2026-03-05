use crate::types::SkillState;
use crate::{SkillDiscoveryPlugin, SkillRuntimePlugin, SKILLS_PLUGIN_ID};
use async_trait::async_trait;
use std::sync::Arc;
use tirea_contract::runtime::behavior::{AgentBehavior, ReadOnlyContext};
use tirea_contract::runtime::phase::{ActionSet, BeforeInferenceAction};

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

#[async_trait]
impl AgentBehavior for SkillPlugin {
    fn id(&self) -> &str {
        SKILLS_PLUGIN_ID
    }

    tirea_contract::declare_plugin_states!(SkillState);

    async fn before_inference(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        AgentBehavior::before_inference(&self.discovery, ctx)
            .await
            .and(AgentBehavior::before_inference(&self.runtime, ctx).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FsSkill, InMemorySkillRegistry, Skill, SkillRegistry};
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;
    use tirea_contract::runtime::phase::Phase;
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
        let actions = AgentBehavior::before_inference(&plugin, &ctx).await;

        // Only discovery catalog is injected; runtime plugin no longer injects system context.
        let system_context_count = actions
            .as_slice()
            .iter()
            .filter(|a| matches!(a, BeforeInferenceAction::AddSystemContext(_)))
            .count();
        assert_eq!(system_context_count, 1);
    }
}
