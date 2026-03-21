use super::PROMPT_SEGMENTS_PLUGIN_ID;
use async_trait::async_trait;
use tirea_contract::runtime::behavior::{AgentBehavior, ReadOnlyContext};
use tirea_contract::runtime::inference::{
    consume_after_emit_prompt_segments_action, PromptSegmentConsumePolicy, PromptSegmentState,
};
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
        let mut has_after_emit = false;
        for item in state.items {
            if item.consume == PromptSegmentConsumePolicy::AfterEmit {
                has_after_emit = true;
            }
            actions = actions.and(BeforeInferenceAction::AddContextMessage(
                item.to_context_message(),
            ));
        }

        if has_after_emit {
            actions = actions.and(BeforeInferenceAction::State(
                consume_after_emit_prompt_segments_action(),
            ));
        }

        actions
    }
}
