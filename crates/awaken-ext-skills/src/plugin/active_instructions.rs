use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

use async_trait::async_trait;

use awaken_runtime::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use awaken_runtime::{PhaseContext, PhaseHook, StateCommand};
use awaken_runtime_contract::StateError;
use awaken_runtime_contract::contract::context_message::ContextMessage;
use awaken_runtime_contract::model::Phase;

use crate::SKILLS_ACTIVE_INSTRUCTIONS_PLUGIN_ID;
use crate::registry::SkillRegistry;
use crate::skill_md::parse_skill_md;
use crate::state::{SkillRenderedActivation, SkillState, SkillStateUpdate, SkillStateValue};

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

    #[cfg(test)]
    pub(crate) async fn render_active_instructions(&self, active_ids: Vec<String>) -> String {
        self.render_active_state(SkillStateValue {
            active: active_ids.into_iter().collect(),
            activations: Default::default(),
        })
        .await
    }

    #[cfg(test)]
    pub(crate) async fn render_active_state(&self, active_state: SkillStateValue) -> String {
        self.render_active_state_with_cleanup(active_state)
            .await
            .instructions
    }

    async fn render_active_state_with_cleanup(
        &self,
        active_state: SkillStateValue,
    ) -> RenderedActiveState {
        let mut rendered = Vec::new();
        let mut rendered_ids = HashSet::new();
        let mut stale_skill_ids = BTreeSet::new();

        for (_key, activation) in active_state.activations {
            rendered_ids.insert(activation.skill_id.clone());
            let Some(skill) = self.registry.get(&activation.skill_id) else {
                stale_skill_ids.insert(activation.skill_id);
                continue;
            };
            if let Some(rendered_fingerprint) = activation.fingerprint.as_deref() {
                match skill.activation_fingerprint() {
                    Some(current_fingerprint) if current_fingerprint == rendered_fingerprint => {}
                    Some(current_fingerprint) => {
                        tracing::warn!(
                            skill_id = %activation.skill_id,
                            rendered_fingerprint,
                            current_fingerprint,
                            "dropping stale rendered skill activation"
                        );
                        stale_skill_ids.insert(activation.skill_id);
                        continue;
                    }
                    None => {
                        tracing::warn!(
                            skill_id = %activation.skill_id,
                            rendered_fingerprint,
                            "dropping rendered skill activation because live skill has no comparable fingerprint"
                        );
                        stale_skill_ids.insert(activation.skill_id);
                        continue;
                    }
                }
            }
            let body = activation.instructions.trim();
            if body.is_empty() {
                continue;
            }
            rendered.push(render_skill_instruction(&activation));
        }

        let mut ids: Vec<String> = active_state
            .active
            .into_iter()
            .filter(|id| !rendered_ids.contains(id))
            .collect();
        ids.sort();
        ids.dedup();

        for skill_id in ids {
            let Some(skill) = self.registry.get(&skill_id) else {
                stale_skill_ids.insert(skill_id);
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

        let instructions = if rendered.is_empty() {
            String::new()
        } else {
            format!(
                "<active_skill_instructions>\n{}\n</active_skill_instructions>",
                rendered.join("\n")
            )
        };

        RenderedActiveState {
            instructions,
            stale_skill_ids: stale_skill_ids.into_iter().collect(),
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct RenderedActiveState {
    instructions: String,
    stale_skill_ids: Vec<String>,
}

struct ActiveSkillInstructionsHook {
    plugin: ActiveSkillInstructionsPlugin,
}

#[async_trait]
impl PhaseHook for ActiveSkillInstructionsHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let active = ctx.state::<SkillState>().cloned().unwrap_or_default();
        if active.active.is_empty() && active.activations.is_empty() {
            return Ok(StateCommand::new());
        }

        let rendered = self.plugin.render_active_state_with_cleanup(active).await;

        let mut cmd = StateCommand::new();
        for skill_id in rendered.stale_skill_ids {
            cmd.update::<SkillState>(SkillStateUpdate::Deactivate(skill_id));
        }

        if !rendered.instructions.is_empty() {
            cmd.schedule_action::<crate::AddContextMessage>(ContextMessage::suffix_system(
                "active_skill_instructions",
                rendered.instructions,
            ))?;
        }

        Ok(cmd)
    }
}

fn render_skill_instruction(activation: &SkillRenderedActivation) -> String {
    format!(
        "<skill_instruction skill=\"{}\">\n{}\n</skill_instruction>",
        activation.skill_id,
        activation.instructions.trim()
    )
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
    use awaken_runtime_contract::state::{Snapshot, StateKey, StateMap};

    #[derive(Debug)]
    struct MockSkill {
        meta: SkillMeta,
        body: &'static str,
        fingerprint: Option<&'static str>,
    }

    #[async_trait]
    impl Skill for MockSkill {
        fn meta(&self) -> &SkillMeta {
            &self.meta
        }
        fn activation_fingerprint(&self) -> Option<&str> {
            self.fingerprint
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

    fn mock_skill(id: &str, body: &'static str) -> Arc<dyn Skill> {
        Arc::new(MockSkill {
            meta: mock_meta(id),
            body,
            fingerprint: None,
        })
    }

    fn mock_skill_with_fingerprint(
        id: &str,
        body: &'static str,
        fingerprint: &'static str,
    ) -> Arc<dyn Skill> {
        Arc::new(MockSkill {
            meta: mock_meta(id),
            body,
            fingerprint: Some(fingerprint),
        })
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

    fn make_ctx_with_state(state: crate::state::SkillStateValue) -> PhaseContext {
        let mut state_map = StateMap::default();
        state_map.insert::<crate::state::SkillState>(state);
        let snapshot = Snapshot::new(0, Arc::new(state_map));
        PhaseContext::new(Phase::BeforeInference, snapshot)
    }

    fn make_ctx_no_state() -> PhaseContext {
        let snapshot = Snapshot::new(0, Arc::new(StateMap::default()));
        PhaseContext::new(Phase::BeforeInference, snapshot)
    }

    #[tokio::test]
    async fn hook_run_schedules_action_when_active_skills_present() {
        let skills: Vec<Arc<dyn Skill>> = vec![mock_skill("s1", "Use s1.")];
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
        let skills: Vec<Arc<dyn Skill>> = vec![mock_skill("s1", "Use s1.")];
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
        let skills: Vec<Arc<dyn Skill>> = vec![mock_skill("s1", "Use s1.")];
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
        let skills: Vec<Arc<dyn Skill>> = vec![mock_skill("s1", "   ")];
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
            mock_skill("good", "Use good."),
        ];
        let plugin = ActiveSkillInstructionsPlugin::new(make_registry(skills));
        let rendered = plugin
            .render_active_instructions(vec!["bad".into(), "good".into()])
            .await;
        assert!(rendered.contains("good"));
        assert!(!rendered.contains("bad"));
    }

    #[tokio::test]
    async fn render_active_state_uses_rendered_activation_instructions() {
        let skills: Vec<Arc<dyn Skill>> = vec![mock_skill("db", "Use ${dialect} syntax.")];
        let plugin = ActiveSkillInstructionsPlugin::new(make_registry(skills));
        let mut state = crate::state::SkillStateValue::default();
        crate::state::SkillState::apply(
            &mut state,
            crate::state::SkillStateUpdate::ActivateRendered {
                skill_id: "db".to_string(),
                instructions: "Use postgres syntax.".to_string(),
                fingerprint: None,
            },
        );

        let rendered = plugin.render_active_state(state).await;
        assert!(rendered.contains("Use postgres syntax."));
        assert!(!rendered.contains("${dialect}"));
    }

    #[tokio::test]
    async fn render_active_state_uses_latest_rendered_activation_for_skill() {
        let skills: Vec<Arc<dyn Skill>> = vec![mock_skill("db", "Use ${dialect} syntax.")];
        let plugin = ActiveSkillInstructionsPlugin::new(make_registry(skills));
        let mut state = crate::state::SkillStateValue::default();
        crate::state::SkillState::apply(
            &mut state,
            crate::state::SkillStateUpdate::ActivateRendered {
                skill_id: "db".to_string(),
                instructions: "Use postgres syntax.".to_string(),
                fingerprint: None,
            },
        );
        crate::state::SkillState::apply(
            &mut state,
            crate::state::SkillStateUpdate::ActivateRendered {
                skill_id: "db".to_string(),
                instructions: "Use mysql syntax.".to_string(),
                fingerprint: None,
            },
        );

        let rendered = plugin.render_active_state(state).await;
        assert!(rendered.contains("Use mysql syntax."));
        assert!(!rendered.contains("Use postgres syntax."));
        assert!(!rendered.contains("${dialect}"));
    }

    #[tokio::test]
    async fn render_active_state_drops_stale_rendered_activation_after_skill_update() {
        let skills: Vec<Arc<dyn Skill>> = vec![mock_skill_with_fingerprint(
            "db",
            "Use current body.",
            "new-fp",
        )];
        let plugin = ActiveSkillInstructionsPlugin::new(make_registry(skills));
        let mut state = crate::state::SkillStateValue::default();
        crate::state::SkillState::apply(
            &mut state,
            crate::state::SkillStateUpdate::ActivateRendered {
                skill_id: "db".to_string(),
                instructions: "Use old rendered body.".to_string(),
                fingerprint: Some("old-fp".to_string()),
            },
        );

        let rendered = plugin.render_active_state(state).await;
        assert!(rendered.is_empty());
        assert!(!rendered.contains("Use old rendered body."));
        assert!(!rendered.contains("Use current body."));
    }

    #[tokio::test]
    async fn render_active_state_reports_stale_rendered_activation_for_cleanup() {
        let skills: Vec<Arc<dyn Skill>> = vec![mock_skill_with_fingerprint(
            "db",
            "Use current body.",
            "new-fp",
        )];
        let plugin = ActiveSkillInstructionsPlugin::new(make_registry(skills));
        let mut state = crate::state::SkillStateValue::default();
        crate::state::SkillState::apply(
            &mut state,
            crate::state::SkillStateUpdate::ActivateRendered {
                skill_id: "db".to_string(),
                instructions: "Use old rendered body.".to_string(),
                fingerprint: Some("old-fp".to_string()),
            },
        );

        let rendered = plugin.render_active_state_with_cleanup(state).await;
        assert!(rendered.instructions.is_empty());
        assert_eq!(rendered.stale_skill_ids, vec!["db"]);
    }

    #[tokio::test]
    async fn hook_run_deactivates_stale_rendered_activation() {
        let skills: Vec<Arc<dyn Skill>> = vec![mock_skill_with_fingerprint(
            "db",
            "Use current body.",
            "new-fp",
        )];
        let plugin = ActiveSkillInstructionsPlugin::new(make_registry(skills));
        let hook = ActiveSkillInstructionsHook { plugin };

        let mut state = crate::state::SkillStateValue::default();
        crate::state::SkillState::apply(
            &mut state,
            crate::state::SkillStateUpdate::ActivateRendered {
                skill_id: "db".to_string(),
                instructions: "Use old rendered body.".to_string(),
                fingerprint: Some("old-fp".to_string()),
            },
        );

        let cmd = PhaseHook::run(&hook, &make_ctx_with_state(state))
            .await
            .unwrap();
        assert!(
            cmd.scheduled_actions().is_empty(),
            "stale rendered activation should not inject instructions"
        );
        assert_eq!(cmd.op_len(), 1);
        assert_eq!(cmd.touched_keys, vec![crate::state::SkillState::KEY]);
    }
}
