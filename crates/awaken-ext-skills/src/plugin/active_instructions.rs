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
