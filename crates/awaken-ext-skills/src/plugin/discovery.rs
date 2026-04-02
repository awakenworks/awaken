use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;

use awaken_contract::StateError;
use awaken_contract::contract::context_message::ContextMessage;
use awaken_contract::model::Phase;
use awaken_runtime::plugins::{Plugin, PluginDescriptor, PluginRegistrar};
use awaken_runtime::state::StateKeyOptions;
use awaken_runtime::{PhaseContext, PhaseHook, StateCommand};

use crate::SKILLS_DISCOVERY_PLUGIN_ID;
use crate::registry::SkillRegistry;
use crate::skill::SkillMeta;
use crate::state::SkillState;
use crate::visibility::{SkillVisibility, SkillVisibilityStateKey, SkillVisibilityStateValue};

/// Injects a skills catalog into the LLM context so the model can discover and activate skills.
#[derive(Clone)]
pub struct SkillDiscoveryPlugin {
    registry: Arc<dyn SkillRegistry>,
    max_entries: usize,
    max_chars: usize,
}

impl std::fmt::Debug for SkillDiscoveryPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillDiscoveryPlugin")
            .field("max_entries", &self.max_entries)
            .field("max_chars", &self.max_chars)
            .finish_non_exhaustive()
    }
}

impl SkillDiscoveryPlugin {
    pub fn new(registry: Arc<dyn SkillRegistry>) -> Self {
        Self {
            registry,
            max_entries: 32,
            max_chars: 16 * 1024,
        }
    }

    pub fn with_limits(mut self, max_entries: usize, max_chars: usize) -> Self {
        self.max_entries = max_entries.max(1);
        self.max_chars = max_chars.max(256);
        self
    }

    fn escape_text(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    pub(crate) fn render_catalog(
        &self,
        _active: &HashSet<String>,
        visibility: Option<&SkillVisibilityStateValue>,
    ) -> String {
        let mut metas: Vec<SkillMeta> = self
            .registry
            .snapshot()
            .values()
            .filter(|s| {
                // Filter by visibility policy state (ADR-0020).
                visibility
                    .map(|v| v.visibility_of(&s.meta().id) != SkillVisibility::Hidden)
                    .unwrap_or(true)
            })
            .map(|s| s.meta().clone())
            .collect();

        if metas.is_empty() {
            return String::new();
        }

        metas.sort_by(|a, b| a.id.cmp(&b.id));

        let total = metas.len();
        let mut out = String::new();
        out.push_str("<available_skills>\n");

        let mut shown = 0usize;
        for m in metas.into_iter().take(self.max_entries) {
            let id = Self::escape_text(&m.id);
            let mut desc = m.description.clone();
            if m.name != m.id && !m.name.trim().is_empty() {
                if desc.trim().is_empty() {
                    desc = m.name.clone();
                } else {
                    desc = format!("{}: {}", m.name.trim(), desc.trim());
                }
            }
            // Append when_to_use if available (ADR-0020).
            if let Some(when) = &m.when_to_use {
                let when = when.trim();
                if !when.is_empty() {
                    desc = if desc.trim().is_empty() {
                        format!("When: {when}")
                    } else {
                        format!("{} — When: {when}", desc.trim())
                    };
                }
            }
            let desc = Self::escape_text(&desc);

            out.push_str("<skill>\n");
            out.push_str(&format!("<name>{}</name>\n", id));
            if !desc.trim().is_empty() {
                out.push_str(&format!("<description>{}</description>\n", desc));
            }
            out.push_str("</skill>\n");
            shown += 1;

            if out.len() >= self.max_chars {
                break;
            }
        }

        out.push_str("</available_skills>\n");

        if shown < total {
            out.push_str(&format!(
                "Note: available_skills truncated (total={}, shown={}).\n",
                total, shown
            ));
        }

        out.push_str("<skills_usage>\n");
        out.push_str("If a listed skill is relevant, call tool \"skill\" with {\"skill\": \"<id or name>\"} before answering.\n");
        out.push_str("Skill resources are not auto-loaded: use \"load_skill_resource\" with {\"skill\": \"<id>\", \"path\": \"references/<file>|assets/<file>\"}.\n");
        out.push_str("To run skill scripts: use \"skill_script\" with {\"skill\": \"<id>\", \"script\": \"scripts/<file>\", \"args\": [..]}.\n");
        out.push_str("</skills_usage>");

        if out.len() > self.max_chars {
            out.truncate(self.max_chars);
        }

        out.trim_end().to_string()
    }
}

struct SkillDiscoveryHook {
    plugin: SkillDiscoveryPlugin,
}

#[async_trait]
impl PhaseHook for SkillDiscoveryHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let active: HashSet<String> = ctx
            .state::<SkillState>()
            .map(|s| s.active.iter().cloned().collect())
            .unwrap_or_default();

        let visibility = ctx.state::<SkillVisibilityStateKey>();
        let rendered = self.plugin.render_catalog(&active, visibility);
        if rendered.is_empty() {
            return Ok(StateCommand::new());
        }

        let mut cmd = StateCommand::new();
        cmd.schedule_action::<crate::AddContextMessage>(ContextMessage::system(
            "skill_catalog",
            rendered,
        ))?;
        Ok(cmd)
    }
}

impl Plugin for SkillDiscoveryPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor {
            name: SKILLS_DISCOVERY_PLUGIN_ID,
        }
    }

    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), StateError> {
        registrar.register_key::<SkillState>(StateKeyOptions {
            persistent: true,
            retain_on_uninstall: false,
            scope: awaken_contract::state::KeyScope::Run,
        })?;

        registrar.register_key::<SkillVisibilityStateKey>(StateKeyOptions {
            persistent: false,
            retain_on_uninstall: false,
            scope: awaken_contract::state::KeyScope::Run,
        })?;

        registrar.register_phase_hook(
            SKILLS_DISCOVERY_PLUGIN_ID,
            Phase::BeforeInference,
            SkillDiscoveryHook {
                plugin: self.clone(),
            },
        )?;

        // Register skill tools
        let registry = self.registry.clone();
        registrar.register_tool(
            crate::SKILL_ACTIVATE_TOOL_ID,
            Arc::new(crate::tools::SkillActivateTool::new(registry.clone())),
        )?;
        registrar.register_tool(
            crate::SKILL_LOAD_RESOURCE_TOOL_ID,
            Arc::new(crate::tools::LoadSkillResourceTool::new(registry.clone())),
        )?;
        registrar.register_tool(
            crate::SKILL_SCRIPT_TOOL_ID,
            Arc::new(crate::tools::SkillScriptTool::new(registry)),
        )?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SkillError;
    use crate::registry::InMemorySkillRegistry;
    use crate::skill::{ScriptResult, Skill, SkillMeta, SkillResource, SkillResourceKind};
    use awaken_contract::state::{Snapshot, StateKey, StateMap};

    #[derive(Debug)]
    struct MockSkill(SkillMeta);

    #[async_trait]
    impl Skill for MockSkill {
        fn meta(&self) -> &SkillMeta {
            &self.0
        }
        async fn read_instructions(&self) -> Result<String, SkillError> {
            Ok(String::new())
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
        SkillMeta::new(id, id, format!("{id} desc"), vec![])
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
    async fn hook_run_schedules_catalog_when_skills_exist() {
        let skills: Vec<Arc<dyn Skill>> = vec![Arc::new(MockSkill(mock_meta("s1")))];
        let plugin = SkillDiscoveryPlugin::new(make_registry(skills));
        let hook = SkillDiscoveryHook { plugin };

        let ctx = make_ctx_no_state();
        let cmd = PhaseHook::run(&hook, &ctx).await.unwrap();
        assert!(
            !cmd.scheduled_actions().is_empty(),
            "should schedule AddContextMessage with catalog when skills exist"
        );
    }

    #[tokio::test]
    async fn hook_run_returns_empty_when_registry_empty() {
        let plugin = SkillDiscoveryPlugin::new(make_registry(vec![]));
        let hook = SkillDiscoveryHook { plugin };

        let ctx = make_ctx_no_state();
        let cmd = PhaseHook::run(&hook, &ctx).await.unwrap();
        assert!(cmd.is_empty(), "should be empty when no skills in registry");
    }

    #[tokio::test]
    async fn hook_run_with_active_state_still_renders_catalog() {
        let skills: Vec<Arc<dyn Skill>> = vec![
            Arc::new(MockSkill(mock_meta("s1"))),
            Arc::new(MockSkill(mock_meta("s2"))),
        ];
        let plugin = SkillDiscoveryPlugin::new(make_registry(skills));
        let hook = SkillDiscoveryHook { plugin };

        let ctx = make_ctx_with_active(vec!["s1".into()]);
        let cmd = PhaseHook::run(&hook, &ctx).await.unwrap();
        assert!(!cmd.scheduled_actions().is_empty());
    }

    #[test]
    fn render_catalog_no_description_tag_when_both_name_and_id_match_and_desc_empty() {
        let skill: Arc<dyn Skill> = Arc::new(MockSkill(SkillMeta::new("s1", "s1", "  ", vec![])));
        let plugin = SkillDiscoveryPlugin::new(make_registry(vec![skill]));
        let active = HashSet::new();
        let s = plugin.render_catalog(&active, None);
        assert!(s.contains("<name>s1</name>"));
        assert!(!s.contains("<description>"));
    }

    #[test]
    fn render_catalog_char_limit_truncates_output() {
        let mut skills: Vec<Arc<dyn Skill>> = Vec::new();
        for i in 0..10 {
            skills.push(Arc::new(MockSkill(mock_meta(&format!("s{i}")))));
        }
        let plugin = SkillDiscoveryPlugin::new(make_registry(skills)).with_limits(100, 256);
        let active = HashSet::new();
        let s = plugin.render_catalog(&active, None);
        assert!(s.len() <= 256);
    }

    #[test]
    fn render_catalog_entry_limit_shows_truncation_note() {
        let mut skills: Vec<Arc<dyn Skill>> = Vec::new();
        for i in 0..5 {
            skills.push(Arc::new(MockSkill(mock_meta(&format!("s{i}")))));
        }
        let plugin = SkillDiscoveryPlugin::new(make_registry(skills)).with_limits(2, 16 * 1024);
        let active = HashSet::new();
        let s = plugin.render_catalog(&active, None);
        assert!(s.contains("truncated"));
        assert_eq!(s.matches("<skill>").count(), 2);
    }
}
