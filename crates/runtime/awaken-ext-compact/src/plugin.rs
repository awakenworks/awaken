//! Compaction as a plugin hook.
//!
//! [`CompactPlugin`] contributes a `BeforeInference` [`PhaseHook`] symmetric with
//! memory recall: on a long conversation it summarizes the older messages through
//! an injected [`SubagentRunner`] sub-agent and injects the summary as **request-only**
//! context (never committed, G13). The main agent's `ContextPolicy::KeepLast` drops
//! the older raw turns from the model view, so summary + kept tail cover the whole
//! conversation. Summarization runs at most once per run (cached by `run_id`).

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::state::{Action, Command, MergePolicy, Scope, StateKey, Store};
use awaken_runtime_contract::plugin::{
    CapabilityBound, Contributions, HookReaction, IdBound, PhaseContext, PhaseHook, PhaseHookPoint,
    Plugin, PluginConfigError, PluginManifest,
};
use awaken_runtime_contract::subagent_runner::{SubagentRequest, SubagentRunner};

use crate::agent::{COMPACT_AGENT_ID, SUMMARIZE_PROMPT};
use crate::config::CompactConfig;
use crate::fold::{fold_point, token_fold_point};

/// A rough, deterministic token estimate (~4 chars/token) over a message slice.
/// Cheap enough to run every `BeforeInference` without a real tokenizer; it drives
/// the token-aware compaction trigger (`max_tokens` × `trigger_ratio`).
fn estimate_tokens(messages: &[Message]) -> u64 {
    let chars: usize = messages.iter().map(|m| m.text_content().len()).sum();
    (chars / 4) as u64
}

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

/// The state command a completed fold stages: a presence marker under
/// `compaction/<run_id>`. The wire event `agent.thread_context_compacted` carries
/// no payload (aligned to `@anthropic-ai/sdk`), so the marker is a bare `true`.
fn compaction_marker(run_id: &str) -> Command {
    Command::set(
        Scope::Thread,
        MergePolicy::Commutative,
        compaction_key(run_id),
        serde_json::Value::Bool(true),
    )
}

/// The number of distinct folds recorded in `state` — one `compaction/<run_id>`
/// key per fold. A protocol adapter projects the compaction event when this count
/// grows across a turn (compare a turn-start baseline to the terminal-step value).
/// Run-id-agnostic, so it works identically for direct and durable ingress — the
/// durable worker mints its own run id, which a run-id match would miss.
pub fn compaction_count(state: &[Command]) -> usize {
    state
        .iter()
        .filter_map(|cmd| match &cmd.action {
            Action::Set(_)
                if cmd.scope == Scope::Thread && cmd.key.0.starts_with(COMPACTION_KEY_PREFIX) =>
            {
                Some(cmd.key.0.as_str())
            }
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>()
        .len()
}

/// Contributes the compaction hook. Constructed with the config and — to actually
/// summarize — a [`SubagentRunner`] (the neutral aux-run port, ADR-0047 D5, shared
/// with the goal judge); without one the hook is inert.
pub struct CompactPlugin {
    config: CompactConfig,
    runner: Option<Arc<dyn SubagentRunner>>,
}

impl CompactPlugin {
    pub fn new(config: CompactConfig) -> Self {
        Self {
            config,
            runner: None,
        }
    }

    #[must_use]
    pub fn with_runner(mut self, runner: Arc<dyn SubagentRunner>) -> Self {
        self.runner = Some(runner);
        self
    }

    fn contribute(&self, config: CompactConfig) -> Contributions {
        let mut contributions = Contributions::new(COMPACT_PLUGIN_ID);
        contributions.state_keys = vec![CompactionContext::KEY.to_string()];
        contributions.phase_hooks.push(Arc::new(CompactHook {
            config,
            runner: self.runner.clone(),
        }));
        contributions
    }
}

/// The run-scoped cell holding this run's compaction decision: `None` (absent)
/// means compaction has not been evaluated yet; `Some(block)` means it has, where a
/// non-empty block is the summary to re-inject each step and an empty block records
/// "evaluated, did not fold". Gates summarization to at most once per run and
/// replays across steps and a resumed run without re-running the compactor
/// sub-agent (ADR-0055); replaces the former per-`run_id` in-process cache.
struct CompactionContext;
impl StateKey for CompactionContext {
    const KEY: &'static str = "compaction_context";
    const SCOPE: Scope = Scope::Run;
    const MERGE: MergePolicy = MergePolicy::Exclusive;
    type Value = Option<Vec<Message>>;
    type Update = Vec<Message>;
    fn apply(value: &mut Option<Vec<Message>>, update: Vec<Message>) {
        *value = Some(update);
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
                state_keys: IdBound::Exact(vec![CompactionContext::KEY.into()]),
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
    runner: Option<Arc<dyn SubagentRunner>>,
}

impl CompactHook {
    /// The request-only summary block for a fold, or `None` when nothing folds.
    async fn compute(&self, conversation: &[Message]) -> Option<Vec<Message>> {
        let runner = self.runner.as_ref()?;
        // Token-aware when the model's window is known (fold at `trigger_ratio` of
        // it), else the message-count `threshold`.
        let fold_to = match self.config.max_tokens {
            Some(max_tokens) => token_fold_point(
                estimate_tokens(conversation),
                max_tokens,
                self.config.trigger_ratio,
                conversation.len(),
                self.config.keep_last,
            )?,
            None => fold_point(
                conversation.len(),
                self.config.threshold,
                self.config.keep_last,
            )?,
        };
        // Seed the `compactor` sub-agent with the older slice plus the summarize
        // prompt, through the shared aux-run port. A per-agent `instructions` override
        // (config) replaces the built-in prompt; otherwise the default is used.
        let mut seed = conversation[..fold_to].to_vec();
        let prompt = self
            .config
            .instructions
            .as_deref()
            .unwrap_or(SUMMARIZE_PROMPT);
        seed.push(Message::text(
            MessageId("compact-prompt".into()),
            Role::User,
            prompt,
        ));
        let reply = runner
            .run(SubagentRequest {
                agent_id: COMPACT_AGENT_ID.to_string(),
                seed,
                cancellation: None,
            })
            .await
            .ok()?;
        let summary = reply.text?;
        if summary.trim().is_empty() {
            return None;
        }
        Some(vec![Message::text(
            MessageId("compact-summary".into()),
            Role::System,
            format!("Summary of earlier conversation: {summary}"),
        )])
    }
}

#[async_trait]
impl PhaseHook for CompactHook {
    fn point(&self) -> PhaseHookPoint {
        PhaseHookPoint::BeforeInference
    }

    async fn on_phase(
        &self,
        ctx: &PhaseContext,
        conversation: &[Message],
        state: &Store,
    ) -> HookReaction {
        if let Some(block) = CompactionContext::load_or_default(state) {
            // A later step of the same run (or a resumed run replaying committed
            // state): replay the summary, but do not re-fold and do not re-stage
            // the marker — it was committed on the folding step (emit-once per run).
            return HookReaction::messages(block);
        }
        match self.compute(conversation).await {
            // A fold happened: record the summary in run state (so later steps and a
            // resumed run replay it), inject it request-only, *and* stage the
            // durable, protocol-neutral marker the adapter projects into the event.
            Some(block) => HookReaction {
                state: vec![
                    CompactionContext::write(&Some(block.clone())),
                    compaction_marker(&ctx.run_id.0),
                ],
                messages: block,
            },
            // No fold this step: record the "evaluated, did not fold" decision so a
            // later step does not re-evaluate the grown conversation and fold late
            // (at-most-once-per-run, matching the former cache).
            None => HookReaction {
                state: vec![CompactionContext::write(&Some(Vec::new()))],
                messages: Vec::new(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use awaken_agent_contract::agent::run::Id as RunId;

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
            after_tool: None,
        }
    }

    use awaken_runtime_contract::subagent_runner::{SubagentError, SubagentReply};

    /// A stub aux-runner: records the seed length it was handed (the folded slice
    /// plus the appended summarize prompt) and returns a fixed summary.
    struct FixedSummarizer {
        seen_len: std::sync::Mutex<usize>,
    }
    #[async_trait]
    impl SubagentRunner for FixedSummarizer {
        async fn run(&self, request: SubagentRequest) -> Result<SubagentReply, SubagentError> {
            *self.seen_len.lock().unwrap() = request.seed.len();
            Ok(SubagentReply {
                text: Some("earlier: X".to_string()),
            })
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
            ..Default::default()
        })
        .with_runner(Arc::new(FixedSummarizer {
            seen_len: std::sync::Mutex::new(0),
        }));
        let hook = &plugin.resolve().phase_hooks[0];
        let reaction = hook.on_phase(&phase_ctx(), &convo(5), &Store::new()).await;
        assert!(reaction.messages.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn long_conversation_summarizes_the_older_slice() {
        let summarizer = Arc::new(FixedSummarizer {
            seen_len: std::sync::Mutex::new(0),
        });
        let plugin = CompactPlugin::new(CompactConfig {
            threshold: 4,
            keep_last: 2,
            ..Default::default()
        })
        .with_runner(summarizer.clone());
        let hook = &plugin.resolve().phase_hooks[0];
        // 10 messages, keep_last 2 → summarize the first 8.
        let reaction = hook.on_phase(&phase_ctx(), &convo(10), &Store::new()).await;
        assert_eq!(*summarizer.seen_len.lock().unwrap(), 9); // 8 folded + summarize prompt
        assert_eq!(reaction.messages.len(), 1);
        assert!(
            reaction.messages[0]
                .text_content()
                .contains("Summary of earlier conversation: earlier: X")
        );
    }

    /// Records the text of the last seed message — the compaction prompt the hook
    /// appended — so a test can assert which prompt reached the compactor.
    struct PromptRecorder {
        seen_prompt: std::sync::Mutex<String>,
    }
    #[async_trait]
    impl SubagentRunner for PromptRecorder {
        async fn run(&self, request: SubagentRequest) -> Result<SubagentReply, SubagentError> {
            *self.seen_prompt.lock().unwrap() = request
                .seed
                .last()
                .map(|m| m.text_content())
                .unwrap_or_default();
            Ok(SubagentReply {
                text: Some("s".to_string()),
            })
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_custom_instructions_config_overrides_the_compaction_prompt() {
        let recorder = Arc::new(PromptRecorder {
            seen_prompt: std::sync::Mutex::new(String::new()),
        });
        // With no override, the built-in summarize prompt is used.
        let default_plugin = CompactPlugin::new(CompactConfig {
            threshold: 4,
            keep_last: 2,
            ..Default::default()
        })
        .with_runner(recorder.clone());
        default_plugin.resolve().phase_hooks[0]
            .on_phase(&phase_ctx(), &convo(10), &Store::new())
            .await;
        assert_eq!(*recorder.seen_prompt.lock().unwrap(), SUMMARIZE_PROMPT);

        // With an override, the per-agent instructions replace it verbatim.
        let custom = "Keep only the API endpoints mentioned.";
        let tuned_plugin = CompactPlugin::new(CompactConfig {
            threshold: 4,
            keep_last: 2,
            instructions: Some(custom.to_string()),
            ..Default::default()
        })
        .with_runner(recorder.clone());
        tuned_plugin.resolve().phase_hooks[0]
            .on_phase(&phase_ctx(), &convo(10), &Store::new())
            .await;
        assert_eq!(*recorder.seen_prompt.lock().unwrap(), custom);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_fold_stages_a_readable_compaction_marker() {
        let plugin = CompactPlugin::new(CompactConfig {
            threshold: 4,
            keep_last: 2,
            ..Default::default()
        })
        .with_runner(Arc::new(FixedSummarizer {
            seen_len: std::sync::Mutex::new(0),
        }));
        let hook = &plugin.resolve().phase_hooks[0];
        let reaction = hook.on_phase(&phase_ctx(), &convo(10), &Store::new()).await;
        // The fold stages exactly one thread-scoped compaction marker, which the
        // read-back helper resolves to `true`.
        assert_eq!(reaction.state.len(), 2);
        assert_eq!(compaction_count(&reaction.state), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_short_conversation_stages_no_marker() {
        let plugin = CompactPlugin::new(CompactConfig {
            threshold: 40,
            keep_last: 8,
            ..Default::default()
        })
        .with_runner(Arc::new(FixedSummarizer {
            seen_len: std::sync::Mutex::new(0),
        }));
        let hook = &plugin.resolve().phase_hooks[0];
        let reaction = hook.on_phase(&phase_ctx(), &convo(5), &Store::new()).await;
        assert_eq!(reaction.state.len(), 1);
        assert_eq!(compaction_count(&reaction.state), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_run_stages_the_fact_at_most_once() {
        let plugin = CompactPlugin::new(CompactConfig {
            threshold: 4,
            keep_last: 2,
            ..Default::default()
        })
        .with_runner(Arc::new(FixedSummarizer {
            seen_len: std::sync::Mutex::new(0),
        }));
        let hook = &plugin.resolve().phase_hooks[0];
        let mut state = Store::new();
        let first = hook.on_phase(&phase_ctx(), &convo(10), &state).await;
        assert_eq!(first.state.len(), 2, "summary block + compaction marker");
        assert_eq!(compaction_count(&first.state), 1);
        // A later step of the same run replays the summary but stages no new fact,
        // gated on the run-scoped compaction state applied here (ADR-0055).
        for command in &first.state {
            state.apply(command);
        }
        let second = hook.on_phase(&phase_ctx(), &convo(10), &state).await;
        assert!(second.state.is_empty(), "emit-once per run");
        assert_eq!(second.messages.len(), 1, "the summary still replays");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn token_budget_triggers_the_fold_despite_a_huge_message_threshold() {
        let plugin = CompactPlugin::new(CompactConfig {
            threshold: 9999, // message mode would never fire
            keep_last: 2,
            max_tokens: Some(10), // budget = 0.8 * 10 = 8 tokens
            trigger_ratio: 0.8,
            instructions: None,
        })
        .with_runner(Arc::new(FixedSummarizer {
            seen_len: std::sync::Mutex::new(0),
        }));
        let hook = &plugin.resolve().phase_hooks[0];
        // 10 short messages (~15 est. tokens) exceed the 8-token budget.
        let reaction = hook.on_phase(&phase_ctx(), &convo(10), &Store::new()).await;
        assert_eq!(
            reaction.messages.len(),
            1,
            "the token budget folded the older slice"
        );
        assert_eq!(compaction_count(&reaction.state), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_wide_token_window_does_not_fold_a_small_conversation() {
        let plugin = CompactPlugin::new(CompactConfig {
            threshold: 9999,
            keep_last: 2,
            max_tokens: Some(1_000_000), // budget far beyond a tiny conversation
            trigger_ratio: 0.8,
            instructions: None,
        })
        .with_runner(Arc::new(FixedSummarizer {
            seen_len: std::sync::Mutex::new(0),
        }));
        let hook = &plugin.resolve().phase_hooks[0];
        let reaction = hook.on_phase(&phase_ctx(), &convo(10), &Store::new()).await;
        assert!(reaction.messages.is_empty());
        assert_eq!(compaction_count(&reaction.state), 0);
    }

    #[test]
    fn resolve_configured_fails_closed_on_bad_config() {
        let plugin = CompactPlugin::new(CompactConfig::default());
        let bad = serde_json::json!({ "threshold": "many" });
        assert!(plugin.resolve_configured(Some(&bad)).is_err());
        assert!(plugin.resolve_configured(None).is_ok());
    }
}
