use crate::registry::SkillRegistry;
use crate::skill_md::parse_skill_md;
use crate::{SkillState, SKILLS_ACTIVE_INSTRUCTIONS_PLUGIN_ID, SKILLS_DISCOVERY_PLUGIN_ID};
use async_trait::async_trait;
use std::sync::Arc;
use tirea_contract::runtime::behavior::{AgentBehavior, PluginOrdering, ReadOnlyContext};
use tirea_contract::runtime::phase::{ActionSet, BeforeInferenceAction};
use tirea_contract::scope::{is_scope_allowed, ScopeDomain};
use tracing::warn;

/// Injects activated skill instructions as hidden tail prompt segments.
#[derive(Clone)]
pub struct ActiveSkillInstructionsPlugin {
    registry: Arc<dyn SkillRegistry>,
}

impl std::fmt::Debug for ActiveSkillInstructionsPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActiveSkillInstructionsPlugin")
            .finish_non_exhaustive()
    }
}

impl ActiveSkillInstructionsPlugin {
    pub fn new(registry: Arc<dyn SkillRegistry>) -> Self {
        Self { registry }
    }

    async fn render_active_instructions(
        &self,
        active_ids: Vec<String>,
        policy: &tirea_contract::runtime::RunPolicy,
    ) -> String {
        let mut rendered = Vec::new();
        let mut ids = active_ids;
        ids.sort();
        ids.dedup();

        for skill_id in ids {
            if !is_scope_allowed(Some(policy), &skill_id, ScopeDomain::Skill) {
                continue;
            }
            let Some(skill) = self.registry.get(&skill_id) else {
                continue;
            };

            let raw = match skill.read_instructions().await {
                Ok(raw) => raw,
                Err(err) => {
                    warn!(skill_id = %skill_id, error = %err, "failed to read active skill instructions");
                    continue;
                }
            };
            let doc = match parse_skill_md(&raw) {
                Ok(doc) => doc,
                Err(err) => {
                    warn!(skill_id = %skill_id, error = %err, "failed to parse active SKILL.md");
                    continue;
                }
            };
            let body = doc.body.trim();
            if body.is_empty() {
                continue;
            }

            rendered.push(format!(
                "<skill_instruction skill=\"{skill_id}\">\n{body}\n</skill_instruction>"
            ));
        }

        if rendered.is_empty() {
            String::new()
        } else {
            format!(
                "<active_skill_instructions>\n{}\n</active_skill_instructions>",
                rendered.join("\n")
            )
        }
    }
}

#[async_trait]
impl AgentBehavior for ActiveSkillInstructionsPlugin {
    fn id(&self) -> &str {
        SKILLS_ACTIVE_INSTRUCTIONS_PLUGIN_ID
    }

    fn ordering(&self) -> PluginOrdering {
        PluginOrdering::after(&[SKILLS_DISCOVERY_PLUGIN_ID])
    }

    tirea_contract::declare_plugin_states!(SkillState);

    async fn before_inference(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        let active: Vec<String> = ctx
            .snapshot_of::<SkillState>()
            .ok()
            .map(|s| s.active.into_iter().collect())
            .unwrap_or_default();
        if active.is_empty() {
            return ActionSet::empty();
        }

        let rendered = self
            .render_active_instructions(active, ctx.run_policy())
            .await;
        if rendered.is_empty() {
            return ActionSet::empty();
        }

        ActionSet::single(BeforeInferenceAction::AddContextMessage(
            tirea_contract::runtime::inference::ContextMessage::suffix_system(
                "active_skill_instructions",
                rendered,
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FsSkill, InMemorySkillRegistry, Skill};
    use std::fs;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tirea_contract::runtime::phase::Phase;
    use tirea_contract::RunPolicy;
    use tirea_state::DocCell;

    fn make_registry(skills: Vec<Arc<dyn Skill>>) -> Arc<dyn SkillRegistry> {
        Arc::new(InMemorySkillRegistry::from_skills(skills))
    }

    fn make_skills() -> (TempDir, Vec<Arc<dyn Skill>>) {
        let td = TempDir::new().unwrap();
        let root = td.path().join("skills");
        fs::create_dir_all(root.join("docx")).unwrap();
        fs::write(
            root.join("docx").join("SKILL.md"),
            "---\nname: docx\ndescription: ok\n---\n# DOCX\nUse docx-js.\n",
        )
        .unwrap();

        let result = FsSkill::discover(root).unwrap();
        (td, FsSkill::into_arc_skills(result.skills))
    }

    #[tokio::test]
    async fn injects_active_skill_instructions_as_suffix_context() {
        let (_td, skills) = make_skills();
        let plugin = ActiveSkillInstructionsPlugin::new(make_registry(skills));
        let config = RunPolicy::new();
        let doc = DocCell::new(serde_json::json!({
            "skills": { "active": ["docx"] }
        }));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

        let actions: Vec<_> = plugin.before_inference(&ctx).await.into_iter().collect();
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            BeforeInferenceAction::AddContextMessage(cm) => {
                assert_eq!(
                    cm.target,
                    tirea_contract::runtime::inference::ContextMessageTarget::SuffixSystem
                );
                assert!(cm.content.contains("<active_skill_instructions>"));
                assert!(cm.content.contains("Use docx-js."));
            }
            _ => panic!("expected AddContextMessage"),
        }
    }

    #[tokio::test]
    async fn no_active_skills_produces_no_actions() {
        let (_td, skills) = make_skills();
        let plugin = ActiveSkillInstructionsPlugin::new(make_registry(skills));
        let config = RunPolicy::new();
        let doc = DocCell::new(serde_json::json!({ "skills": { "active": [] } }));
        let ctx = ReadOnlyContext::new(Phase::BeforeInference, "t1", &[], &config, &doc);

        assert!(plugin.before_inference(&ctx).await.is_empty());
    }
}
