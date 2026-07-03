//! Recall as a plugin hook.
//!
//! [`MemoryPlugin`] contributes a `BeforeInference` [`PhaseHook`] that injects
//! bounded recall of saved memories as **request-only** context — the same
//! message-injection shape a tool-outcome hook uses, but at inference time and
//! never committed (G13). This is how recall reaches the model through the plugin
//! framework (under a `CapabilityBound`, G30) rather than a host side channel.
//!
//! The hook does bounded recall (①) only: a phase hook has no model handle, so
//! relevance selection (③) — which needs a model call — stays a host concern.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_runtime_contract::plugin::{
    CapabilityBound, Contributions, PhaseContext, PhaseHook, PhaseHookPoint, PhaseReaction, Plugin,
    PluginManifest,
};

use crate::recall::{RecallBounds, recall_block};
use crate::store::MemoryStore;

/// The plugin id under which memory recall is activated (must be listed in a run's
/// `plugin_ids` to contribute, G30).
pub const MEMORY_PLUGIN_ID: &str = "memory";

/// Contributes the recall hook. Constructed at the composition root with the
/// memory store (shared with extraction) and the recall bounds.
pub struct MemoryPlugin {
    store: MemoryStore,
    bounds: RecallBounds,
}

impl MemoryPlugin {
    pub fn new(store: MemoryStore, bounds: RecallBounds) -> Self {
        Self { store, bounds }
    }
}

impl Plugin for MemoryPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: MEMORY_PLUGIN_ID.into(),
            requires: Vec::new(),
            config_sections: Vec::new(),
            bound: CapabilityBound {
                phase_hooks: vec![PhaseHookPoint::BeforeInference],
                ..Default::default()
            },
        }
    }

    fn resolve(&self) -> Contributions {
        let mut contributions = Contributions::new(MEMORY_PLUGIN_ID);
        contributions.phase_hooks.push(Arc::new(RecallHook {
            store: self.store.clone(),
            bounds: self.bounds.clone(),
        }));
        contributions
    }
}

/// The `BeforeInference` hook: read the store and return bounded recall as
/// request-only context.
struct RecallHook {
    store: MemoryStore,
    bounds: RecallBounds,
}

#[async_trait]
impl PhaseHook for RecallHook {
    fn point(&self) -> PhaseHookPoint {
        PhaseHookPoint::BeforeInference
    }

    async fn on_phase(&self, _ctx: &PhaseContext) -> PhaseReaction {
        match recall_block(&self.store, &self.bounds) {
            Some(block) => PhaseReaction::context(vec![Message::text(
                MessageId("mem-recall".into()),
                Role::System,
                block,
            )]),
            None => PhaseReaction::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(entries: &[(&str, &str)]) -> MemoryStore {
        let root = std::env::temp_dir().join(format!(
            "awaken-mem-plugin-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = MemoryStore::new(&root);
        for (n, c) in entries {
            store.write(n, c).unwrap();
        }
        store
    }

    #[test]
    fn manifest_declares_the_before_inference_hook() {
        let plugin = MemoryPlugin::new(store_with(&[]), RecallBounds::default());
        let bound = plugin.manifest().bound;
        assert_eq!(bound.phase_hooks, vec![PhaseHookPoint::BeforeInference]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hook_injects_recall_as_request_only_context() {
        let plugin = MemoryPlugin::new(
            store_with(&[("pref", "user likes tea")]),
            RecallBounds::default(),
        );
        let hook = &plugin.resolve().phase_hooks[0];
        let reaction = hook
            .on_phase(&PhaseContext {
                run_id: awaken_agent_contract::agent::run::Id("r".into()),
                step: 0,
                point: PhaseHookPoint::BeforeInference,
            })
            .await;
        assert!(reaction.state.is_empty(), "recall stages no state");
        assert_eq!(reaction.context.len(), 1);
        assert_eq!(reaction.context[0].role, Role::System);
        assert!(
            reaction.context[0]
                .text_content()
                .contains("user likes tea")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn empty_store_injects_nothing() {
        let plugin = MemoryPlugin::new(store_with(&[]), RecallBounds::default());
        let hook = &plugin.resolve().phase_hooks[0];
        let reaction = hook
            .on_phase(&PhaseContext {
                run_id: awaken_agent_contract::agent::run::Id("r".into()),
                step: 0,
                point: PhaseHookPoint::BeforeInference,
            })
            .await;
        assert!(reaction.context.is_empty());
    }
}
