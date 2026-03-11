use crate::contracts::runtime::behavior::{AgentBehavior, ReadOnlyContext};
use crate::contracts::runtime::phase::{
    ActionSet, AfterInferenceAction, AfterToolExecuteAction, BeforeInferenceAction,
    BeforeToolExecuteAction, LifecycleAction,
};
use async_trait::async_trait;
use std::sync::Arc;

/// Compose multiple behaviors into a single [`AgentBehavior`].
///
/// If the list contains a single behavior, returns it directly.
/// If it contains multiple, wraps them in a composite that concatenates
/// their action lists in order.
///
/// This is the public API for behavior composition — callers never need
/// to know about the concrete composite type.
pub fn compose_behaviors(
    id: impl Into<String>,
    behaviors: Vec<Arc<dyn AgentBehavior>>,
) -> Arc<dyn AgentBehavior> {
    match behaviors.len() {
        0 => Arc::new(crate::contracts::runtime::behavior::NoOpBehavior),
        1 => behaviors.into_iter().next().unwrap(),
        _ => Arc::new(CompositeBehavior::new(id, behaviors)),
    }
}

/// An [`AgentBehavior`] that composes multiple sub-behaviors.
///
/// Each phase hook executes all sub-behaviors concurrently, merging their
/// action sets in registration order. All sub-behaviors receive the same
/// [`ReadOnlyContext`] snapshot — they do not see each other's effects
/// within the same phase. The loop validates and applies all collected
/// actions after the composite hook returns.
pub(crate) struct CompositeBehavior {
    id: String,
    behaviors: Vec<Arc<dyn AgentBehavior>>,
}

impl CompositeBehavior {
    pub(crate) fn new(id: impl Into<String>, behaviors: Vec<Arc<dyn AgentBehavior>>) -> Self {
        Self {
            id: id.into(),
            behaviors,
        }
    }
}

#[async_trait]
impl AgentBehavior for CompositeBehavior {
    fn id(&self) -> &str {
        &self.id
    }

    fn behavior_ids(&self) -> Vec<&str> {
        self.behaviors
            .iter()
            .flat_map(|b| b.behavior_ids())
            .collect()
    }

    async fn run_start(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
        let futs: Vec<_> = self.behaviors.iter().map(|b| b.run_start(ctx)).collect();
        futures::future::join_all(futs)
            .await
            .into_iter()
            .fold(ActionSet::empty(), |acc, a| acc.and(a))
    }

    async fn step_start(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
        let futs: Vec<_> = self.behaviors.iter().map(|b| b.step_start(ctx)).collect();
        futures::future::join_all(futs)
            .await
            .into_iter()
            .fold(ActionSet::empty(), |acc, a| acc.and(a))
    }

    async fn before_inference(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        let futs: Vec<_> = self
            .behaviors
            .iter()
            .map(|b| b.before_inference(ctx))
            .collect();
        futures::future::join_all(futs)
            .await
            .into_iter()
            .fold(ActionSet::empty(), |acc, a| acc.and(a))
    }

    async fn after_inference(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<AfterInferenceAction> {
        let futs: Vec<_> = self
            .behaviors
            .iter()
            .map(|b| b.after_inference(ctx))
            .collect();
        futures::future::join_all(futs)
            .await
            .into_iter()
            .fold(ActionSet::empty(), |acc, a| acc.and(a))
    }

    async fn before_tool_execute(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeToolExecuteAction> {
        let futs: Vec<_> = self
            .behaviors
            .iter()
            .map(|b| b.before_tool_execute(ctx))
            .collect();
        futures::future::join_all(futs)
            .await
            .into_iter()
            .fold(ActionSet::empty(), |acc, a| acc.and(a))
    }

    async fn after_tool_execute(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<AfterToolExecuteAction> {
        let futs: Vec<_> = self
            .behaviors
            .iter()
            .map(|b| b.after_tool_execute(ctx))
            .collect();
        futures::future::join_all(futs)
            .await
            .into_iter()
            .fold(ActionSet::empty(), |acc, a| acc.and(a))
    }

    async fn step_end(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
        let futs: Vec<_> = self.behaviors.iter().map(|b| b.step_end(ctx)).collect();
        futures::future::join_all(futs)
            .await
            .into_iter()
            .fold(ActionSet::empty(), |acc, a| acc.and(a))
    }

    async fn run_end(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
        let futs: Vec<_> = self.behaviors.iter().map(|b| b.run_end(ctx)).collect();
        futures::future::join_all(futs)
            .await
            .into_iter()
            .fold(ActionSet::empty(), |acc, a| acc.and(a))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::runtime::phase::BeforeInferenceAction;
    use crate::contracts::runtime::phase::Phase;
    use crate::contracts::RunPolicy;
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

        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::AddSystemContext(self.text.clone()))
        }
    }

    struct BlockBehavior;

    #[async_trait]
    impl AgentBehavior for BlockBehavior {
        fn id(&self) -> &str {
            "blocker"
        }

        async fn before_tool_execute(
            &self,
            ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeToolExecuteAction> {
            if ctx.tool_name() == Some("dangerous") {
                ActionSet::single(BeforeToolExecuteAction::Block("denied".into()))
            } else {
                ActionSet::empty()
            }
        }
    }

    fn make_ctx<'a>(
        doc: &'a DocCell,
        run_policy: &'a RunPolicy,
        phase: Phase,
    ) -> ReadOnlyContext<'a> {
        ReadOnlyContext::new(phase, "thread_1", &[], run_policy, doc)
    }

    #[tokio::test]
    async fn composite_merges_actions() {
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
        let run_policy = RunPolicy::new();
        let ctx = make_ctx(&doc, &run_policy, Phase::BeforeInference);
        let actions = composite.before_inference(&ctx).await;

        assert_eq!(actions.len(), 2);
        let v = actions.into_vec();
        assert!(matches!(v[0], BeforeInferenceAction::AddSystemContext(_)));
        assert!(matches!(v[1], BeforeInferenceAction::AddSystemContext(_)));
    }

    #[tokio::test]
    async fn composite_empty_behaviors_returns_empty() {
        let composite = CompositeBehavior::new("empty", vec![]);
        let doc = DocCell::new(json!({}));
        let run_policy = RunPolicy::new();
        let ctx = make_ctx(&doc, &run_policy, Phase::BeforeInference);

        let actions = composite.before_inference(&ctx).await;
        assert!(actions.is_empty());
    }

    #[tokio::test]
    async fn composite_preserves_action_order() {
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
        let run_policy = RunPolicy::new();
        let ctx = make_ctx(&doc, &run_policy, Phase::BeforeInference);
        let actions = composite.before_inference(&ctx).await;

        // BlockBehavior returns empty for BeforeInference, so 2 actions
        assert_eq!(actions.len(), 2);
        let v = actions.into_vec();
        assert!(matches!(v[0], BeforeInferenceAction::AddSystemContext(_)));
        assert!(matches!(v[1], BeforeInferenceAction::AddSystemContext(_)));
    }

    #[test]
    fn compose_behaviors_empty_returns_noop() {
        let behavior = compose_behaviors("test", Vec::new());

        assert_eq!(behavior.id(), "noop");
        assert_eq!(behavior.behavior_ids(), vec!["noop"]);
    }

    #[test]
    fn compose_behaviors_single_passthrough() {
        let input = Arc::new(ContextBehavior {
            id: "single".into(),
            text: "ctx".into(),
        }) as Arc<dyn AgentBehavior>;
        let behavior = compose_behaviors("ignored", vec![input.clone()]);

        assert!(Arc::ptr_eq(&behavior, &input));
        assert_eq!(behavior.id(), "single");
        assert_eq!(behavior.behavior_ids(), vec!["single"]);
    }

    #[test]
    fn compose_behaviors_multiple_keeps_leaf_behavior_ids_order() {
        let behavior = compose_behaviors(
            "composed",
            vec![
                Arc::new(ContextBehavior {
                    id: "a".into(),
                    text: "ctx_a".into(),
                }),
                Arc::new(ContextBehavior {
                    id: "b".into(),
                    text: "ctx_b".into(),
                }),
            ],
        );

        assert_eq!(behavior.id(), "composed");
        assert_eq!(behavior.behavior_ids(), vec!["a", "b"]);
    }
}
