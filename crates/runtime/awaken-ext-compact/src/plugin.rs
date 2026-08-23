//! Compaction as a plugin hook.
//!
//! [`CompactPlugin`] contributes a `BeforeInference` [`PhaseHook`] symmetric with
//! memory recall: on a long conversation it summarizes the older messages through
//! an injected ordinary Agent-backed tool and injects the summary as **request-only**
//! context (never committed, G13). A successful fold activates the Run-scoped
//! [`ContextWindow`], so summary + optional bridge + kept tail cover the whole
//! conversation; before that, the kernel keeps all history. Summarization runs at
//! most once per run, gated on the run-scoped
//! [`ContextMessages`] state so it replays across steps and a resumed run
//! instead of recomputing (ADR-0055).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::state::{StateKey, Store};
use awaken_runtime_contract::compaction::RunCompactionMarker;
use awaken_runtime_contract::content_fingerprint;
use awaken_runtime_contract::plugin::{
    CapabilityBound, ContextMessages, ContextWindow, Contributions, HookReaction, IdBound,
    PhaseContext, PhaseHook, PhaseHookPoint, Plugin, PluginConfigError, PluginManifest,
};
use awaken_runtime_contract::tool::{RawTool, ToolCall, invoke_raw_tool};

use crate::agent::SUMMARIZE_PROMPT;
use crate::backend::{CompactArtifact, CompactBackend, CompactRequest};
use crate::config::CompactConfig;
use crate::fold::{fold_point, prefetch_fold_point, token_fold_point};

/// A rough, deterministic token estimate (~4 chars/token) over a message slice.
/// Cheap enough to run every `BeforeInference` without a real tokenizer; it drives
/// the token-aware compaction trigger (`max_tokens` × `trigger_ratio`).
fn estimate_tokens(messages: &[Message]) -> u64 {
    let chars: usize = messages.iter().map(|m| m.text_content().len()).sum();
    (chars / 4) as u64
}

/// The plugin id under which compaction is activated (G30).
pub const COMPACT_PLUGIN_ID: &str = "compact";

/// Contributes the compaction hook. Constructed with the config and — to actually
/// summarize — an ordinary Agent-backed [`RawTool`]; without one the hook is inert.
pub struct CompactPlugin {
    config: CompactConfig,
    agent_tool: Option<Arc<dyn RawTool>>,
    backend: Option<(String, Arc<dyn CompactBackend>)>,
}

impl CompactPlugin {
    pub fn new(config: CompactConfig) -> Self {
        Self {
            config,
            agent_tool: None,
            backend: None,
        }
    }

    #[must_use]
    pub fn with_agent_tool(mut self, tool: Arc<dyn RawTool>) -> Self {
        self.agent_tool = Some(tool);
        self
    }

    /// Use the host's asynchronous compaction backend in this parent-Thread
    /// namespace. It supersedes the synchronous compatibility tool.
    #[must_use]
    pub fn with_backend(
        mut self,
        scope: impl Into<String>,
        backend: Arc<dyn CompactBackend>,
    ) -> Self {
        self.backend = Some((scope.into(), backend));
        self
    }

    fn contribute(&self, config: CompactConfig) -> Contributions {
        let mut contributions = Contributions::new(COMPACT_PLUGIN_ID);
        contributions.declare_state_key(ContextMessages::KEY);
        contributions.declare_state_key(ContextWindow::KEY);
        contributions.register_hook(Arc::new(CompactHook {
            config,
            agent_tool: self.agent_tool.clone(),
            backend: self.backend.clone(),
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
                state_keys: IdBound::Exact(vec![
                    ContextMessages::KEY.into(),
                    ContextWindow::KEY.into(),
                ]),
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
    agent_tool: Option<Arc<dyn RawTool>>,
    backend: Option<(String, Arc<dyn CompactBackend>)>,
}

impl CompactHook {
    fn fold_to(&self, conversation: &[Message]) -> Option<usize> {
        // Token-aware when the model's window is known (fold at `trigger_ratio` of
        // it), else the message-count `threshold`.
        match self.config.max_tokens {
            Some(max_tokens) => token_fold_point(
                estimate_tokens(conversation),
                max_tokens,
                self.config.trigger_ratio,
                conversation.len(),
                self.config.keep_last,
            ),
            None => fold_point(
                conversation.len(),
                self.config.threshold,
                self.config.keep_last,
            ),
        }
    }

    fn seed(&self, conversation: &[Message], fold_to: usize) -> Vec<Message> {
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
        seed
    }

    fn request(&self, scope: &str, conversation: &[Message], fold_to: usize) -> CompactRequest {
        let seed = self.seed(conversation, fold_to);
        let key = format!(
            "compact-{}",
            content_fingerprint(&(scope, &self.config.agent_id, fold_to, &seed))
                .unwrap_or_default()
        );
        CompactRequest {
            agent_id: self.config.agent_id.clone(),
            scope: scope.to_string(),
            key,
            covered_messages: fold_to,
            seed,
        }
    }

    fn block(
        artifact: CompactArtifact,
        conversation: &[Message],
        fold_to: usize,
    ) -> Option<Vec<Message>> {
        if artifact.summary.trim().is_empty() || artifact.covered_messages > fold_to {
            return None;
        }
        let mut block = vec![Message::text(
            MessageId("compact-summary".into()),
            Role::System,
            format!("Summary of earlier conversation: {}", artifact.summary),
        )];
        // A soft-prefetched artifact may cover an earlier prefix. Preserve the
        // unsummarized bridge verbatim before the main windowed tail.
        block.extend_from_slice(&conversation[artifact.covered_messages..fold_to]);
        Some(block)
    }

    async fn compute(&self, parent_run_id: &str, conversation: &[Message]) -> Option<Vec<Message>> {
        let estimated_tokens = estimate_tokens(conversation);
        let Some(fold_to) = self.fold_to(conversation) else {
            if let Some((scope, backend)) = &self.backend
                && let Some(prefetch_to) = prefetch_fold_point(
                    conversation.len(),
                    estimated_tokens,
                    self.config.threshold,
                    self.config.max_tokens,
                    self.config.trigger_ratio,
                    self.config.prefetch_ratio,
                    self.config.keep_last,
                )
            {
                backend
                    .prefetch(self.request(scope, conversation, prefetch_to))
                    .await;
            }
            return None;
        };

        if let Some((scope, backend)) = &self.backend {
            if let Some(ready) = backend.latest_ready(scope, fold_to).await {
                return Self::block(ready, conversation, fold_to);
            }
            let request = self.request(scope, conversation, fold_to);
            return Self::block(backend.summarize(request).await?, conversation, fold_to);
        }

        let agent_tool = self.agent_tool.as_ref()?;
        let seed = self.seed(conversation, fold_to);
        let reply = invoke_raw_tool(
            agent_tool.as_ref(),
            ToolCall {
                call_id: format!("compact/{parent_run_id}"),
                tool_id: agent_tool.id().to_string(),
                arguments: serde_json::json!({
                    "agent_id": self.config.agent_id,
                    "seed": seed,
                }),
            },
            None,
        )
        .await
        .ok()?;
        if reply.is_error {
            return None;
        }
        let summary = reply.text();
        if summary.trim().is_empty() {
            return None;
        }
        Self::block(
            CompactArtifact {
                scope: String::new(),
                key: parent_run_id.to_string(),
                covered_messages: fold_to,
                summary,
            },
            conversation,
            fold_to,
        )
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
        // Already evaluated this run (a later step or a resumed run replaying
        // committed state): the kernel injects any summary from `ContextMessages`,
        // so do not re-fold and do not re-stage the marker (emit-once per run).
        if ContextMessages::load_or_default(state).contains_key(COMPACT_PLUGIN_ID) {
            return HookReaction::default();
        }
        match self.compute(&ctx.run_id.0, conversation).await {
            // A fold happened: record the summary under this producer in
            // `ContextMessages` (the kernel injects it request-only across steps and
            // a resumed run) *and* stage the durable, protocol-neutral marker the
            // adapter projects into the event.
            Some(block) => HookReaction::state(vec![
                ContextMessages::write(&BTreeMap::from([(COMPACT_PLUGIN_ID.to_string(), block)])),
                ContextWindow::write(&Some(self.config.keep_last)),
                RunCompactionMarker::command(&ctx.run_id.0),
            ]),
            // No fold this step: record the "evaluated, did not fold" decision (an
            // empty entry) so a later step does not re-evaluate the grown
            // conversation and fold late (at-most-once-per-run, matching the cache).
            None => HookReaction::state(vec![ContextMessages::write(&BTreeMap::from([(
                COMPACT_PLUGIN_ID.to_string(),
                Vec::new(),
            )]))]),
        }
    }
}

#[cfg(test)]
mod tests {
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_runtime_contract::plugin::PhaseKind;

    use super::*;

    fn convo(n: usize) -> Vec<Message> {
        (0..n)
            .map(|i| {
                Message::text(
                    MessageId(format!("m{i}")),
                    Role::User,
                    format!("message {i}"),
                )
            })
            .collect()
    }

    /// The request-only block a reaction wrote for this plugin, read back from the
    /// `ContextMessages` state the kernel injects (ADR-0055).
    fn injected(reaction: &HookReaction) -> Vec<Message> {
        let mut store = Store::new();
        for command in &reaction.state {
            store.apply(command);
        }
        ContextMessages::load_or_default(&store)
            .remove(COMPACT_PLUGIN_ID)
            .unwrap_or_default()
    }

    fn phase_ctx() -> PhaseContext {
        PhaseContext {
            run_id: RunId("r".into()),
            step: 0,
            kind: PhaseKind::BeforeInference {
                run_input: Default::default(),
            },
        }
    }

    use awaken_runtime_contract::tool::{ToolCall, ToolError, ToolOutput};

    fn agent_seed(call: &ToolCall) -> Vec<Message> {
        serde_json::from_value(call.arguments["seed"].clone()).expect("agent tool seed")
    }

    fn agent_seed_len(call: &ToolCall) -> usize {
        agent_seed(call).len()
    }

    /// A stub aux-runner: records the seed length it was handed (the folded slice
    /// plus the appended summarize prompt) and returns a fixed summary.
    struct FixedSummarizer {
        seen_len: std::sync::Mutex<usize>,
    }
    #[async_trait]
    impl RawTool for FixedSummarizer {
        fn id(&self) -> &str {
            "test_agent"
        }
        async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
            *self.seen_len.lock().unwrap() = agent_seed_len(&call);
            Ok(ToolOutput::ok(call.call_id, "earlier: X"))
        }
    }

    struct FakeBackend {
        prefetched: std::sync::Mutex<Vec<CompactRequest>>,
        ready: std::sync::Mutex<Option<CompactArtifact>>,
        summarized: std::sync::Mutex<Vec<CompactRequest>>,
    }

    #[async_trait]
    impl CompactBackend for FakeBackend {
        async fn prefetch(&self, request: CompactRequest) {
            self.prefetched.lock().unwrap().push(request);
        }

        async fn latest_ready(
            &self,
            scope: &str,
            at_most_messages: usize,
        ) -> Option<CompactArtifact> {
            self.ready.lock().unwrap().clone().filter(|artifact| {
                artifact.scope == scope && artifact.covered_messages <= at_most_messages
            })
        }

        async fn summarize(&self, request: CompactRequest) -> Option<CompactArtifact> {
            self.summarized.lock().unwrap().push(request.clone());
            Some(CompactArtifact {
                scope: request.scope,
                key: request.key,
                covered_messages: request.covered_messages,
                summary: "hard summary".into(),
            })
        }
    }

    fn fake_backend(ready: Option<CompactArtifact>) -> Arc<FakeBackend> {
        Arc::new(FakeBackend {
            prefetched: std::sync::Mutex::new(Vec::new()),
            ready: std::sync::Mutex::new(ready),
            summarized: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn msg(text: &str) -> Message {
        Message::text(MessageId("m".into()), Role::User, text)
    }

    #[test]
    fn estimate_tokens_is_content_length_over_four_floored() {
        // ~4 chars/token via integer floor division of summed text length.
        assert_eq!(estimate_tokens(&[]), 0, "no messages → 0 tokens");
        assert_eq!(estimate_tokens(&[msg("")]), 0);
        // Boundaries around a single 4-char token.
        assert_eq!(estimate_tokens(&[msg("abc")]), 0, "3/4 floors to 0");
        assert_eq!(estimate_tokens(&[msg("abcd")]), 1, "4/4 == 1");
        assert_eq!(estimate_tokens(&[msg("abcde")]), 1, "5/4 floors to 1");
        assert_eq!(estimate_tokens(&[msg("abcdefg")]), 1, "7/4 floors to 1");
        assert_eq!(estimate_tokens(&[msg("abcdefgh")]), 2, "8/4 == 2");
        // The estimate sums text across messages before dividing (6+6 = 12 → 3),
        // not per-message flooring (which would give 1+1 = 2).
        assert_eq!(estimate_tokens(&[msg("abcdef"), msg("ghijkl")]), 3);
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
        .with_agent_tool(Arc::new(FixedSummarizer {
            seen_len: std::sync::Mutex::new(0),
        }));
        let hook = &plugin.resolve().phase_hooks[0];
        let reaction = hook.on_phase(&phase_ctx(), &convo(5), &Store::new()).await;
        assert!(injected(&reaction).is_empty());
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
        .with_agent_tool(summarizer.clone());
        let hook = &plugin.resolve().phase_hooks[0];
        // 10 messages, keep_last 2 → summarize the first 8.
        let reaction = hook.on_phase(&phase_ctx(), &convo(10), &Store::new()).await;
        assert_eq!(*summarizer.seen_len.lock().unwrap(), 9); // 8 folded + summarize prompt
        let block = injected(&reaction);
        assert_eq!(block.len(), 1);
        assert!(
            block[0]
                .text_content()
                .contains("Summary of earlier conversation: earlier: X")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn soft_threshold_prefetches_without_blocking_or_marking_a_fold() {
        // Test design — Causes: conversation crosses only the soft prefetch
        // threshold. Effects: one prefix is prefetched with no injected summary,
        // synchronous summarize call, or marker. Constraints/invariants: prefetch
        // is non-blocking and not a fold authority. Decision rule P1: soft-only=>
        // one prefetch request and zero committed fold effects.
        let backend = fake_backend(None);
        let plugin = CompactPlugin::new(CompactConfig {
            threshold: 10,
            keep_last: 2,
            prefetch_ratio: 0.5,
            ..Default::default()
        })
        .with_backend("thread-a", backend.clone());
        let reaction = plugin.resolve().phase_hooks[0]
            .on_phase(&phase_ctx(), &convo(6), &Store::new())
            .await;

        assert!(injected(&reaction).is_empty());
        assert!(!RunCompactionMarker::is_recorded(&reaction.state, "r"));
        assert_eq!(backend.summarized.lock().unwrap().len(), 0);
        let prefetched = backend.prefetched.lock().unwrap();
        assert_eq!(prefetched.len(), 1);
        assert_eq!(prefetched[0].covered_messages, 4);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hard_fold_reuses_ready_prefix_and_bridges_every_uncovered_message() {
        // Test design — Causes: the hard threshold is crossed with a ready cached
        // summary covering five messages. Effects: cache is reused and every
        // uncovered pre-tail message is bridged before the marker. Constraints/
        // invariants: no second summary is generated and message order is exact.
        // Decision rule H1: ready prefix=>summary+uncovered bridge+one marker.
        let backend = fake_backend(Some(CompactArtifact {
            scope: "thread-a".into(),
            key: "soft".into(),
            covered_messages: 5,
            summary: "soft summary".into(),
        }));
        let plugin = CompactPlugin::new(CompactConfig {
            threshold: 4,
            keep_last: 2,
            ..Default::default()
        })
        .with_backend("thread-a", backend.clone());
        let reaction = plugin.resolve().phase_hooks[0]
            .on_phase(&phase_ctx(), &convo(10), &Store::new())
            .await;

        let block = injected(&reaction);
        assert_eq!(block.len(), 4, "summary + messages 5..8 bridge");
        assert!(block[0].text_content().contains("soft summary"));
        assert_eq!(
            block[1..]
                .iter()
                .map(Message::text_content)
                .collect::<Vec<_>>(),
            ["message 5", "message 6", "message 7"]
        );
        assert_eq!(backend.summarized.lock().unwrap().len(), 0);
        assert!(RunCompactionMarker::is_recorded(&reaction.state, "r"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hard_cache_miss_awaits_the_exact_stable_request() {
        // Test design — Causes: hard threshold is crossed and no prefix is cached.
        // Effects: the exact eight-message prefix is summarized, injected, and
        // marked. Constraints/invariants: one stable request owns the fold and
        // the retained tail is excluded. Decision rule H2: hard+miss=>one await,
        // one summary block, one run marker.
        let backend = fake_backend(None);
        let plugin = CompactPlugin::new(CompactConfig {
            threshold: 4,
            keep_last: 2,
            ..Default::default()
        })
        .with_backend("thread-a", backend.clone());
        let reaction = plugin.resolve().phase_hooks[0]
            .on_phase(&phase_ctx(), &convo(10), &Store::new())
            .await;

        assert_eq!(injected(&reaction).len(), 1);
        let summarized = backend.summarized.lock().unwrap();
        assert_eq!(summarized.len(), 1);
        assert_eq!(summarized[0].covered_messages, 8);
        assert!(RunCompactionMarker::is_recorded(&reaction.state, "r"));
    }

    /// Records the text of the last seed message — the compaction prompt the hook
    /// appended — so a test can assert which prompt reached the compactor.
    struct PromptRecorder {
        seen_prompt: std::sync::Mutex<String>,
    }
    #[async_trait]
    impl RawTool for PromptRecorder {
        fn id(&self) -> &str {
            "test_agent"
        }
        async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
            *self.seen_prompt.lock().unwrap() = agent_seed(&call)
                .last()
                .map(|m| m.text_content())
                .unwrap_or_default();
            Ok(ToolOutput::ok(call.call_id, "s"))
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
        .with_agent_tool(recorder.clone());
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
        .with_agent_tool(recorder.clone());
        tuned_plugin.resolve().phase_hooks[0]
            .on_phase(&phase_ctx(), &convo(10), &Store::new())
            .await;
        assert_eq!(*recorder.seen_prompt.lock().unwrap(), custom);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_fold_stages_a_readable_compaction_marker() {
        // Test design — Causes: an eligible long conversation produces a summary.
        // Effects: summary, context window, and one readable Run marker are staged.
        // Constraints/invariants: the marker shares the same staged state as the
        // fold and is readable by the canonical helper. Decision rule M1:
        // successful fold=>three commands, marker=true, retained window=2.
        let plugin = CompactPlugin::new(CompactConfig {
            threshold: 4,
            keep_last: 2,
            ..Default::default()
        })
        .with_agent_tool(Arc::new(FixedSummarizer {
            seen_len: std::sync::Mutex::new(0),
        }));
        let hook = &plugin.resolve().phase_hooks[0];
        let reaction = hook.on_phase(&phase_ctx(), &convo(10), &Store::new()).await;
        // The fold stages exactly one thread-scoped compaction marker, which the
        // read-back helper resolves to `true`.
        assert_eq!(reaction.state.len(), 3);
        assert!(RunCompactionMarker::is_recorded(&reaction.state, "r"));
        let mut state = Store::new();
        for command in &reaction.state {
            state.apply(command);
        }
        assert_eq!(ContextWindow::load(&state).unwrap(), Some(2));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_short_conversation_stages_no_marker() {
        // Test design — Causes: conversation remains below the hard threshold.
        // Effects: only the no-fold evaluation is staged and no marker appears.
        // Constraints/invariants: evaluation alone cannot claim compaction.
        // Decision rule M2: short input=>zero summary/marker and one no-fold row.
        let plugin = CompactPlugin::new(CompactConfig {
            threshold: 40,
            keep_last: 8,
            ..Default::default()
        })
        .with_agent_tool(Arc::new(FixedSummarizer {
            seen_len: std::sync::Mutex::new(0),
        }));
        let hook = &plugin.resolve().phase_hooks[0];
        let reaction = hook.on_phase(&phase_ctx(), &convo(5), &Store::new()).await;
        assert_eq!(reaction.state.len(), 1);
        assert!(!RunCompactionMarker::is_recorded(&reaction.state, "r"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_run_stages_the_fact_at_most_once() {
        // Test design — Causes: the same Run evaluates an eligible conversation
        // twice after applying its first staged state. Effects: the first call
        // folds; replay stages nothing. Constraints/invariants: compaction is
        // emit-once per Run. Decision rule I1: absent marker=>fold once;
        // recorded marker=>no messages and no state.
        let plugin = CompactPlugin::new(CompactConfig {
            threshold: 4,
            keep_last: 2,
            ..Default::default()
        })
        .with_agent_tool(Arc::new(FixedSummarizer {
            seen_len: std::sync::Mutex::new(0),
        }));
        let hook = &plugin.resolve().phase_hooks[0];
        let mut state = Store::new();
        let first = hook.on_phase(&phase_ctx(), &convo(10), &state).await;
        assert_eq!(
            first.state.len(),
            3,
            "summary block + context window + compaction marker"
        );
        assert!(RunCompactionMarker::is_recorded(&first.state, "r"));
        // A later step of the same run replays the summary but stages no new fact,
        // gated on the run-scoped compaction state applied here (ADR-0055).
        for command in &first.state {
            state.apply(command);
        }
        // The summary the first step staged is the block the kernel replays.
        assert_eq!(injected(&first).len(), 1, "the summary is in run state");
        let second = hook.on_phase(&phase_ctx(), &convo(10), &state).await;
        assert!(second.state.is_empty(), "emit-once per run");
        assert!(second.messages.is_empty(), "a replay stages nothing new");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn token_budget_triggers_the_fold_despite_a_huge_message_threshold() {
        // Test design — Causes: estimated tokens exceed the configured budget
        // while message count cannot trigger. Effects: the older slice folds and
        // records a marker. Constraints/invariants: token and count triggers feed
        // the same fold path. Decision rule T1: token-over+count-under=>one fold.
        let plugin = CompactPlugin::new(CompactConfig {
            agent_id: crate::COMPACT_AGENT_ID.to_string(),
            agent_instructions: None,
            threshold: 9999, // message mode would never fire
            keep_last: 2,
            max_tokens: Some(10), // budget = 0.8 * 10 = 8 tokens
            trigger_ratio: 0.8,
            prefetch_ratio: 0.75,
            instructions: None,
        })
        .with_agent_tool(Arc::new(FixedSummarizer {
            seen_len: std::sync::Mutex::new(0),
        }));
        let hook = &plugin.resolve().phase_hooks[0];
        // 10 short messages (~15 est. tokens) exceed the 8-token budget.
        let reaction = hook.on_phase(&phase_ctx(), &convo(10), &Store::new()).await;
        assert_eq!(
            injected(&reaction).len(),
            1,
            "the token budget folded the older slice"
        );
        assert!(RunCompactionMarker::is_recorded(&reaction.state, "r"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_wide_token_window_does_not_fold_a_small_conversation() {
        // Test design — Causes: both token usage and message count remain below
        // their thresholds. Effects: no summary and no marker are staged.
        // Constraints/invariants: configured capacity is not treated as usage.
        // Decision rule T2: token-under+count-under=>no fold effects.
        let plugin = CompactPlugin::new(CompactConfig {
            agent_id: crate::COMPACT_AGENT_ID.to_string(),
            agent_instructions: None,
            threshold: 9999,
            keep_last: 2,
            max_tokens: Some(1_000_000), // budget far beyond a tiny conversation
            trigger_ratio: 0.8,
            prefetch_ratio: 0.75,
            instructions: None,
        })
        .with_agent_tool(Arc::new(FixedSummarizer {
            seen_len: std::sync::Mutex::new(0),
        }));
        let hook = &plugin.resolve().phase_hooks[0];
        let reaction = hook.on_phase(&phase_ctx(), &convo(10), &Store::new()).await;
        assert!(injected(&reaction).is_empty());
        assert!(!RunCompactionMarker::is_recorded(&reaction.state, "r"));
    }

    #[test]
    fn resolve_configured_fails_closed_on_bad_config() {
        let plugin = CompactPlugin::new(CompactConfig::default());
        let bad = serde_json::json!({ "threshold": "many" });
        assert!(plugin.resolve_configured(Some(&bad)).is_err());
        assert!(plugin.resolve_configured(None).is_ok());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn without_a_runner_the_hook_is_inert() {
        // Test design — Causes: a long conversation reaches a hook with no
        // summarizer runner. Effects: it records only a no-fold evaluation.
        // Constraints/invariants: absence cannot invoke a fallback runner or mark
        // a fold. Decision rule A1: eligible+no runner=>no summary/marker.
        // No Agent-backed tool wired: even a long conversation folds nothing. The hook
        // still records the "evaluated, did not fold" entry, but stages no marker.
        let plugin = CompactPlugin::new(CompactConfig {
            threshold: 4,
            keep_last: 2,
            ..Default::default()
        });
        let hook = &plugin.resolve().phase_hooks[0];
        let reaction = hook.on_phase(&phase_ctx(), &convo(10), &Store::new()).await;
        assert!(injected(&reaction).is_empty());
        assert!(!RunCompactionMarker::is_recorded(&reaction.state, "r"));
        assert_eq!(
            reaction.state.len(),
            1,
            "only the no-fold ContextMessages entry is staged"
        );
    }

    /// A runner that returns a blank (whitespace-only) summary.
    struct BlankSummarizer;
    #[async_trait]
    impl RawTool for BlankSummarizer {
        fn id(&self) -> &str {
            "test_agent"
        }
        async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok(call.call_id, "   \n\t "))
        }
    }

    /// A runner that produces no assistant text at all.
    struct NoTextSummarizer;
    #[async_trait]
    impl RawTool for NoTextSummarizer {
        fn id(&self) -> &str {
            "test_agent"
        }
        async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok(call.call_id, ""))
        }
    }

    /// A runner that fails (unknown agent / transport error).
    struct ErrSummarizer;
    #[async_trait]
    impl RawTool for ErrSummarizer {
        fn id(&self) -> &str {
            "test_agent"
        }
        async fn invoke(&self, _call: ToolCall) -> Result<ToolOutput, ToolError> {
            Err(ToolError::Execution("boom".to_string()))
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_blank_or_missing_or_failed_summary_folds_nothing() {
        // Test design — Causes: summarization returns blank text, no text, or an
        // execution error. Effects: each row records no-fold without summary or
        // marker. Constraints/invariants: only nonblank successful output can
        // authorize a fold. Decision rule D1-D3: each degenerate result=>same
        // side-effect-free no-fold projection.
        // Each degenerate runner outcome must yield a no-fold decision: no summary
        // block injected and no compaction marker staged (only the no-fold entry).
        for runner in [
            Arc::new(BlankSummarizer) as Arc<dyn RawTool>,
            Arc::new(NoTextSummarizer),
            Arc::new(ErrSummarizer),
        ] {
            let plugin = CompactPlugin::new(CompactConfig {
                threshold: 4,
                keep_last: 2,
                ..Default::default()
            })
            .with_agent_tool(runner);
            let hook = &plugin.resolve().phase_hooks[0];
            let reaction = hook.on_phase(&phase_ctx(), &convo(10), &Store::new()).await;
            assert!(injected(&reaction).is_empty(), "no summary block");
            assert!(
                !RunCompactionMarker::is_recorded(&reaction.state, "r"),
                "no marker"
            );
            assert_eq!(reaction.state.len(), 1, "only the no-fold entry");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_no_fold_step_gates_a_later_grown_conversation() {
        // Test design — Causes: a Run first evaluates below threshold, persists
        // that decision, then its conversation grows past threshold. Effects:
        // the later call remains inert. Constraints/invariants: evaluation is
        // emit-once per Run, whether it folded or not. Decision rule G1:
        // recorded no-fold+later growth=>no late fold.
        // The None branch records an empty ContextMessages entry so a run that once
        // decided not to fold does not fold late when the conversation later grows.
        let plugin = CompactPlugin::new(CompactConfig {
            threshold: 40,
            keep_last: 8,
            ..Default::default()
        })
        .with_agent_tool(Arc::new(FixedSummarizer {
            seen_len: std::sync::Mutex::new(0),
        }));
        let hook = &plugin.resolve().phase_hooks[0];
        let mut state = Store::new();
        // Step 1: short conversation → evaluated, did not fold.
        let first = hook.on_phase(&phase_ctx(), &convo(5), &state).await;
        assert!(injected(&first).is_empty());
        assert!(!RunCompactionMarker::is_recorded(&first.state, "r"));
        assert_eq!(first.state.len(), 1, "the no-fold entry is recorded");
        for command in &first.state {
            state.apply(command);
        }
        // Step 2: the conversation has grown past threshold, but the run already
        // evaluated compaction → it must not fold late (emit-once per run).
        let second = hook.on_phase(&phase_ctx(), &convo(100), &state).await;
        assert!(second.state.is_empty(), "no late fold once evaluated");
        assert!(!RunCompactionMarker::is_recorded(&second.state, "r"));
    }

    #[test]
    fn compaction_marker_is_scoped_to_the_exact_run() {
        // Causes: C1 markers for Runs A and B share one Thread; C2 A is staged
        // twice by replay. Effects: E1 each exact Run remains observable; E2 an
        // unrelated Run remains absent. Decision rule R1=C1+C2=>E1+E2.
        // Constraints/invariants: marker membership is exact-Run scoped and
        // duplicate replay neither aliases nor creates another authority.
        let commands = vec![
            RunCompactionMarker::command("run-a"),
            RunCompactionMarker::command("run-b"),
            RunCompactionMarker::command("run-a"),
        ];
        assert!(
            RunCompactionMarker::is_recorded(&commands, "run-a"),
            "R1/E1"
        );
        assert!(
            RunCompactionMarker::is_recorded(&commands, "run-b"),
            "R1/E1"
        );
        assert!(
            !RunCompactionMarker::is_recorded(&commands, "run-c"),
            "R1/E2"
        );
    }
}
