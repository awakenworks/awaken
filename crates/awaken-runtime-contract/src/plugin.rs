//! Plugin mechanism: manifests, capability bounds, resolved contributions, and
//! the per-run execution environment.
//!
//! A `Plugin` is a factory that declares a `PluginManifest` (id, dependencies,
//! config sections, and a `CapabilityBound`) and resolves once into
//! `Contributions`. The runtime merges every active plugin's contributions into a
//! `ResolvedExecutionEnv`, enforcing that each plugin's actual contributions are a
//! subset of its declared bound (G30, fail-closed) and that ids are unique and
//! dependency-ordered. Hooks emit state through the commit path; they never write
//! a store or bypass permission (G9).

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::message::Message;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::state::Command as StateCommand;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The phases a hook can observe in one model/tool step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PhaseHookPoint {
    StepStart,
    BeforeInference,
    AfterInference,
    StepEnd,
}

/// The upper bound of what a plugin may contribute. Actual contributions must be
/// a subset of this (G30); anything outside is a fail-closed violation.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CapabilityBound {
    pub tool_ids: Vec<String>,
    pub state_keys: Vec<String>,
    pub phase_hooks: Vec<PhaseHookPoint>,
    /// Scheduled-action kinds this plugin may contribute (ADR-0027) — the
    /// id-bearing axis G30 names alongside tools and state keys.
    pub action_kinds: Vec<String>,
    /// Run-end continuation guard ids this plugin may contribute.
    pub run_end_guards: Vec<String>,
}

/// Declared plugin identity and bound. One `validate`-able home for config.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    /// Other plugin ids that must resolve before this one (ordering only).
    pub requires: Vec<String>,
    pub config_sections: Vec<String>,
    pub bound: CapabilityBound,
}

/// Context passed to a phase hook. Immutable data; a hook returns state commands
/// rather than mutating anything directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseContext {
    pub run_id: RunId,
    pub step: usize,
    pub point: PhaseHookPoint,
}

/// A phase hook: behavior contributed by a plugin at one phase point. Async so a
/// real hook can consult an external system; it can only stage state commands.
#[async_trait]
pub trait PhaseHook: Send + Sync {
    fn point(&self) -> PhaseHookPoint;
    async fn on_phase(&self, ctx: &PhaseContext) -> Vec<StateCommand>;
}

/// What a run-end guard sees when the model/tool loop reaches a natural end (a
/// text-only turn). Immutable: a guard reads the conversation and the run-scoped
/// forced-continuation count, then returns a decision.
pub struct RunEndContext<'a> {
    pub run_id: RunId,
    /// The full conversation transcript at the natural-end boundary.
    pub conversation: &'a [Message],
    /// How many times a guard has already steered this run — the runtime's
    /// run-scoped continuation counter. A guard reads it to enforce its own
    /// iteration budget; the runtime also caps total steps as a runaway backstop.
    pub forced_continuations: usize,
}

/// A run-end guard's decision at a natural-end boundary. The runtime owns *when*
/// the loop stops; the guard supplies the *predicate* and any feedback. `detail`
/// is opaque to the runtime (anti-corruption): the guard's own classification,
/// forwarded to the host without the kernel interpreting it.
pub enum RunEndDecision {
    /// End the run. `detail` is surfaced to the host as an opaque round result.
    Complete { detail: serde_json::Value },
    /// Continue for another turn: append `feedback` as a user message and loop.
    /// `detail` describes this non-terminal round, opaque to the runtime.
    Steer {
        feedback: String,
        detail: serde_json::Value,
    },
}

/// A run-end continuation guard: consulted at the natural-end boundary to decide
/// whether the run ends or takes another steered turn (e.g. goal/outcome
/// evaluation). The runtime consults registered guards in dependency order and
/// takes the first that steers; if none steer, the run ends carrying the last
/// guard's completion detail. Async so a guard can grade through an external
/// judge before deciding.
#[async_trait]
pub trait RunEndGuard: Send + Sync {
    /// Stable id, checked against the plugin's `CapabilityBound` (G30).
    fn id(&self) -> &str;
    async fn evaluate(&self, ctx: &RunEndContext<'_>) -> RunEndDecision;
}

/// One plugin's resolved contributions. Built once by `Plugin::resolve`; holds
/// live hook behavior, so it is runtime-side wiring, not serialized truth.
#[derive(Clone)]
pub struct Contributions {
    pub plugin_id: String,
    pub tools: Vec<String>,
    pub state_keys: Vec<String>,
    pub phase_hooks: Vec<Arc<dyn PhaseHook>>,
    /// Scheduled-action kinds this plugin contributes (ADR-0027).
    pub action_kinds: Vec<String>,
    /// Run-end continuation guards this plugin contributes.
    pub run_end_guards: Vec<Arc<dyn RunEndGuard>>,
}

impl Contributions {
    pub fn new(plugin_id: impl Into<String>) -> Self {
        Self {
            plugin_id: plugin_id.into(),
            tools: Vec::new(),
            state_keys: Vec::new(),
            phase_hooks: Vec::new(),
            action_kinds: Vec::new(),
            run_end_guards: Vec::new(),
        }
    }
}

/// A plugin factory. `resolve` is called once per run to compile contributions
/// from config; it must not perform mutable registration side effects.
pub trait Plugin: Send + Sync {
    fn manifest(&self) -> PluginManifest;
    fn resolve(&self) -> Contributions;
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BoundViolation {
    #[error("plugin {plugin} contributes tool {id:?} outside its declared bound")]
    Tool { plugin: String, id: String },
    #[error("plugin {plugin} contributes state key {id:?} outside its declared bound")]
    StateKey { plugin: String, id: String },
    #[error("plugin {plugin} registers hook {point:?} outside its declared bound")]
    Hook {
        plugin: String,
        point: PhaseHookPoint,
    },
    #[error("plugin {plugin} contributes action kind {id:?} outside its declared bound")]
    ActionKind { plugin: String, id: String },
    #[error("plugin {plugin} contributes run-end guard {id:?} outside its declared bound")]
    RunEndGuard { plugin: String, id: String },
}

/// Enforce that a plugin's actual contributions are a subset of its bound (G30).
pub fn enforce_bound(
    manifest: &PluginManifest,
    contributions: &Contributions,
) -> Result<(), BoundViolation> {
    for tool in &contributions.tools {
        if !manifest.bound.tool_ids.contains(tool) {
            return Err(BoundViolation::Tool {
                plugin: manifest.id.clone(),
                id: tool.clone(),
            });
        }
    }
    for key in &contributions.state_keys {
        if !manifest.bound.state_keys.contains(key) {
            return Err(BoundViolation::StateKey {
                plugin: manifest.id.clone(),
                id: key.clone(),
            });
        }
    }
    for hook in &contributions.phase_hooks {
        if !manifest.bound.phase_hooks.contains(&hook.point()) {
            return Err(BoundViolation::Hook {
                plugin: manifest.id.clone(),
                point: hook.point(),
            });
        }
    }
    for kind in &contributions.action_kinds {
        if !manifest.bound.action_kinds.contains(kind) {
            return Err(BoundViolation::ActionKind {
                plugin: manifest.id.clone(),
                id: kind.clone(),
            });
        }
    }
    for guard in &contributions.run_end_guards {
        if !manifest
            .bound
            .run_end_guards
            .iter()
            .any(|id| id == guard.id())
        {
            return Err(BoundViolation::RunEndGuard {
                plugin: manifest.id.clone(),
                id: guard.id().to_string(),
            });
        }
    }
    Ok(())
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MergeError {
    #[error("bound violation: {0}")]
    Bound(#[from] BoundViolation),
    #[error("duplicate tool id {id:?} contributed by {first} and {second}")]
    DuplicateTool {
        id: String,
        first: String,
        second: String,
    },
    #[error("duplicate action kind {id:?} contributed by {first} and {second}")]
    DuplicateActionKind {
        id: String,
        first: String,
        second: String,
    },
    #[error("plugin {plugin} requires {missing:?}, which is not active")]
    MissingDependency { plugin: String, missing: String },
    #[error("dependency cycle among active plugins")]
    DependencyCycle,
}

/// The merged contributions of every active plugin for one run. Built by
/// validating each plugin against its bound, rejecting duplicate tool ids, and
/// ordering plugins so dependencies resolve first.
pub struct ResolvedExecutionEnv {
    pub order: Vec<String>,
    pub tools: Vec<String>,
    pub state_keys: Vec<String>,
    pub phase_hooks: Vec<Arc<dyn PhaseHook>>,
    /// Scheduled-action kinds the selected plugins contribute. A kind absent here
    /// (its plugin is not selected for the run) cannot be staged (ADR-0027).
    pub action_kinds: Vec<String>,
    /// Run-end continuation guards the selected plugins contribute, in dependency
    /// order. Consulted at the natural-end boundary.
    pub run_end_guards: Vec<Arc<dyn RunEndGuard>>,
}

impl ResolvedExecutionEnv {
    /// Merge active `(manifest, contributions)` pairs. Fails closed on a bound
    /// violation, duplicate tool id, missing dependency, or dependency cycle.
    pub fn merge(plugins: Vec<(PluginManifest, Contributions)>) -> Result<Self, MergeError> {
        for (manifest, contributions) in &plugins {
            enforce_bound(manifest, contributions)?;
        }

        let active: Vec<String> = plugins.iter().map(|(m, _)| m.id.clone()).collect();
        for (manifest, _) in &plugins {
            for required in &manifest.requires {
                if !active.contains(required) {
                    return Err(MergeError::MissingDependency {
                        plugin: manifest.id.clone(),
                        missing: required.clone(),
                    });
                }
            }
        }

        let order = topological_order(&plugins)?;

        // Re-emit contributions in dependency order.
        let mut tools: Vec<String> = Vec::new();
        let mut tool_owner: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        let mut state_keys: Vec<String> = Vec::new();
        let mut phase_hooks: Vec<Arc<dyn PhaseHook>> = Vec::new();
        let mut run_end_guards: Vec<Arc<dyn RunEndGuard>> = Vec::new();
        let mut action_kinds: Vec<String> = Vec::new();
        let mut action_owner: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();

        for id in &order {
            let (_, contributions) = plugins
                .iter()
                .find(|(m, _)| &m.id == id)
                .expect("ordered id is present");
            for tool in &contributions.tools {
                if let Some(first) = tool_owner.get(tool) {
                    return Err(MergeError::DuplicateTool {
                        id: tool.clone(),
                        first: first.clone(),
                        second: id.clone(),
                    });
                }
                tool_owner.insert(tool.clone(), id.clone());
                tools.push(tool.clone());
            }
            for key in &contributions.state_keys {
                if !state_keys.contains(key) {
                    state_keys.push(key.clone());
                }
            }
            phase_hooks.extend(contributions.phase_hooks.iter().cloned());
            run_end_guards.extend(contributions.run_end_guards.iter().cloned());
            for kind in &contributions.action_kinds {
                if let Some(first) = action_owner.get(kind) {
                    return Err(MergeError::DuplicateActionKind {
                        id: kind.clone(),
                        first: first.clone(),
                        second: id.clone(),
                    });
                }
                action_owner.insert(kind.clone(), id.clone());
                action_kinds.push(kind.clone());
            }
        }

        Ok(Self {
            order,
            tools,
            state_keys,
            phase_hooks,
            action_kinds,
            run_end_guards,
        })
    }

    /// Whether a scheduled-action `kind` is contributed by a selected plugin — the
    /// fail-closed check before staging a kind-based scheduled action (ADR-0027).
    pub fn permits_action_kind(&self, kind: &str) -> bool {
        self.action_kinds.iter().any(|k| k == kind)
    }

    /// Hooks registered for one phase point, in dependency order.
    pub fn hooks_for(&self, point: PhaseHookPoint) -> Vec<Arc<dyn PhaseHook>> {
        self.phase_hooks
            .iter()
            .filter(|h| h.point() == point)
            .cloned()
            .collect()
    }

    /// Run-end continuation guards, in dependency order.
    pub fn run_end_guards(&self) -> &[Arc<dyn RunEndGuard>] {
        &self.run_end_guards
    }
}

/// Order plugins so every `requires` dependency precedes the plugin (Kahn). The
/// input order breaks ties, keeping merges deterministic.
fn topological_order(
    plugins: &[(PluginManifest, Contributions)],
) -> Result<Vec<String>, MergeError> {
    let ids: Vec<String> = plugins.iter().map(|(m, _)| m.id.clone()).collect();
    let mut ordered: Vec<String> = Vec::new();

    while ordered.len() < ids.len() {
        let mut progressed = false;
        for (manifest, _) in plugins {
            if ordered.contains(&manifest.id) {
                continue;
            }
            let deps_ready = manifest.requires.iter().all(|dep| ordered.contains(dep));
            if deps_ready {
                ordered.push(manifest.id.clone());
                progressed = true;
            }
        }
        if !progressed {
            return Err(MergeError::DependencyCycle);
        }
    }
    Ok(ordered)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeHook(PhaseHookPoint);

    #[async_trait]
    impl PhaseHook for FakeHook {
        fn point(&self) -> PhaseHookPoint {
            self.0
        }
        async fn on_phase(&self, _ctx: &PhaseContext) -> Vec<StateCommand> {
            Vec::new()
        }
    }

    fn manifest(id: &str, bound: CapabilityBound) -> PluginManifest {
        PluginManifest {
            id: id.to_string(),
            requires: Vec::new(),
            config_sections: Vec::new(),
            bound,
        }
    }

    #[test]
    fn enforce_bound_accepts_a_subset() {
        let m = manifest(
            "p",
            CapabilityBound {
                tool_ids: vec!["t".into()],
                state_keys: vec!["k".into()],
                phase_hooks: vec![PhaseHookPoint::StepStart],
                action_kinds: vec!["a".into()],
                ..Default::default()
            },
        );
        let mut c = Contributions::new("p");
        c.tools.push("t".into());
        c.state_keys.push("k".into());
        c.action_kinds.push("a".into());
        c.phase_hooks
            .push(Arc::new(FakeHook(PhaseHookPoint::StepStart)));
        assert!(enforce_bound(&m, &c).is_ok());
    }

    #[test]
    fn enforce_bound_rejects_out_of_bound_contributions() {
        let m = manifest("p", CapabilityBound::default());

        let mut tool = Contributions::new("p");
        tool.tools.push("t".into());
        assert!(matches!(
            enforce_bound(&m, &tool),
            Err(BoundViolation::Tool { .. })
        ));

        let mut key = Contributions::new("p");
        key.state_keys.push("k".into());
        assert!(matches!(
            enforce_bound(&m, &key),
            Err(BoundViolation::StateKey { .. })
        ));

        let mut hook = Contributions::new("p");
        hook.phase_hooks
            .push(Arc::new(FakeHook(PhaseHookPoint::StepEnd)));
        assert!(matches!(
            enforce_bound(&m, &hook),
            Err(BoundViolation::Hook { .. })
        ));

        let mut kind = Contributions::new("p");
        kind.action_kinds.push("a".into());
        assert!(matches!(
            enforce_bound(&m, &kind),
            Err(BoundViolation::ActionKind { .. })
        ));
    }

    fn with_action_kind(id: &str, kind: &str) -> (PluginManifest, Contributions) {
        let m = manifest(
            id,
            CapabilityBound {
                action_kinds: vec![kind.into()],
                ..Default::default()
            },
        );
        let mut c = Contributions::new(id);
        c.action_kinds.push(kind.into());
        (m, c)
    }

    #[test]
    fn merge_collects_action_kinds_and_rejects_duplicates() {
        // A selected plugin's action kind is in the resolved env; an unselected
        // one's is absent (RS-SCH-005).
        let env =
            ResolvedExecutionEnv::merge(vec![with_action_kind("p", "remind")]).expect("merges");
        assert!(env.permits_action_kind("remind"));
        assert!(!env.permits_action_kind("not-contributed"));

        let dup = vec![with_action_kind("a", "k"), with_action_kind("b", "k")];
        assert!(matches!(
            ResolvedExecutionEnv::merge(dup),
            Err(MergeError::DuplicateActionKind { .. })
        ));
    }

    fn with_tool(id: &str, tool: &str) -> (PluginManifest, Contributions) {
        let m = manifest(
            id,
            CapabilityBound {
                tool_ids: vec![tool.into()],
                ..Default::default()
            },
        );
        let mut c = Contributions::new(id);
        c.tools.push(tool.into());
        (m, c)
    }

    #[test]
    fn merge_rejects_duplicate_tool_ids() {
        let plugins = vec![with_tool("a", "dup"), with_tool("b", "dup")];
        assert!(matches!(
            ResolvedExecutionEnv::merge(plugins),
            Err(MergeError::DuplicateTool { .. })
        ));
    }

    #[test]
    fn merge_rejects_missing_dependency() {
        let mut m = manifest("a", CapabilityBound::default());
        m.requires.push("missing".into());
        let plugins = vec![(m, Contributions::new("a"))];
        assert!(matches!(
            ResolvedExecutionEnv::merge(plugins),
            Err(MergeError::MissingDependency { .. })
        ));
    }

    #[test]
    fn merge_rejects_a_dependency_cycle() {
        let mut a = manifest("a", CapabilityBound::default());
        a.requires.push("b".into());
        let mut b = manifest("b", CapabilityBound::default());
        b.requires.push("a".into());
        let plugins = vec![(a, Contributions::new("a")), (b, Contributions::new("b"))];
        assert_eq!(
            ResolvedExecutionEnv::merge(plugins).err(),
            Some(MergeError::DependencyCycle)
        );
    }

    #[test]
    fn merge_orders_dependencies_first() {
        let mut b = manifest("b", CapabilityBound::default());
        b.requires.push("a".into());
        let a = manifest("a", CapabilityBound::default());
        // Input order is [b, a]; a must come first because b requires it.
        let plugins = vec![(b, Contributions::new("b")), (a, Contributions::new("a"))];
        let env = ResolvedExecutionEnv::merge(plugins).expect("merges");
        assert_eq!(env.order, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn hooks_for_filters_by_point() {
        let m = manifest(
            "p",
            CapabilityBound {
                phase_hooks: vec![PhaseHookPoint::StepStart, PhaseHookPoint::StepEnd],
                ..Default::default()
            },
        );
        let mut c = Contributions::new("p");
        c.phase_hooks
            .push(Arc::new(FakeHook(PhaseHookPoint::StepStart)));
        c.phase_hooks
            .push(Arc::new(FakeHook(PhaseHookPoint::StepEnd)));
        let env = ResolvedExecutionEnv::merge(vec![(m, c)]).expect("merges");
        assert_eq!(env.hooks_for(PhaseHookPoint::StepStart).len(), 1);
        assert_eq!(env.hooks_for(PhaseHookPoint::StepEnd).len(), 1);
        assert_eq!(env.hooks_for(PhaseHookPoint::BeforeInference).len(), 0);
    }
}
