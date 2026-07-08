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
use awaken_agent_contract::agent::state::{Action, Command, MergePolicy, Scope};
use awaken_runtime_contract::plugin::{
    CapabilityBound, Contributions, PhaseContext, PhaseHook, PhaseHookPoint, PhaseReaction, Plugin,
    PluginConfigError, PluginManifest,
};

use crate::config::CompactConfig;
use crate::fold::fold_point;

/// The plugin id under which compaction is activated (G30).
pub const COMPACT_PLUGIN_ID: &str = "compact";

/// Thread-state key prefix under which a completed fold records its fact
/// (`compaction/<run_id>`). Neutral runtime vocabulary — the wire event
/// `agent.thread_context_compacted` is projected from this by a protocol adapter,
/// never named here (G16). Keyed by `run_id` so each turn's fold is a distinct,
/// idempotent fact the host reads back exactly once.
const COMPACTION_KEY_PREFIX: &str = "compaction/";

fn compaction_key(run_id: &str) -> String {
    format!("{COMPACTION_KEY_PREFIX}{run_id}")
}

/// The neutral fact a completed fold commits to thread state.
#[derive(serde::Serialize, serde::Deserialize)]
struct CompactionFact {
    /// Best-effort token estimate of the older slice that was folded away.
    pre_compaction_tokens: u64,
}

/// The pre-compaction token count `run_id` committed, or `None` if that run did
/// not fold. The single read-back seam for a protocol adapter that projects the
/// compaction event: it owns the key + payload shape so no consumer duplicates them.
pub fn compaction_pre_tokens(state: &[Command], run_id: &str) -> Option<u64> {
    let key = compaction_key(run_id);
    state.iter().rev().find_map(|cmd| match &cmd.action {
        Action::Set(value) if cmd.scope == Scope::Thread && cmd.key.0 == key => {
            serde_json::from_value::<CompactionFact>(value.clone())
                .ok()
                .map(|f| f.pre_compaction_tokens)
        }
        _ => None,
    })
}

/// A rough, deterministic token estimate (~4 chars/token) over a message slice —
/// enough to populate `pre_compaction_tokens` without a real tokenizer.
fn estimate_tokens(messages: &[Message]) -> u64 {
    let chars: usize = messages.iter().map(|m| m.text_content().len()).sum();
    (chars / 4) as u64
}

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

/// A completed fold: the request-only summary block plus the token estimate of
/// the slice it replaced.
struct Folded {
    summary: Vec<Message>,
    pre_compaction_tokens: u64,
}

impl CompactHook {
    async fn compute(&self, conversation: &[Message]) -> Option<Folded> {
        let summarizer = self.summarizer.as_ref()?;
        let fold_to = fold_point(
            conversation.len(),
            self.config.threshold,
            self.config.keep_last,
        )?;
        let older = &conversation[..fold_to];
        let summary = summarizer.summarize(older).await?;
        if summary.trim().is_empty() {
            return None;
        }
        Some(Folded {
            summary: vec![Message::text(
                MessageId("compact-summary".into()),
                Role::System,
                format!("Summary of earlier conversation: {summary}"),
            )],
            pre_compaction_tokens: estimate_tokens(older),
        })
    }
}

#[async_trait]
impl PhaseHook for CompactHook {
    fn point(&self) -> PhaseHookPoint {
        PhaseHookPoint::BeforeInference
    }

    async fn on_phase(&self, ctx: &PhaseContext, conversation: &[Message]) -> PhaseReaction {
        if let Some(hit) = self.cache.lock().unwrap().get(&ctx.run_id) {
            // A later step of the same run: replay the summary, but do not re-stage
            // the fact — it was committed on the folding step (emit-once per run).
            return PhaseReaction::context(hit.clone());
        }
        let folded = self.compute(conversation).await;
        let block = folded
            .as_ref()
            .map(|f| f.summary.clone())
            .unwrap_or_default();
        self.cache.lock().unwrap().insert(ctx.run_id.clone(), block);
        match folded {
            // A fold happened: inject the summary request-only *and* stage the
            // durable, protocol-neutral fact the adapter projects into the event.
            Some(f) => PhaseReaction {
                state: vec![Command::set(
                    Scope::Thread,
                    MergePolicy::Commutative,
                    compaction_key(&ctx.run_id.0),
                    serde_json::to_value(CompactionFact {
                        pre_compaction_tokens: f.pre_compaction_tokens,
                    })
                    .expect("compaction fact serializes"),
                )],
                context: f.summary,
            },
            None => PhaseReaction::default(),
        }
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

    #[tokio::test(flavor = "current_thread")]
    async fn a_fold_stages_a_readable_compaction_fact() {
        let plugin = CompactPlugin::new(CompactConfig {
            threshold: 4,
            keep_last: 2,
        })
        .with_summarizer(Arc::new(FixedSummarizer {
            seen_len: std::sync::Mutex::new(0),
        }));
        let hook = &plugin.resolve().phase_hooks[0];
        let reaction = hook.on_phase(&phase_ctx(), &convo(10)).await;
        // The fold stages exactly one thread-scoped compaction command, which the
        // read-back helper resolves to a positive pre-compaction token estimate.
        assert_eq!(reaction.state.len(), 1);
        let tokens = compaction_pre_tokens(&reaction.state, "r");
        assert!(
            tokens.is_some_and(|t| t > 0),
            "pre_compaction_tokens: {tokens:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_short_conversation_stages_no_fact() {
        let plugin = CompactPlugin::new(CompactConfig {
            threshold: 40,
            keep_last: 8,
        })
        .with_summarizer(Arc::new(FixedSummarizer {
            seen_len: std::sync::Mutex::new(0),
        }));
        let hook = &plugin.resolve().phase_hooks[0];
        let reaction = hook.on_phase(&phase_ctx(), &convo(5)).await;
        assert!(reaction.state.is_empty());
        assert_eq!(compaction_pre_tokens(&reaction.state, "r"), None);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_run_stages_the_fact_at_most_once() {
        let plugin = CompactPlugin::new(CompactConfig {
            threshold: 4,
            keep_last: 2,
        })
        .with_summarizer(Arc::new(FixedSummarizer {
            seen_len: std::sync::Mutex::new(0),
        }));
        let hook = &plugin.resolve().phase_hooks[0];
        let first = hook.on_phase(&phase_ctx(), &convo(10)).await;
        assert_eq!(first.state.len(), 1);
        // A later step of the same run replays the summary but stages no new fact.
        let second = hook.on_phase(&phase_ctx(), &convo(10)).await;
        assert!(second.state.is_empty(), "emit-once per run");
        assert_eq!(second.context.len(), 1, "the summary still replays");
    }

    #[test]
    fn resolve_configured_fails_closed_on_bad_config() {
        let plugin = CompactPlugin::new(CompactConfig::default());
        let bad = serde_json::json!({ "threshold": "many" });
        assert!(plugin.resolve_configured(Some(&bad)).is_err());
        assert!(plugin.resolve_configured(None).is_ok());
    }
}
