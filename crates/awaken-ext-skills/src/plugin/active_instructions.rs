use std::sync::Arc;

use async_trait::async_trait;

use awaken_contract::StateError;
use awaken_contract::contract::context_message::ContextMessage;
use awaken_contract::model::Phase;
use awaken_runtime::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use awaken_runtime::{PhaseContext, PhaseHook, StateCommand};

use crate::SKILLS_ACTIVE_INSTRUCTIONS_PLUGIN_ID;
use crate::registry::SkillRegistry;
use crate::skill_md::parse_skill_md;
use crate::state::SkillState;

/// Injects activated skill instructions as hidden suffix prompt segments.
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

    pub(crate) async fn render_active_instructions(&self, active_ids: Vec<String>) -> String {
        let mut rendered = Vec::new();
        let mut ids = active_ids;
        ids.sort();
        ids.dedup();

        for skill_id in ids {
            let Some(skill) = self.registry.get(&skill_id) else {
                continue;
            };

            let raw = match skill.read_instructions().await {
                Ok(raw) => raw,
                Err(err) => {
                    tracing::warn!(skill_id = %skill_id, error = %err, "failed to read active skill instructions");
                    continue;
                }
            };
            let doc = match parse_skill_md(&raw) {
                Ok(doc) => doc,
                Err(err) => {
                    tracing::warn!(skill_id = %skill_id, error = %err, "failed to parse active SKILL.md");
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

struct ActiveSkillInstructionsHook {
    plugin: ActiveSkillInstructionsPlugin,
}

#[async_trait]
impl PhaseHook for ActiveSkillInstructionsHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let active: Vec<String> = ctx
            .state::<SkillState>()
            .map(|s| s.active.iter().cloned().collect())
            .unwrap_or_default();
        if active.is_empty() {
            return Ok(StateCommand::new());
        }

        let rendered = self.plugin.render_active_instructions(active).await;
        if rendered.is_empty() {
            return Ok(StateCommand::new());
        }

        let mut cmd = StateCommand::new();
        cmd.schedule_action::<crate::AddContextMessage>(ContextMessage::suffix_system(
            "active_skill_instructions",
            rendered,
        ))?;
        Ok(cmd)
    }
}

impl Plugin for ActiveSkillInstructionsPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: SKILLS_ACTIVE_INSTRUCTIONS_PLUGIN_ID,
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        // SkillState registration is handled by SkillDiscoveryPlugin.
        // We only register the phase hook here.
        registrar.register_phase_hook(
            SKILLS_ACTIVE_INSTRUCTIONS_PLUGIN_ID,
            Phase::BeforeInference,
            ActiveSkillInstructionsHook {
                plugin: self.clone(),
            },
        )?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SkillError;
    use crate::registry::{InMemorySkillRegistry, SkillRegistry};
    use crate::skill::{ScriptResult, Skill, SkillMeta, SkillResource, SkillResourceKind};
    use awaken_contract::state::{Snapshot, StateKey, StateMap};

    #[derive(Debug)]
    struct MockSkill {
        meta: SkillMeta,
        body: &'static str,
    }

    #[async_trait]
    impl Skill for MockSkill {
        fn meta(&self) -> &SkillMeta {
            &self.meta
        }
        async fn read_instructions(&self) -> Result<String, SkillError> {
            Ok(format!(
                "---\nname: {}\ndescription: ok\n---\n{}\n",
                self.meta.id, self.body
            ))
        }
        async fn load_resource(
            &self,
            _: SkillResourceKind,
            _: &str,
        ) -> Result<SkillResource, SkillError> {
            Err(SkillError::Unsupported("mock".into()))
        }
        async fn run_script(&self, _: &str, _: &[String]) -> Result<ScriptResult, SkillError> {
            Err(SkillError::Unsupported("mock".into()))
        }
    }

    #[derive(Debug)]
    struct FailingSkill(SkillMeta);

    #[async_trait]
    impl Skill for FailingSkill {
        fn meta(&self) -> &SkillMeta {
            &self.0
        }
        async fn read_instructions(&self) -> Result<String, SkillError> {
            Err(SkillError::Io("disk error".into()))
        }
        async fn load_resource(
            &self,
            _: SkillResourceKind,
            _: &str,
        ) -> Result<SkillResource, SkillError> {
            Err(SkillError::Unsupported("mock".into()))
        }
        async fn run_script(&self, _: &str, _: &[String]) -> Result<ScriptResult, SkillError> {
            Err(SkillError::Unsupported("mock".into()))
        }
    }

    fn mock_meta(id: &str) -> SkillMeta {
        SkillMeta::new(id, id, "ok", vec![])
    }

    fn make_registry(skills: Vec<Arc<dyn Skill>>) -> Arc<dyn SkillRegistry> {
        Arc::new(InMemorySkillRegistry::from_skills(skills))
    }

    fn make_ctx_with_active(active: Vec<String>) -> PhaseContext {
        let mut state_map = StateMap::default();
        let mut val = crate::state::SkillStateValue::default();
        for id in active {
            crate::state::SkillState::apply(&mut val, crate::state::SkillStateUpdate::Activate(id));
        }
        state_map.insert::<crate::state::SkillState>(val);
        let snapshot = Snapshot::new(0, Arc::new(state_map));
        PhaseContext::new(Phase::BeforeInference, snapshot)
    }

    fn make_ctx_no_state() -> PhaseContext {
        let snapshot = Snapshot::new(0, Arc::new(StateMap::default()));
        PhaseContext::new(Phase::BeforeInference, snapshot)
    }

    #[tokio::test]
    async fn hook_run_schedules_action_when_active_skills_present() {
        let skills: Vec<Arc<dyn Skill>> = vec![Arc::new(MockSkill {
            meta: mock_meta("s1"),
            body: "Use s1.",
        })];
        let plugin = ActiveSkillInstructionsPlugin::new(make_registry(skills));
        let hook = ActiveSkillInstructionsHook { plugin };

        let ctx = make_ctx_with_active(vec!["s1".into()]);
        let cmd = PhaseHook::run(&hook, &ctx).await.unwrap();
        assert!(
            !cmd.scheduled_actions().is_empty(),
            "should schedule AddContextMessage when active skill instructions exist"
        );
    }

    #[tokio::test]
    async fn hook_run_returns_empty_when_no_skill_state() {
        let skills: Vec<Arc<dyn Skill>> = vec![Arc::new(MockSkill {
            meta: mock_meta("s1"),
            body: "Use s1.",
        })];
        let plugin = ActiveSkillInstructionsPlugin::new(make_registry(skills));
        let hook = ActiveSkillInstructionsHook { plugin };

        let ctx = make_ctx_no_state();
        let cmd = PhaseHook::run(&hook, &ctx).await.unwrap();
        assert!(
            cmd.is_empty(),
            "should be empty when no SkillState in snapshot"
        );
    }

    #[tokio::test]
    async fn hook_run_returns_empty_when_active_set_empty() {
        let skills: Vec<Arc<dyn Skill>> = vec![Arc::new(MockSkill {
            meta: mock_meta("s1"),
            body: "Use s1.",
        })];
        let plugin = ActiveSkillInstructionsPlugin::new(make_registry(skills));
        let hook = ActiveSkillInstructionsHook { plugin };

        let ctx = make_ctx_with_active(vec![]);
        let cmd = PhaseHook::run(&hook, &ctx).await.unwrap();
        assert!(cmd.is_empty(), "should be empty when active set is empty");
    }

    #[tokio::test]
    async fn hook_run_returns_empty_when_skill_read_fails() {
        let skills: Vec<Arc<dyn Skill>> = vec![Arc::new(FailingSkill(mock_meta("s1")))];
        let plugin = ActiveSkillInstructionsPlugin::new(make_registry(skills));
        let hook = ActiveSkillInstructionsHook { plugin };

        let ctx = make_ctx_with_active(vec!["s1".into()]);
        let cmd = PhaseHook::run(&hook, &ctx).await.unwrap();
        assert!(
            cmd.is_empty(),
            "should be empty when read_instructions fails"
        );
    }

    #[tokio::test]
    async fn hook_run_returns_empty_when_body_is_whitespace() {
        let skills: Vec<Arc<dyn Skill>> = vec![Arc::new(MockSkill {
            meta: mock_meta("s1"),
            body: "   ",
        })];
        let plugin = ActiveSkillInstructionsPlugin::new(make_registry(skills));
        let hook = ActiveSkillInstructionsHook { plugin };

        let ctx = make_ctx_with_active(vec!["s1".into()]);
        let cmd = PhaseHook::run(&hook, &ctx).await.unwrap();
        assert!(
            cmd.is_empty(),
            "should be empty when skill body is whitespace-only"
        );
    }

    #[tokio::test]
    async fn render_active_instructions_skips_failed_read() {
        let skills: Vec<Arc<dyn Skill>> = vec![
            Arc::new(FailingSkill(mock_meta("bad"))),
            Arc::new(MockSkill {
                meta: mock_meta("good"),
                body: "Use good.",
            }),
        ];
        let plugin = ActiveSkillInstructionsPlugin::new(make_registry(skills));
        let rendered = plugin
            .render_active_instructions(vec!["bad".into(), "good".into()])
            .await;
        assert!(rendered.contains("good"));
        assert!(!rendered.contains("bad"));
    }
}
