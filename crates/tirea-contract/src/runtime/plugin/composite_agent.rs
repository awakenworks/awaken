use super::agent::{AgentBehavior, ReadOnlyContext};
use crate::runtime::plugin::phase::effect::PhaseOutput;
use async_trait::async_trait;
use std::any::TypeId;
use std::collections::HashSet;
use std::sync::Arc;

/// An [`AgentBehavior`] that composes multiple sub-behaviors.
///
/// Each phase hook iterates all sub-behaviors in order, collecting their
/// [`PhaseOutput`]s into a single merged result. All sub-behaviors receive
/// the same [`ReadOnlyContext`] snapshot — they do not see each other's
/// effects within the same phase. The loop applies all collected effects
/// sequentially after the composite hook returns.
pub struct CompositeBehavior {
    id: String,
    behaviors: Vec<Arc<dyn AgentBehavior>>,
}

impl CompositeBehavior {
    pub fn new(id: impl Into<String>, behaviors: Vec<Arc<dyn AgentBehavior>>) -> Self {
        Self {
            id: id.into(),
            behaviors,
        }
    }
}

/// Merge `source` effects and state actions into `target`.
fn merge_output(target: &mut PhaseOutput, source: PhaseOutput) {
    target.effects.extend(source.effects);
    target.state_actions.extend(source.state_actions);
}

#[async_trait]
impl AgentBehavior for CompositeBehavior {
    fn id(&self) -> &str {
        &self.id
    }

    fn owned_states(&self) -> HashSet<TypeId> {
        self.behaviors
            .iter()
            .flat_map(|b| b.owned_states())
            .collect()
    }

    async fn run_start(&self, ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        let mut merged = PhaseOutput::default();
        for b in &self.behaviors {
            merge_output(&mut merged, b.run_start(ctx).await);
        }
        merged
    }

    async fn step_start(&self, ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        let mut merged = PhaseOutput::default();
        for b in &self.behaviors {
            merge_output(&mut merged, b.step_start(ctx).await);
        }
        merged
    }

    async fn before_inference(&self, ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        let mut merged = PhaseOutput::default();
        for b in &self.behaviors {
            merge_output(&mut merged, b.before_inference(ctx).await);
        }
        merged
    }

    async fn after_inference(&self, ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        let mut merged = PhaseOutput::default();
        for b in &self.behaviors {
            merge_output(&mut merged, b.after_inference(ctx).await);
        }
        merged
    }

    async fn before_tool_execute(&self, ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        let mut merged = PhaseOutput::default();
        for b in &self.behaviors {
            merge_output(&mut merged, b.before_tool_execute(ctx).await);
        }
        merged
    }

    async fn after_tool_execute(&self, ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        let mut merged = PhaseOutput::default();
        for b in &self.behaviors {
            merge_output(&mut merged, b.after_tool_execute(ctx).await);
        }
        merged
    }

    async fn step_end(&self, ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        let mut merged = PhaseOutput::default();
        for b in &self.behaviors {
            merge_output(&mut merged, b.step_end(ctx).await);
        }
        merged
    }

    async fn run_end(&self, ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
        let mut merged = PhaseOutput::default();
        for b in &self.behaviors {
            merge_output(&mut merged, b.run_end(ctx).await);
        }
        merged
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::plugin::phase::effect::PhaseEffect;
    use crate::runtime::plugin::phase::Phase;
    use crate::RunConfig;
    use serde_json::json;
    use tirea_state::DocCell;

    struct ContextBehavior {
        id: String,
        text: String,
    }

    #[async_trait]
    impl AgentBehavior for ContextBehavior {
        fn id(&self) -> &str {
            &self.id
        }

        async fn before_inference(&self, _ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
            PhaseOutput::new().system_context(&self.text)
        }
    }

    struct BlockBehavior;

    #[async_trait]
    impl AgentBehavior for BlockBehavior {
        fn id(&self) -> &str {
            "blocker"
        }

        async fn before_tool_execute(&self, ctx: &ReadOnlyContext<'_>) -> PhaseOutput {
            if ctx.tool_name() == Some("dangerous") {
                PhaseOutput::new().block_tool("denied")
            } else {
                PhaseOutput::default()
            }
        }
    }

    fn make_ctx(doc: &DocCell, phase: Phase) -> ReadOnlyContext<'_> {
        let config = Box::leak(Box::new(RunConfig::new()));
        ReadOnlyContext::new(phase, "thread_1", &[], config, doc)
    }

    #[tokio::test]
    async fn composite_merges_effects() {
        let behaviors: Vec<Arc<dyn AgentBehavior>> = vec![
            Arc::new(ContextBehavior {
                id: "a".into(),
                text: "ctx_a".into(),
            }),
            Arc::new(ContextBehavior {
                id: "b".into(),
                text: "ctx_b".into(),
            }),
        ];
        let composite = CompositeBehavior::new("test", behaviors);

        let doc = DocCell::new(json!({}));
        let ctx = make_ctx(&doc, Phase::BeforeInference);
        let output = composite.before_inference(&ctx).await;

        assert_eq!(output.effects.len(), 2);
        assert!(matches!(&output.effects[0], PhaseEffect::SystemContext(s) if s == "ctx_a"));
        assert!(matches!(&output.effects[1], PhaseEffect::SystemContext(s) if s == "ctx_b"));
    }

    #[tokio::test]
    async fn composite_owned_states_union() {
        struct TypedBehavior {
            types: HashSet<TypeId>,
        }

        #[async_trait]
        impl AgentBehavior for TypedBehavior {
            fn id(&self) -> &str {
                "typed"
            }
            fn owned_states(&self) -> HashSet<TypeId> {
                self.types.clone()
            }
        }

        let type_a = TypeId::of::<u32>();
        let type_b = TypeId::of::<String>();

        let behaviors: Vec<Arc<dyn AgentBehavior>> = vec![
            Arc::new(TypedBehavior {
                types: [type_a].into_iter().collect(),
            }),
            Arc::new(TypedBehavior {
                types: [type_b].into_iter().collect(),
            }),
        ];
        let composite = CompositeBehavior::new("test", behaviors);

        let states = composite.owned_states();
        assert!(states.contains(&type_a));
        assert!(states.contains(&type_b));
        assert_eq!(states.len(), 2);
    }

    #[tokio::test]
    async fn composite_empty_behaviors_returns_empty() {
        let composite = CompositeBehavior::new("empty", vec![]);
        let doc = DocCell::new(json!({}));
        let ctx = make_ctx(&doc, Phase::BeforeInference);

        let output = composite.before_inference(&ctx).await;
        assert!(output.is_empty());
    }

    #[tokio::test]
    async fn composite_preserves_effect_order() {
        let behaviors: Vec<Arc<dyn AgentBehavior>> = vec![
            Arc::new(ContextBehavior {
                id: "first".into(),
                text: "1".into(),
            }),
            Arc::new(BlockBehavior),
            Arc::new(ContextBehavior {
                id: "last".into(),
                text: "2".into(),
            }),
        ];
        let composite = CompositeBehavior::new("order_test", behaviors);

        let doc = DocCell::new(json!({}));
        let ctx = make_ctx(&doc, Phase::BeforeInference);
        let output = composite.before_inference(&ctx).await;

        // BlockBehavior returns empty for BeforeInference, so 2 effects
        assert_eq!(output.effects.len(), 2);
        assert!(matches!(&output.effects[0], PhaseEffect::SystemContext(s) if s == "1"));
        assert!(matches!(&output.effects[1], PhaseEffect::SystemContext(s) if s == "2"));
    }
}
