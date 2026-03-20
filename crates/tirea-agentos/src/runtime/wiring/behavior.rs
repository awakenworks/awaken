use crate::contracts::runtime::behavior::{AgentBehavior, ReadOnlyContext};
use crate::contracts::runtime::phase::{
    ActionSet, AfterInferenceAction, AfterToolExecuteAction, BeforeInferenceAction,
    BeforeToolExecuteAction, LifecycleAction,
};
use async_trait::async_trait;
use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;

/// Error returned when plugin ordering constraints form a cycle.
#[derive(Debug, Clone)]
pub struct PluginOrderingCycleError {
    pub involved: Vec<String>,
}

impl std::fmt::Display for PluginOrderingCycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "plugin ordering cycle detected among: {}",
            self.involved.join(", ")
        )
    }
}

impl std::error::Error for PluginOrderingCycleError {}

/// Compose multiple behaviors into a single [`AgentBehavior`].
///
/// Returns an error if ordering constraints form a cycle.
pub fn compose_behaviors(
    id: impl Into<String>,
    behaviors: Vec<Arc<dyn AgentBehavior>>,
) -> Result<Arc<dyn AgentBehavior>, PluginOrderingCycleError> {
    match behaviors.len() {
        0 => Ok(Arc::new(crate::contracts::runtime::behavior::NoOpBehavior)),
        1 => Ok(behaviors.into_iter().next().unwrap()),
        _ => Ok(Arc::new(CompositeBehavior::new(id, behaviors)?)),
    }
}

/// An [`AgentBehavior`] that composes multiple sub-behaviors.
///
/// Each phase hook executes sub-behaviors sequentially in topological order.
pub(crate) struct CompositeBehavior {
    id: String,
    behaviors: Vec<Arc<dyn AgentBehavior>>,
}

impl CompositeBehavior {
    pub(crate) fn new(
        id: impl Into<String>,
        behaviors: Vec<Arc<dyn AgentBehavior>>,
    ) -> Result<Self, PluginOrderingCycleError> {
        let sorted = topological_sort(&behaviors)?;
        Ok(Self {
            id: id.into(),
            behaviors: sorted,
        })
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

    fn configure(&self, config: &mut tirea_contract::runtime::run::config::AgentRunConfig) {
        for b in &self.behaviors {
            b.configure(config);
        }
    }

    async fn run_start(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
        let mut combined = ActionSet::empty();
        for b in &self.behaviors {
            combined = combined.and(b.run_start(ctx).await);
        }
        combined
    }

    async fn step_start(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
        let mut combined = ActionSet::empty();
        for b in &self.behaviors {
            combined = combined.and(b.step_start(ctx).await);
        }
        combined
    }

    async fn before_inference(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeInferenceAction> {
        let mut combined = ActionSet::empty();
        for b in &self.behaviors {
            combined = combined.and(b.before_inference(ctx).await);
        }
        combined
    }

    async fn after_inference(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<AfterInferenceAction> {
        let mut combined = ActionSet::empty();
        for b in &self.behaviors {
            combined = combined.and(b.after_inference(ctx).await);
        }
        combined
    }

    async fn before_tool_execute(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<BeforeToolExecuteAction> {
        let mut combined = ActionSet::empty();
        for b in &self.behaviors {
            combined = combined.and(b.before_tool_execute(ctx).await);
        }
        combined
    }

    async fn after_tool_execute(
        &self,
        ctx: &ReadOnlyContext<'_>,
    ) -> ActionSet<AfterToolExecuteAction> {
        let mut combined = ActionSet::empty();
        for b in &self.behaviors {
            combined = combined.and(b.after_tool_execute(ctx).await);
        }
        combined
    }

    async fn step_end(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
        let mut combined = ActionSet::empty();
        for b in &self.behaviors {
            combined = combined.and(b.step_end(ctx).await);
        }
        combined
    }

    async fn run_end(&self, ctx: &ReadOnlyContext<'_>) -> ActionSet<LifecycleAction> {
        let mut combined = ActionSet::empty();
        for b in &self.behaviors {
            combined = combined.and(b.run_end(ctx).await);
        }
        combined
    }
}

/// Kahn's algorithm with stable tie-breaking by original index.
fn topological_sort(
    behaviors: &[Arc<dyn AgentBehavior>],
) -> Result<Vec<Arc<dyn AgentBehavior>>, PluginOrderingCycleError> {
    let n = behaviors.len();
    if n <= 1 {
        return Ok(behaviors.to_vec());
    }

    let mut id_to_idx: HashMap<&str, usize> = HashMap::with_capacity(n);
    for (i, b) in behaviors.iter().enumerate() {
        id_to_idx.entry(b.id()).or_insert(i);
    }

    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut in_degree: Vec<usize> = vec![0; n];

    for (i, b) in behaviors.iter().enumerate() {
        let ord = b.ordering();
        for &dep_id in ord.after {
            if let Some(&dep_idx) = id_to_idx.get(dep_id) {
                if dep_idx != i {
                    adj[dep_idx].push(i);
                    in_degree[i] += 1;
                }
            }
        }
        for &dep_id in ord.before {
            if let Some(&dep_idx) = id_to_idx.get(dep_id) {
                if dep_idx != i {
                    adj[i].push(dep_idx);
                    in_degree[dep_idx] += 1;
                }
            }
        }
    }

    let mut heap: BinaryHeap<std::cmp::Reverse<usize>> = BinaryHeap::new();
    for (i, deg) in in_degree.iter().enumerate() {
        if *deg == 0 {
            heap.push(std::cmp::Reverse(i));
        }
    }

    let mut sorted: Vec<Arc<dyn AgentBehavior>> = Vec::with_capacity(n);
    while let Some(std::cmp::Reverse(idx)) = heap.pop() {
        sorted.push(Arc::clone(&behaviors[idx]));
        for &next in &adj[idx] {
            in_degree[next] -= 1;
            if in_degree[next] == 0 {
                heap.push(std::cmp::Reverse(next));
            }
        }
    }

    if sorted.len() != n {
        let involved: Vec<String> = behaviors
            .iter()
            .enumerate()
            .filter(|(i, _)| in_degree[*i] > 0)
            .map(|(_, b)| b.id().to_string())
            .collect();
        return Err(PluginOrderingCycleError { involved });
    }

    Ok(sorted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::runtime::behavior::PluginOrdering;
    use crate::contracts::runtime::phase::{BeforeInferenceAction, Phase};
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
            ActionSet::single(BeforeInferenceAction::AddContextMessage(
                tirea_contract::runtime::inference::ContextMessage {
                    key: self.id.clone(),
                    content: self.text.clone(),
                    cooldown_turns: 0,
                    target: Default::default(),
                },
            ))
        }
    }

    struct OrderedBehavior {
        id: &'static str,
        text: String,
        ord: PluginOrdering,
    }

    #[async_trait]
    impl AgentBehavior for OrderedBehavior {
        fn id(&self) -> &str {
            self.id
        }
        fn ordering(&self) -> PluginOrdering {
            self.ord.clone()
        }
        async fn before_inference(
            &self,
            _ctx: &ReadOnlyContext<'_>,
        ) -> ActionSet<BeforeInferenceAction> {
            ActionSet::single(BeforeInferenceAction::AddContextMessage(
                tirea_contract::runtime::inference::ContextMessage {
                    key: self.id.to_string(),
                    content: self.text.clone(),
                    cooldown_turns: 0,
                    target: Default::default(),
                },
            ))
        }
    }

    fn make_ctx<'a>(doc: &'a DocCell, rp: &'a RunPolicy, phase: Phase) -> ReadOnlyContext<'a> {
        ReadOnlyContext::new(phase, "t1", &[], rp, doc)
    }

    fn extract_texts(actions: ActionSet<BeforeInferenceAction>) -> Vec<String> {
        actions
            .into_vec()
            .into_iter()
            .filter_map(|a| match a {
                BeforeInferenceAction::AddContextMessage(m) => Some(m.content),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn composite_merges_actions() {
        let b: Vec<Arc<dyn AgentBehavior>> = vec![
            Arc::new(ContextBehavior {
                id: "a".into(),
                text: "A".into(),
            }),
            Arc::new(ContextBehavior {
                id: "b".into(),
                text: "B".into(),
            }),
        ];
        let c = CompositeBehavior::new("t", b).unwrap();
        let doc = DocCell::new(json!({}));
        let rp = RunPolicy::new();
        assert_eq!(
            c.before_inference(&make_ctx(&doc, &rp, Phase::BeforeInference))
                .await
                .len(),
            2
        );
    }

    #[test]
    fn compose_behaviors_empty_returns_noop() {
        assert_eq!(compose_behaviors("t", vec![]).unwrap().id(), "noop");
    }

    #[test]
    fn compose_behaviors_single_passthrough() {
        let input = Arc::new(ContextBehavior {
            id: "s".into(),
            text: "".into(),
        }) as Arc<dyn AgentBehavior>;
        let out = compose_behaviors("x", vec![input.clone()]).unwrap();
        assert!(Arc::ptr_eq(&out, &input));
    }

    #[test]
    fn topological_sort_preserves_order_without_constraints() {
        let b: Vec<Arc<dyn AgentBehavior>> = vec![
            Arc::new(ContextBehavior {
                id: "c".into(),
                text: "".into(),
            }),
            Arc::new(ContextBehavior {
                id: "a".into(),
                text: "".into(),
            }),
        ];
        let sorted = topological_sort(&b).unwrap();
        let ids: Vec<&str> = sorted.iter().map(|x| x.id()).collect();
        assert_eq!(ids, vec!["c", "a"]);
    }

    #[test]
    fn topological_sort_after_constraint() {
        let b: Vec<Arc<dyn AgentBehavior>> = vec![
            Arc::new(OrderedBehavior {
                id: "B",
                text: "".into(),
                ord: PluginOrdering::after(&["A"]),
            }),
            Arc::new(OrderedBehavior {
                id: "A",
                text: "".into(),
                ord: PluginOrdering::NONE,
            }),
        ];
        let sorted = topological_sort(&b).unwrap();
        let ids: Vec<&str> = sorted.iter().map(|x| x.id()).collect();
        assert_eq!(ids, vec!["A", "B"]);
    }

    #[test]
    fn topological_sort_cycle_detected() {
        let b: Vec<Arc<dyn AgentBehavior>> = vec![
            Arc::new(OrderedBehavior {
                id: "A",
                text: "".into(),
                ord: PluginOrdering::after(&["B"]),
            }),
            Arc::new(OrderedBehavior {
                id: "B",
                text: "".into(),
                ord: PluginOrdering::after(&["A"]),
            }),
        ];
        assert!(topological_sort(&b).is_err());
    }

    #[tokio::test]
    async fn ordering_constraint_affects_action_order() {
        let b: Vec<Arc<dyn AgentBehavior>> = vec![
            Arc::new(OrderedBehavior {
                id: "B",
                text: "B".into(),
                ord: PluginOrdering::after(&["A"]),
            }),
            Arc::new(OrderedBehavior {
                id: "A",
                text: "A".into(),
                ord: PluginOrdering::NONE,
            }),
        ];
        let c = CompositeBehavior::new("t", b).unwrap();
        let doc = DocCell::new(json!({}));
        let rp = RunPolicy::new();
        let texts = extract_texts(
            c.before_inference(&make_ctx(&doc, &rp, Phase::BeforeInference))
                .await,
        );
        assert_eq!(texts, vec!["A", "B"]);
    }
}
