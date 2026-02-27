use crate::SKILLS_RUNTIME_PLUGIN_ID;
use async_trait::async_trait;
use tirea_contract::runtime::plugin::agent::{AgentBehavior, ReadOnlyContext};
use tirea_contract::runtime::plugin::phase::effect::PhaseOutput;
use tirea_contract::runtime::plugin::phase::BeforeInferenceContext;
use tirea_contract::runtime::plugin::AgentPlugin;

/// Placeholder plugin for activated skill state.
///
/// Skill instructions are injected via `append_user_messages` (single injection path)
/// and tool results for references/scripts/assets are already visible in conversation
/// history. This plugin no longer injects system context to avoid token waste from
/// duplicate injection.
#[derive(Debug, Default, Clone)]
pub struct SkillRuntimePlugin;

impl SkillRuntimePlugin {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl AgentPlugin for SkillRuntimePlugin {
    fn id(&self) -> &str {
        SKILLS_RUNTIME_PLUGIN_ID
    }

    async fn before_inference(&self, _ctx: &mut BeforeInferenceContext<'_, '_>) {
        // No-op: skill content is delivered via append_user_messages and tool results.
    }
}

#[async_trait]
impl AgentBehavior for SkillRuntimePlugin {
    fn id(&self) -> &str {
        SKILLS_RUNTIME_PLUGIN_ID
    }

    async fn before_inference(&self, _ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        // No-op: skill content is delivered via append_user_messages and tool results.
        PhaseOutput::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tirea_contract::testing::TestFixture;
    use tirea_contract::runtime::tool_call::ToolDescriptor;

    #[tokio::test]
    async fn plugin_does_not_inject_system_context() {
        let fixture = TestFixture::new_with_state(json!({
            "skills": {
                "active": ["a"],
                "instructions": {"a": "Do X"},
                "references": {},
                "scripts": {}
            }
        }));
        let mut step = fixture.step(vec![ToolDescriptor::new("t", "t", "t")]);
        let p = SkillRuntimePlugin::new();
        let mut before = BeforeInferenceContext::new(&mut step);
        AgentPlugin::before_inference(&p, &mut before).await;
        assert!(
            step.system_context.is_empty(),
            "runtime plugin should not inject system context"
        );
    }
}
