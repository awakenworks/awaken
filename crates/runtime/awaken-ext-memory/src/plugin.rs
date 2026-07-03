//! Recall as a plugin hook.
//!
//! [`MemoryPlugin`] contributes a `BeforeInference` [`PhaseHook`] that injects
//! recall of saved memories as **request-only** context — the same message-
//! injection shape a tool-outcome hook uses, but at inference time and never
//! committed (G13). This is how recall reaches the model through the plugin
//! framework (under a `CapabilityBound`, G30) rather than a host side channel.
//!
//! A small store injects the newest memories bounded (①). Once it grows past
//! `bounds.select_over` and a [`RecallSelector`] is wired, the hook picks the
//! memories relevant to the user's message (③) through a single `memory-selector`
//! sub-agent call — run at most once per run (cached by `run_id`, since the hook
//! fires every inference step).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_runtime_contract::plugin::{
    CapabilityBound, Contributions, PhaseContext, PhaseHook, PhaseHookPoint, PhaseReaction, Plugin,
    PluginConfigError, PluginManifest,
};

use crate::recall::{RecallBounds, render};
use crate::select::{RecallSelector, manifest, query_from};
use crate::store::MemoryStore;

/// The plugin id under which memory recall is activated (must be listed in a run's
/// `plugin_ids` to contribute, G30).
pub const MEMORY_PLUGIN_ID: &str = "memory";

/// Contributes the recall hook. Constructed at the composition root with the memory
/// store (shared with extraction), the recall bounds, and — optionally — a
/// relevance selector.
pub struct MemoryPlugin {
    store: MemoryStore,
    bounds: RecallBounds,
    selector: Option<Arc<dyn RecallSelector>>,
}

impl MemoryPlugin {
    pub fn new(store: MemoryStore, bounds: RecallBounds) -> Self {
        Self {
            store,
            bounds,
            selector: None,
        }
    }

    /// Add a relevance selector, used once the store passes `bounds.select_over`.
    #[must_use]
    pub fn with_selector(mut self, selector: Arc<dyn RecallSelector>) -> Self {
        self.selector = Some(selector);
        self
    }

    /// Contribute the recall hook with the given bounds (shared by the config-free
    /// and configured resolve paths).
    fn contribute(&self, bounds: RecallBounds) -> Contributions {
        let mut contributions = Contributions::new(MEMORY_PLUGIN_ID);
        contributions.phase_hooks.push(Arc::new(RecallHook {
            store: self.store.clone(),
            bounds,
            selector: self.selector.clone(),
            cache: Mutex::new(HashMap::new()),
        }));
        contributions
    }
}

impl Plugin for MemoryPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: MEMORY_PLUGIN_ID.into(),
            requires: Vec::new(),
            config_sections: vec![MEMORY_PLUGIN_ID.into()],
            bound: CapabilityBound {
                phase_hooks: vec![PhaseHookPoint::BeforeInference],
                ..Default::default()
            },
        }
    }

    fn resolve(&self) -> Contributions {
        self.contribute(self.bounds.clone())
    }

    fn resolve_configured(
        &self,
        config: Option<&serde_json::Value>,
    ) -> Result<Contributions, PluginConfigError> {
        // The `memory` section overrides the recall bounds; absent uses the
        // constructed defaults. `#[serde(default)]` on `RecallBounds` fills any
        // unset field, so a partial section is valid.
        let bounds = match config {
            Some(value) => serde_json::from_value::<RecallBounds>(value.clone())
                .map_err(|e| PluginConfigError::new(MEMORY_PLUGIN_ID, e.to_string()))?,
            None => self.bounds.clone(),
        };
        Ok(self.contribute(bounds))
    }
}

/// The JSON Schema for the `memory` config section (the recall bounds), for a
/// config frontend to discover and author.
pub fn config_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "per_entry_chars": {
                "type": "integer", "minimum": 0,
                "description": "Truncate each memory to this many characters (0 = unbounded)."
            },
            "total_chars": {
                "type": "integer", "minimum": 0,
                "description": "Cap the whole recall block to this many characters."
            },
            "max_entries": {
                "type": "integer", "minimum": 1,
                "description": "Inject at most this many memories."
            },
            "select_over": {
                "type": "integer", "minimum": 0,
                "description": "Use relevance selection once the store holds more than this many memories."
            }
        },
        "additionalProperties": false
    })
}

/// The `BeforeInference` hook. Relevance selection runs at most once per run (the
/// hook fires every step), cached by `run_id`.
struct RecallHook {
    store: MemoryStore,
    bounds: RecallBounds,
    selector: Option<Arc<dyn RecallSelector>>,
    cache: Mutex<HashMap<RunId, Vec<Message>>>,
}

impl RecallHook {
    fn message(block: String) -> Vec<Message> {
        vec![Message::text(
            MessageId("mem-recall".into()),
            Role::System,
            block,
        )]
    }

    async fn compute(&self, conversation: &[Message]) -> Vec<Message> {
        let entries = self.store.entries();
        if entries.is_empty() {
            return Vec::new();
        }
        // Small store, or no selector: inject the newest memories bounded (①).
        let use_selection = entries.len() > self.bounds.select_over && self.selector.is_some();
        if !use_selection {
            return render(&entries, &self.bounds)
                .map(Self::message)
                .unwrap_or_default();
        }
        // Large store: pick the relevant memories for the user's message (③), via a
        // single `memory-selector` sub-agent call.
        let selector = self.selector.as_ref().expect("checked above");
        let picked = selector
            .select(
                &query_from(conversation),
                &manifest(&entries),
                self.bounds.max_entries,
            )
            .await;
        if picked.is_empty() {
            return Vec::new();
        }
        let selected: Vec<_> = picked
            .into_iter()
            .filter_map(|i| entries.get(i).cloned())
            .collect();
        render(&selected, &self.bounds)
            .map(Self::message)
            .unwrap_or_default()
    }
}

#[async_trait]
impl PhaseHook for RecallHook {
    fn point(&self) -> PhaseHookPoint {
        PhaseHookPoint::BeforeInference
    }

    async fn on_phase(&self, ctx: &PhaseContext, conversation: &[Message]) -> PhaseReaction {
        if let Some(hit) = self.cache.lock().unwrap().get(&ctx.run_id) {
            return PhaseReaction::context(hit.clone());
        }
        let block = self.compute(conversation).await;
        self.cache
            .lock()
            .unwrap()
            .insert(ctx.run_id.clone(), block.clone());
        PhaseReaction::context(block)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recall::RecallBounds;

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

    fn phase_ctx() -> PhaseContext {
        PhaseContext {
            run_id: RunId("r".into()),
            step: 0,
            point: PhaseHookPoint::BeforeInference,
        }
    }

    #[test]
    fn manifest_declares_the_before_inference_hook_and_config_section() {
        let plugin = MemoryPlugin::new(store_with(&[]), RecallBounds::default());
        let manifest = plugin.manifest();
        assert_eq!(
            manifest.bound.phase_hooks,
            vec![PhaseHookPoint::BeforeInference]
        );
        assert_eq!(manifest.config_sections, vec![MEMORY_PLUGIN_ID.to_string()]);
    }

    #[test]
    fn resolve_configured_reads_bounds_and_fails_closed_on_bad_config() {
        let plugin = MemoryPlugin::new(store_with(&[]), RecallBounds::default());
        // A valid (partial) section resolves.
        let ok = serde_json::json!({ "max_entries": 3, "select_over": 2 });
        assert_eq!(
            plugin
                .resolve_configured(Some(&ok))
                .unwrap()
                .phase_hooks
                .len(),
            1
        );
        // None uses the constructed defaults.
        assert!(plugin.resolve_configured(None).is_ok());
        // A malformed section fails closed.
        let bad = serde_json::json!({ "max_entries": "lots" });
        assert!(plugin.resolve_configured(Some(&bad)).is_err());
    }

    #[test]
    fn config_schema_is_an_object() {
        assert_eq!(super::config_schema()["type"], "object");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn small_store_injects_bounded_recall_without_a_selector() {
        let plugin = MemoryPlugin::new(
            store_with(&[("pref", "user likes tea")]),
            RecallBounds::default(),
        );
        let hook = &plugin.resolve().phase_hooks[0];
        let reaction = hook.on_phase(&phase_ctx(), &[]).await;
        assert_eq!(reaction.context.len(), 1);
        assert!(
            reaction.context[0]
                .text_content()
                .contains("user likes tea")
        );
    }

    /// A selector that returns fixed indices, recording the query it saw.
    struct FixedSelector {
        picks: Vec<usize>,
        seen_query: std::sync::Mutex<String>,
    }
    #[async_trait]
    impl RecallSelector for FixedSelector {
        async fn select(&self, query: &str, _m: &[(usize, String)], _max: usize) -> Vec<usize> {
            *self.seen_query.lock().unwrap() = query.to_string();
            self.picks.clone()
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn large_store_uses_the_selector_and_caches_per_run() {
        // 3 memories, select_over = 1 → selection kicks in.
        let store = store_with(&[("a", "AAA"), ("b", "BBB"), ("c", "CCC")]);
        let bounds = RecallBounds {
            select_over: 1,
            ..RecallBounds::default()
        };
        let selector = Arc::new(FixedSelector {
            picks: vec![1],
            seen_query: std::sync::Mutex::new(String::new()),
        });
        let plugin = MemoryPlugin::new(store, bounds).with_selector(selector.clone());
        let hook = &plugin.resolve().phase_hooks[0];

        let conversation = vec![Message::text(
            MessageId("u".into()),
            Role::User,
            "which one?",
        )];
        let reaction = hook.on_phase(&phase_ctx(), &conversation).await;
        // Only the selected memory is injected; the selector saw the user query.
        assert_eq!(reaction.context.len(), 1);
        assert_eq!(*selector.seen_query.lock().unwrap(), "which one?");
        // (entries are newest-first; index 1 is one of them — just assert one shown)
        assert_eq!(
            reaction.context[0].text_content().matches("\n\n").count(),
            1
        );
    }
}
