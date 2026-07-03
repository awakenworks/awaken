//! Compaction as a plugin hook.
//!
//! [`CompactPlugin`] contributes a `BeforeInference` [`PhaseHook`] symmetric with
//! memory recall: on a long conversation it summarizes the older messages through
//! an injected [`Summarizer`] sub-agent and injects the summary as **request-only**
//! context (never committed, G13). The main agent's `ContextPolicy::KeepLast` drops
//! the older raw turns from the model view, so summary + kept tail cover the whole
//! conversation. Summarization runs at most once per run (cached by `run_id`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_runtime_contract::plugin::{
    CapabilityBound, Contributions, PhaseContext, PhaseHook, PhaseHookPoint, PhaseReaction, Plugin,
    PluginConfigError, PluginManifest,
};

use crate::config::CompactConfig;
use crate::fold::fold_point;

/// The plugin id under which compaction is activated (G30).
pub const COMPACT_PLUGIN_ID: &str = "compact";

/// Summarizes the older part of a conversation. Implemented by the host over a
/// `compactor` sub-agent, so this crate stays free of the aux-agent substrate.
#[async_trait]
pub trait Summarizer: Send + Sync {
    /// Summarize `older` into a short block, or `None` when nothing useful results.
    async fn summarize(&self, older: &[Message]) -> Option<String>;
}

/// Contributes the compaction hook. Constructed with the config and — to actually
/// summarize — a [`Summarizer`]; without one the hook is inert.
pub struct CompactPlugin {
    config: CompactConfig,
    summarizer: Option<Arc<dyn Summarizer>>,
}

impl CompactPlugin {
    pub fn new(config: CompactConfig) -> Self {
        Self {
            config,
            summarizer: None,
        }
    }

    #[must_use]
    pub fn with_summarizer(mut self, summarizer: Arc<dyn Summarizer>) -> Self {
        self.summarizer = Some(summarizer);
        self
    }

    fn contribute(&self, config: CompactConfig) -> Contributions {
        let mut contributions = Contributions::new(COMPACT_PLUGIN_ID);
        contributions.phase_hooks.push(Arc::new(CompactHook {
            config,
            summarizer: self.summarizer.clone(),
            cache: Mutex::new(HashMap::new()),
        }));
        contributions
    }
}

impl Plugin for CompactPlugin {
    fn manifest(&self) -> PluginManifest {
        PluginManifest {
            id: COMPACT_PLUGIN_ID.into(),
            requires: Vec::new(),
            config_sections: vec![COMPACT_PLUGIN_ID.into()],
            bound: CapabilityBound {
                phase_hooks: vec![PhaseHookPoint::BeforeInference],
                ..Default::default()
            },
        }
    }

    fn resolve(&self) -> Contributions {
        self.contribute(self.config.clone())
    }

    fn resolve_configured(
        &self,
        config: Option<&serde_json::Value>,
    ) -> Result<Contributions, PluginConfigError> {
        let config = match config {
            Some(value) => serde_json::from_value::<CompactConfig>(value.clone())
                .map_err(|e| PluginConfigError::new(COMPACT_PLUGIN_ID, e.to_string()))?,
            None => self.config.clone(),
        };
        Ok(self.contribute(config))
    }
}

/// The JSON Schema for the `compact` config section.
pub fn config_schema() -> serde_json::Value {
    crate::config::config_schema()
}

struct CompactHook {
    config: CompactConfig,
    summarizer: Option<Arc<dyn Summarizer>>,
    cache: Mutex<HashMap<RunId, Vec<Message>>>,
}

impl CompactHook {
    async fn compute(&self, conversation: &[Message]) -> Vec<Message> {
        let Some(summarizer) = &self.summarizer else {
            return Vec::new();
        };
        let Some(fold_to) = fold_point(
            conversation.len(),
            self.config.threshold,
            self.config.keep_last,
        ) else {
            return Vec::new();
        };
        match summarizer.summarize(&conversation[..fold_to]).await {
            Some(summary) if !summary.trim().is_empty() => vec![Message::text(
                MessageId("compact-summary".into()),
                Role::System,
                format!("Summary of earlier conversation: {summary}"),
            )],
            _ => Vec::new(),
        }
    }
}

#[async_trait]
impl PhaseHook for CompactHook {
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

    fn convo(n: usize) -> Vec<Message> {
        (0..n)
            .map(|i| Message::text(MessageId(format!("m{i}")), Role::User, format!("turn {i}")))
            .collect()
    }

    fn phase_ctx() -> PhaseContext {
        PhaseContext {
            run_id: RunId("r".into()),
            step: 0,
            point: PhaseHookPoint::BeforeInference,
        }
    }

    struct FixedSummarizer {
        seen_len: std::sync::Mutex<usize>,
    }
    #[async_trait]
    impl Summarizer for FixedSummarizer {
        async fn summarize(&self, older: &[Message]) -> Option<String> {
            *self.seen_len.lock().unwrap() = older.len();
            Some("earlier: X".to_string())
        }
    }

    #[test]
    fn manifest_declares_the_hook_and_config_section() {
        let plugin = CompactPlugin::new(CompactConfig::default());
        let m = plugin.manifest();
        assert_eq!(m.bound.phase_hooks, vec![PhaseHookPoint::BeforeInference]);
        assert_eq!(m.config_sections, vec![COMPACT_PLUGIN_ID.to_string()]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn short_conversation_injects_nothing() {
        let plugin = CompactPlugin::new(CompactConfig {
            threshold: 40,
            keep_last: 8,
        })
        .with_summarizer(Arc::new(FixedSummarizer {
            seen_len: std::sync::Mutex::new(0),
        }));
        let hook = &plugin.resolve().phase_hooks[0];
        let reaction = hook.on_phase(&phase_ctx(), &convo(5)).await;
        assert!(reaction.context.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn long_conversation_summarizes_the_older_slice() {
        let summarizer = Arc::new(FixedSummarizer {
            seen_len: std::sync::Mutex::new(0),
        });
        let plugin = CompactPlugin::new(CompactConfig {
            threshold: 4,
            keep_last: 2,
        })
        .with_summarizer(summarizer.clone());
        let hook = &plugin.resolve().phase_hooks[0];
        // 10 messages, keep_last 2 → summarize the first 8.
        let reaction = hook.on_phase(&phase_ctx(), &convo(10)).await;
        assert_eq!(*summarizer.seen_len.lock().unwrap(), 8);
        assert_eq!(reaction.context.len(), 1);
        assert!(
            reaction.context[0]
                .text_content()
                .contains("Summary of earlier conversation: earlier: X")
        );
    }

    #[test]
    fn resolve_configured_fails_closed_on_bad_config() {
        let plugin = CompactPlugin::new(CompactConfig::default());
        let bad = serde_json::json!({ "threshold": "many" });
        assert!(plugin.resolve_configured(Some(&bad)).is_err());
        assert!(plugin.resolve_configured(None).is_ok());
    }
}
