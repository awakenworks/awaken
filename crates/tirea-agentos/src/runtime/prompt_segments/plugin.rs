use super::PromptSegmentState;
use super::PROMPT_SEGMENTS_PLUGIN_ID;
use async_trait::async_trait;
use tirea_contract::runtime::behavior::{AgentBehavior, ReadOnlyContext};
use tirea_contract::runtime::phase::{ActionSet, BeforeInferenceAction};

/// Bridges durable prompt-segment state into per-inference hidden context.
#[derive(Debug, Clone, Default)]
pub struct PromptSegmentsPlugin;

#[async_trait]
impl AgentBehavior for PromptSegmentsPlugin {
    fn id(&self) -> &str {
        PROMPT_SEGMENTS_PLUGIN_ID
    }

    tirea_contract::declare_plugin_states!(PromptSegmentState);

    async fn before_inference(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        let Some(state) = ctx.snapshot_of::<PromptSegmentState>().ok() else {
            return ActionSet::empty();
        };
        if state.items.is_empty() {
            return ActionSet::empty();
        }

        let mut actions = ActionSet::empty();
        for item in state.items {
            actions = actions.and(BeforeInferenceAction::AddContextMessage(item));
        }

        actions
    }
}
