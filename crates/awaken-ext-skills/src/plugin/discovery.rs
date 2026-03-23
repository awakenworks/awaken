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

    pub(crate) fn render_catalog(&self, _active: &HashSet<String>) -> String {
        let mut metas: Vec<SkillMeta> = self
            .registry
            .snapshot()
            .values()
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

        let rendered = self.plugin.render_catalog(&active);
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

        registrar.register_phase_hook(
            SKILLS_DISCOVERY_PLUGIN_ID,
            Phase::BeforeInference,
            SkillDiscoveryHook {
                plugin: self.clone(),
            },
        )?;

        Ok(())
    }
}
