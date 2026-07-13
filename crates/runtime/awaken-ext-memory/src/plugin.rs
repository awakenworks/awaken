//! Recall as a plugin hook.
//!
//! [`MemoryPlugin`] contributes a `BeforeInference` [`PhaseHook`] that injects
//! recall of saved memories as **request-only** context — prepended at inference
//! time and never committed (G13). This is how recall reaches the model through
//! the plugin framework (under a `CapabilityBound`, G30) rather than a host side
//! channel.
//!
//! A small store injects the newest memories bounded (①). Once it grows past
//! `bounds.select_over` and a [`RecallSelector`] is wired, the hook picks the
//! memories relevant to the user's message (③) through a single `memory-selector`
//! sub-agent call — run at most once per run, gated on the run-scoped
//! [`RecallContext`] state so it replays across steps and a resumed run instead
//! of recomputing (ADR-0055).

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::state::{MergePolicy, Scope, StateKey, Store};
use awaken_runtime_contract::plugin::{
    CapabilityBound, Contributions, HookReaction, IdBound, PhaseContext, PhaseHook, PhaseHookPoint,
    Plugin, PluginConfigError, PluginManifest,
};

use crate::localfs::MemoryDir;
use crate::recall::{RecallBounds, render};
use crate::select::{RecallSelector, manifest, query_from};

/// The plugin id under which memory recall is activated (must be listed in a run's
/// `plugin_ids` to contribute, G30).
pub const MEMORY_PLUGIN_ID: &str = "memory";

/// Contributes the recall hook. Constructed at the composition root with the memory
/// store (shared with extraction), the recall bounds, and — optionally — a
/// relevance selector.
pub struct MemoryPlugin {
    store: MemoryDir,
    bounds: RecallBounds,
    selector: Option<Arc<dyn RecallSelector>>,
}

impl MemoryPlugin {
    pub fn new(store: MemoryDir, bounds: RecallBounds) -> Self {
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
        contributions.declare_state_key(RecallContext::KEY);
        contributions.register_hook(Arc::new(RecallHook {
            store: self.store.clone(),
            bounds,
            selector: self.selector.clone(),
        }));
        contributions
    }
}

/// The run-scoped cell holding this run's computed recall block. `None` (absent)
/// means recall has not run yet; `Some(block)` (possibly empty) means it has, so a
/// later step — or a resumed run replaying committed state — re-injects the same
/// block without re-running the relevance selector (ADR-0055). Replaces the former
/// per-`run_id` in-process cache, which did not survive resume.
struct RecallContext;
impl StateKey for RecallContext {
    const KEY: &'static str = "recall_context";
    const SCOPE: Scope = Scope::Run;
    const MERGE: MergePolicy = MergePolicy::Exclusive;
    type Value = Option<Vec<Message>>;
}

impl Plugin for MemoryPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: MEMORY_PLUGIN_ID.into(),
            requires: Vec::new(),
            config_sections: vec![MEMORY_PLUGIN_ID.into()],
            bound: CapabilityBound {
                phase_hooks: vec![PhaseHookPoint::BeforeInference],
                state_keys: IdBound::Exact(vec![RecallContext::KEY.into()]),
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
/// hook fires every step), gated on the run-scoped [`RecallContext`] state so the
/// block replays across steps and a resumed run rather than recomputing.
struct RecallHook {
    store: MemoryDir,
    bounds: RecallBounds,
    selector: Option<Arc<dyn RecallSelector>>,
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

    async fn on_phase(
        &self,
        _ctx: &PhaseContext,
        conversation: &[Message],
        state: &Store,
    ) -> HookReaction {
        // Already computed this run (replayed across steps and resume): re-inject
        // the same request-only block without re-running the selector.
        if let Some(block) = RecallContext::load_or_default(state) {
            return HookReaction::messages(block);
        }
        let block = self.compute(conversation).await;
        HookReaction {
            state: vec![RecallContext::write(&Some(block.clone()))],
            messages: block,
        }
    }
}

#[cfg(test)]
mod tests {
    use awaken_agent_contract::agent::run::Id as RunId;

    use super::*;
    use crate::recall::RecallBounds;

    fn store_with(entries: &[(&str, &str)]) -> MemoryDir {
        let root = std::env::temp_dir().join(format!(
            "awaken-mem-plugin-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = MemoryDir::new(&root);
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
            after_tool: None,
        }
    }

    #[test]
    fn recall_context_key_writes_and_reads_back() {
        let block = vec![Message::text(
            MessageId("m".into()),
            Role::System,
            "recalled",
        )];
        let mut store = Store::new();
        store.apply(&RecallContext::write(&Some(block.clone())));
        assert_eq!(RecallContext::load_or_default(&store), Some(block));
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
        let reaction = hook.on_phase(&phase_ctx(), &[], &Store::new()).await;
        assert_eq!(reaction.messages.len(), 1);
        assert!(
            reaction.messages[0]
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
    async fn large_store_uses_the_selector_and_gates_on_run_state() {
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
        let mut state = Store::new();
        let reaction = hook.on_phase(&phase_ctx(), &conversation, &state).await;
        // Only the selected memory is injected; the selector saw the user query.
        assert_eq!(reaction.messages.len(), 1);
        assert_eq!(*selector.seen_query.lock().unwrap(), "which one?");
        // (entries are newest-first; index 1 is one of them — just assert one shown)
        assert_eq!(
            reaction.messages[0].text_content().matches("\n\n").count(),
            1
        );
        // The first call staged its recall block into run state.
        assert!(!reaction.state.is_empty());

        // Apply that state (as the engine does), then a later step of the same run
        // replays the block without re-running the selector — the gate the deleted
        // per-run cache used to provide, now resume-safe (ADR-0055).
        for command in &reaction.state {
            state.apply(command);
        }
        *selector.seen_query.lock().unwrap() = String::new();
        let replay = hook.on_phase(&phase_ctx(), &conversation, &state).await;
        assert_eq!(replay.messages.len(), 1);
        assert!(replay.state.is_empty(), "a replay stages no new state");
        assert_eq!(
            *selector.seen_query.lock().unwrap(),
            "",
            "selector must not re-run once recall is computed for the run"
        );
    }
}
