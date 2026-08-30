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

use crate::agent::SUMMARIZE_PROMPT;
use crate::backend::{CompactArtifact, CompactBackend, CompactRequest};
use crate::config::CompactConfig;
use crate::fold::{prefetch_fold_point, token_fold_point};
use async_trait::async_trait;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::state::{StateKey, Store};
use awaken_runtime_contract::compaction::RunCompactionMarker;
use awaken_runtime_contract::content_fingerprint;
use awaken_runtime_contract::plugin::{
    CapabilityBound, ContextMessages, ContextWindow, ContextWindowPlan, Contributions,
    HookReaction, IdBound, PhaseContext, PhaseHook, PhaseHookPoint, Plugin, PluginConfigError,
    PluginManifest,
};

/// A rough, deterministic token estimate (~4 chars/token) over a message slice.
/// Cheap enough to run every `BeforeInference` without a real tokenizer; it drives
/// the publication-frozen token-aware compaction trigger (`max_tokens`).
fn estimate_tokens(messages: &[Message]) -> u64 {
    let chars: usize = messages.iter().map(|m| m.text_content().len()).sum();
    (chars / 4) as u64
}

/// The plugin id under which compaction is activated (G30).
pub const COMPACT_PLUGIN_ID: &str = "compact";

/// Contributes the compaction hook. The host-backed [`CompactBackend`] is the
/// single labor boundary; without one the hook is inert.
pub struct CompactPlugin {
    config: CompactConfig,
    backend: Option<(String, Arc<dyn CompactBackend>)>,
}

impl CompactPlugin {
    pub fn new(config: CompactConfig) -> Self {
        Self {
            config,
            backend: None,
        }
    }

    /// Use the host's asynchronous compaction backend in this parent-Thread
    /// namespace.
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
    backend: Option<(String, Arc<dyn CompactBackend>)>,
}

/// Fail-closed admission for a backend artifact. The backend is an accelerator,
/// never an authority: the plugin independently checks the exact scope, stable
/// content key, covered prefix, and nonblank result before context injection.
const fn artifact_is_usable(
    scope_matches: bool,
    key_matches: bool,
    coverage_matches: bool,
    covered_messages: usize,
    fold_to: usize,
    summary_nonempty: bool,
) -> bool {
    scope_matches
        && key_matches
        && coverage_matches
        && covered_messages <= fold_to
        && summary_nonempty
}

impl CompactHook {
    fn fold_to(&self, conversation: &[Message]) -> Option<usize> {
        // Config publication already derived the one effective token window.
        // Absent means there is no trigger basis; Runtime never invents a
        // message-count fallback or a second ratio.
        self.config.max_tokens.and_then(|max_tokens| {
            token_fold_point(
                estimate_tokens(conversation),
                max_tokens,
                conversation.len(),
                self.config.keep_last,
            )
        })
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
        request: &CompactRequest,
        conversation: &[Message],
        fold_to: usize,
    ) -> Option<Vec<Message>> {
        if !artifact_is_usable(
            artifact.scope == request.scope,
            artifact.key == request.key,
            artifact.covered_messages == request.covered_messages,
            artifact.covered_messages,
            fold_to,
            !artifact.summary.trim().is_empty(),
        ) {
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

    async fn compute(&self, conversation: &[Message], fold_to: usize) -> Option<Vec<Message>> {
        let (scope, backend) = self.backend.as_ref()?;
        if let Some(ready) = backend.latest_ready(scope, fold_to).await {
            // Validate the bound before slicing `conversation` to reconstruct
            // the stable key. An invalid cache row is discarded and the exact
            // hard request remains authoritative.
            if ready.covered_messages <= fold_to {
                let request = self.request(scope, conversation, ready.covered_messages);
                if let Some(block) = Self::block(ready, &request, conversation, fold_to) {
                    return Some(block);
                }
            }
        }
        let request = self.request(scope, conversation, fold_to);
        Self::block(
            backend.summarize(request.clone()).await?,
            &request,
            conversation,
            fold_to,
        )
    }
}

#[cfg(kani)]
mod artifact_verification {
    use super::*;

    #[kani::proof]
    fn compaction_artifact_requires_exact_identity_coverage_and_content() {
        let scope_matches: bool = kani::any();
        let key_matches: bool = kani::any();
        let coverage_matches: bool = kani::any();
        let covered_messages: usize = kani::any();
        let fold_to: usize = kani::any();
        let summary_nonempty: bool = kani::any();
        assert_eq!(
            artifact_is_usable(
                scope_matches,
                key_matches,
                coverage_matches,
                covered_messages,
                fold_to,
                summary_nonempty,
            ),
            scope_matches
                && key_matches
                && coverage_matches
                && covered_messages <= fold_to
                && summary_nonempty,
        );
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
        let Some(fold_to) = self.fold_to(conversation) else {
            // A soft prefetch is preparation only. It must not write the
            // hard-decision marker, otherwise a later step in this same Run can
            // never cross the hard threshold and consume the prepared artifact.
            if let Some((scope, backend)) = &self.backend
                && let Some(max_tokens) = self.config.max_tokens
                && let Some(prefetch_to) = prefetch_fold_point(
                    conversation.len(),
                    estimate_tokens(conversation),
                    max_tokens,
                    self.config.prefetch_ratio,
                    self.config.keep_last,
                )
            {
                backend
                    .prefetch(self.request(scope, conversation, prefetch_to))
                    .await;
            }
            return HookReaction::default();
        };
        match self.compute(conversation, fold_to).await {
            // A fold happened: record the summary under this producer in
            // `ContextMessages` (the kernel injects it request-only across steps and
            // a resumed run) *and* stage the durable, protocol-neutral marker the
            // adapter projects into the event.
            Some(block) => HookReaction::state(vec![
                ContextMessages::write(&BTreeMap::from([(COMPACT_PLUGIN_ID.to_string(), block)])),
                ContextWindow::write(&ContextWindowPlan::anchored(
                    self.config.keep_last,
                    conversation.len(),
                )),
                RunCompactionMarker::command(&ctx.run_id.0),
            ]),
            // The hard threshold fired but labor produced no admissible artifact:
            // record that exact hard attempt, so a later step does not start a
            // second compactor Run under the same parent Run.
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

    struct FakeBackend {
        prefetched: std::sync::Mutex<Vec<CompactRequest>>,
        ready: std::sync::Mutex<Option<CompactArtifact>>,
        summarized: std::sync::Mutex<Vec<CompactRequest>>,
        hard_summary: std::sync::Mutex<Option<String>>,
        hard_scope: std::sync::Mutex<Option<String>>,
        hard_key: std::sync::Mutex<Option<String>>,
        hard_covered_messages: std::sync::Mutex<Option<usize>>,
    }

    #[async_trait]
    impl CompactBackend for FakeBackend {
        async fn prefetch(&self, request: CompactRequest) {
            self.prefetched.lock().unwrap().push(request);
        }

        async fn latest_ready(
            &self,
            _scope: &str,
            _at_most_messages: usize,
        ) -> Option<CompactArtifact> {
            // Deliberately return the configured artifact verbatim. Tests then
            // prove the plugin, not this fake, owns fail-closed admission.
            self.ready.lock().unwrap().clone()
        }

        async fn summarize(&self, request: CompactRequest) -> Option<CompactArtifact> {
            self.summarized.lock().unwrap().push(request.clone());
            Some(CompactArtifact {
                scope: self
                    .hard_scope
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or(request.scope),
                key: self.hard_key.lock().unwrap().clone().unwrap_or(request.key),
                covered_messages: self
                    .hard_covered_messages
                    .lock()
                    .unwrap()
                    .unwrap_or(request.covered_messages),
                summary: self.hard_summary.lock().unwrap().clone()?,
            })
        }
    }

    fn fake_backend(ready: Option<CompactArtifact>) -> Arc<FakeBackend> {
        Arc::new(FakeBackend {
            prefetched: std::sync::Mutex::new(Vec::new()),
            ready: std::sync::Mutex::new(ready),
            summarized: std::sync::Mutex::new(Vec::new()),
            hard_summary: std::sync::Mutex::new(Some("hard summary".into())),
            hard_scope: std::sync::Mutex::new(None),
            hard_key: std::sync::Mutex::new(None),
            hard_covered_messages: std::sync::Mutex::new(None),
        })
    }

    fn ready_artifact(
        config: &CompactConfig,
        scope: &str,
        conversation: &[Message],
        covered_messages: usize,
        summary: &str,
    ) -> CompactArtifact {
        let hook = CompactHook {
            config: config.clone(),
            backend: None,
        };
        let request = hook.request(scope, conversation, covered_messages);
        CompactArtifact {
            scope: request.scope,
            key: request.key,
            covered_messages,
            summary: summary.into(),
        }
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
        let backend = fake_backend(None);
        let plugin = CompactPlugin::new(CompactConfig {
            keep_last: 8,
            max_tokens: Some(1_000),
            ..Default::default()
        })
        .with_backend("thread-a", backend.clone());
        let hook = &plugin.resolve().phase_hooks[0];
        let reaction = hook.on_phase(&phase_ctx(), &convo(5), &Store::new()).await;
        assert!(injected(&reaction).is_empty());
        assert!(backend.summarized.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn long_conversation_summarizes_the_older_slice() {
        let backend = fake_backend(None);
        let plugin = CompactPlugin::new(CompactConfig {
            keep_last: 2,
            max_tokens: Some(1),
            ..Default::default()
        })
        .with_backend("thread-a", backend.clone());
        let hook = &plugin.resolve().phase_hooks[0];
        // 10 messages, keep_last 2 → summarize the first 8.
        let reaction = hook.on_phase(&phase_ctx(), &convo(10), &Store::new()).await;
        assert_eq!(backend.summarized.lock().unwrap()[0].seed.len(), 9); // 8 folded + prompt
        let block = injected(&reaction);
        assert_eq!(block.len(), 1);
        assert!(
            block[0]
                .text_content()
                .contains("Summary of earlier conversation: hard summary")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn soft_token_window_prefetches_without_blocking_or_marking_a_fold() {
        // Test design — Causes: conversation crosses only the soft prefetch
        // token window. Effects: one prefix is prefetched with no injected summary,
        // synchronous summarize call, or marker. Constraints/invariants: prefetch
        // is non-blocking and not a fold authority. Decision rule P1: soft-only=>
        // one prefetch request and zero committed fold effects.
        let backend = fake_backend(None);
        let plugin = CompactPlugin::new(CompactConfig {
            keep_last: 2,
            max_tokens: Some(20),
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
        // Test design — Causes: the hard token window is crossed with a ready cached
        // summary covering five messages. Effects: cache is reused and every
        // uncovered pre-tail message is bridged before the marker. Constraints/
        // invariants: no second summary is generated and message order is exact.
        // Decision rule H1: ready prefix=>summary+uncovered bridge+one marker.
        let config = CompactConfig {
            keep_last: 2,
            max_tokens: Some(1),
            ..Default::default()
        };
        let conversation = convo(10);
        let backend = fake_backend(Some(ready_artifact(
            &config,
            "thread-a",
            &conversation,
            5,
            "soft summary",
        )));
        let plugin = CompactPlugin::new(config).with_backend("thread-a", backend.clone());
        let reaction = plugin.resolve().phase_hooks[0]
            .on_phase(&phase_ctx(), &conversation, &Store::new())
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
        // Test design — Causes: hard token window is crossed and no prefix is cached.
        // Effects: the exact eight-message prefix is summarized, injected, and
        // marked. Constraints/invariants: one stable request owns the fold and
        // the retained tail is excluded. Decision rule H2: hard+miss=>one await,
        // one summary block, one run marker.
        let backend = fake_backend(None);
        let plugin = CompactPlugin::new(CompactConfig {
            keep_last: 2,
            max_tokens: Some(1),
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

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_ready_artifact_is_discarded_before_exact_hard_resolution() {
        // Cause/effect decision table: C1 ready scope differs; C2 stable key
        // differs; C3 covered prefix exceeds the hard fold point. For each row,
        // E1 the cache row is never injected, E2 the exact hard request runs,
        // and E3 only that exact result authorizes the marker. Constraint: cache
        // readiness is acceleration evidence, never context authority.
        let config = CompactConfig {
            keep_last: 2,
            max_tokens: Some(1),
            ..Default::default()
        };
        let conversation = convo(10);
        for axis in ["scope", "key", "coverage"] {
            let mut artifact = ready_artifact(
                &config,
                "thread-a",
                &conversation,
                if axis == "coverage" { 9 } else { 5 },
                "must not be injected",
            );
            if axis == "scope" {
                artifact.scope = "thread-b".into();
            } else if axis == "key" {
                artifact.key = "wrong-content-key".into();
            }
            let backend = fake_backend(Some(artifact));
            let plugin =
                CompactPlugin::new(config.clone()).with_backend("thread-a", backend.clone());
            let reaction = plugin.resolve().phase_hooks[0]
                .on_phase(&phase_ctx(), &conversation, &Store::new())
                .await;

            assert_eq!(backend.summarized.lock().unwrap().len(), 1, "{axis}/E2");
            let block = injected(&reaction);
            assert_eq!(block.len(), 1, "{axis}/E1+E2");
            assert!(
                block[0].text_content().contains("hard summary"),
                "{axis}/E1"
            );
            assert!(
                RunCompactionMarker::is_recorded(&reaction.state, "r"),
                "{axis}/E3"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_hard_artifact_fails_closed_without_context_injection() {
        // Cause/effect decision table: H1 exact request returns another scope;
        // H2 another stable key; H3 another coverage boundary. Each row causes
        // E1 no injected summary, E2 no compaction marker, and E3 one recorded
        // failed hard attempt. Constraint: a backend result cannot widen or
        // substitute the request identity that authorized its labor.
        for axis in ["scope", "key", "coverage"] {
            let backend = fake_backend(None);
            match axis {
                "scope" => *backend.hard_scope.lock().unwrap() = Some("thread-b".into()),
                "key" => *backend.hard_key.lock().unwrap() = Some("wrong-content-key".into()),
                "coverage" => *backend.hard_covered_messages.lock().unwrap() = Some(7),
                _ => unreachable!(),
            }
            let plugin = CompactPlugin::new(CompactConfig {
                keep_last: 2,
                max_tokens: Some(1),
                ..Default::default()
            })
            .with_backend("thread-a", backend);
            let reaction = plugin.resolve().phase_hooks[0]
                .on_phase(&phase_ctx(), &convo(10), &Store::new())
                .await;

            assert!(injected(&reaction).is_empty(), "{axis}/E1");
            assert!(
                !RunCompactionMarker::is_recorded(&reaction.state, "r"),
                "{axis}/E2"
            );
            assert_eq!(reaction.state.len(), 1, "{axis}/E3");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_custom_instructions_config_overrides_the_compaction_prompt() {
        let default_backend = fake_backend(None);
        // With no override, the built-in summarize prompt is used.
        let default_plugin = CompactPlugin::new(CompactConfig {
            keep_last: 2,
            max_tokens: Some(1),
            ..Default::default()
        })
        .with_backend("thread-a", default_backend.clone());
        default_plugin.resolve().phase_hooks[0]
            .on_phase(&phase_ctx(), &convo(10), &Store::new())
            .await;
        assert_eq!(
            default_backend.summarized.lock().unwrap()[0]
                .seed
                .last()
                .map(Message::text_content)
                .unwrap_or_default(),
            SUMMARIZE_PROMPT
        );

        // With an override, the per-agent instructions replace it verbatim.
        let custom = "Keep only the API endpoints mentioned.";
        let tuned_backend = fake_backend(None);
        let tuned_plugin = CompactPlugin::new(CompactConfig {
            keep_last: 2,
            max_tokens: Some(1),
            instructions: Some(custom.to_string()),
            ..Default::default()
        })
        .with_backend("thread-a", tuned_backend.clone());
        tuned_plugin.resolve().phase_hooks[0]
            .on_phase(&phase_ctx(), &convo(10), &Store::new())
            .await;
        assert_eq!(
            tuned_backend.summarized.lock().unwrap()[0]
                .seed
                .last()
                .map(Message::text_content)
                .unwrap_or_default(),
            custom
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_fold_stages_a_readable_compaction_marker() {
        // Test design — Causes: an eligible long conversation produces a summary.
        // Effects: summary, context window, and one readable Run marker are staged.
        // Constraints/invariants: the marker shares the same staged state as the
        // fold and is readable by the canonical helper. Decision rule M1:
        // successful fold=>three commands, marker=true, retained window=2.
        let backend = fake_backend(None);
        let plugin = CompactPlugin::new(CompactConfig {
            keep_last: 2,
            max_tokens: Some(1),
            ..Default::default()
        })
        .with_backend("thread-a", backend);
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
        let window = ContextWindow::load(&state).unwrap();
        assert_eq!(window.keep_last_at(10), Some(2));
        assert_eq!(
            window.keep_last_at(13),
            Some(5),
            "the suffix grows with the transcript so the fold boundary stays fixed"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_short_conversation_stages_no_hard_decision() {
        // Test design — Causes: conversation remains below the hard token window.
        // Effects: no state or marker is staged. Constraints/invariants: a soft
        // observation cannot consume the later hard decision. Decision rule M2:
        // short input=>zero summary, marker, and hard-decision state.
        let backend = fake_backend(None);
        let plugin = CompactPlugin::new(CompactConfig {
            keep_last: 8,
            max_tokens: Some(1_000),
            ..Default::default()
        })
        .with_backend("thread-a", backend);
        let hook = &plugin.resolve().phase_hooks[0];
        let reaction = hook.on_phase(&phase_ctx(), &convo(5), &Store::new()).await;
        assert!(reaction.state.is_empty());
        assert!(!RunCompactionMarker::is_recorded(&reaction.state, "r"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_run_stages_the_fact_at_most_once() {
        // Test design — Causes: the same Run evaluates an eligible conversation
        // twice after applying its first staged state. Effects: the first call
        // folds; replay stages nothing. Constraints/invariants: compaction is
        // emit-once per Run. Decision rule I1: absent marker=>fold once;
        // recorded marker=>no messages and no state.
        let backend = fake_backend(None);
        let plugin = CompactPlugin::new(CompactConfig {
            keep_last: 2,
            max_tokens: Some(1),
            ..Default::default()
        })
        .with_backend("thread-a", backend);
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
    async fn frozen_token_window_triggers_the_fold() {
        // Test design — Cause: estimated tokens reach the sole publication-frozen
        // effective window. Effects: the older slice folds and records a marker.
        // Decision rule T1: token-at-or-over-window=>one fold.
        let backend = fake_backend(None);
        let plugin = CompactPlugin::new(CompactConfig {
            agent_id: crate::COMPACT_AGENT_ID.to_string(),
            agent_instructions: None,
            keep_last: 2,
            max_tokens: Some(10),
            prefetch_ratio: 0.75,
            instructions: None,
        })
        .with_backend("thread-a", backend);
        let hook = &plugin.resolve().phase_hooks[0];
        // 10 short messages exceed the 10-token effective window.
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
        // Test design — Cause: token usage remains below the frozen window.
        // Effects: no summary and no marker are staged.
        // Constraints/invariants: configured capacity is not treated as usage.
        // Decision rule T2: token-under+count-under=>no fold effects.
        let backend = fake_backend(None);
        let plugin = CompactPlugin::new(CompactConfig {
            agent_id: crate::COMPACT_AGENT_ID.to_string(),
            agent_instructions: None,
            keep_last: 2,
            max_tokens: Some(1_000_000),
            prefetch_ratio: 0.75,
            instructions: None,
        })
        .with_backend("thread-a", backend);
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
    async fn without_a_backend_the_hook_is_inert() {
        // Test design — Causes: a long conversation reaches a hook with no
        // backend. Effects: it records only the failed hard attempt. Constraints/
        // invariants: absence cannot invoke a fallback labor path or mark a fold.
        // Decision rule A1: eligible+no backend=>no summary/marker.
        let plugin = CompactPlugin::new(CompactConfig {
            keep_last: 2,
            max_tokens: Some(1),
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

    #[tokio::test(flavor = "current_thread")]
    async fn a_blank_or_missing_backend_artifact_folds_nothing() {
        // Test design — Causes: the sole backend returns either blank content or
        // no artifact. Effects: each row records a failed hard attempt without a
        // summary or marker. Constraints/invariants: only nonblank successful
        // output can authorize a fold. Rules D1=blank and D2=missing both yield
        // the same side-effect-free no-fold projection.
        for summary in [Some("   \n\t ".to_string()), None] {
            let backend = fake_backend(None);
            *backend.hard_summary.lock().unwrap() = summary;
            let plugin = CompactPlugin::new(CompactConfig {
                keep_last: 2,
                max_tokens: Some(1),
                ..Default::default()
            })
            .with_backend("thread-a", backend);
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
    async fn a_soft_prefetch_does_not_consume_the_later_hard_fold() {
        // Test design — Causes: C1 a Run first crosses only the soft threshold;
        // C2 its conversation later crosses the hard threshold. Effects: E1 C1
        // schedules prefetch without durable decision state; E2 C2 performs one
        // exact hard resolution and stages the fold. Decision rule G1=C1+C2=>
        // E1+E2. Constraint: an optimization cannot consume hard authority.
        let backend = fake_backend(None);
        let plugin = CompactPlugin::new(CompactConfig {
            keep_last: 8,
            max_tokens: Some(40),
            prefetch_ratio: 0.5,
            ..Default::default()
        })
        .with_backend("thread-a", backend.clone());
        let hook = &plugin.resolve().phase_hooks[0];
        let mut state = Store::new();
        // Ten short messages cross the 20-token soft threshold but not 40 hard.
        let first = hook.on_phase(&phase_ctx(), &convo(10), &state).await;
        assert!(injected(&first).is_empty());
        assert!(!RunCompactionMarker::is_recorded(&first.state, "r"));
        assert!(first.state.is_empty(), "G1/E1");
        assert_eq!(backend.prefetched.lock().unwrap().len(), 1, "G1/E1");
        for command in &first.state {
            state.apply(command);
        }
        // Step 2: growth crosses the hard threshold and remains eligible.
        let second = hook.on_phase(&phase_ctx(), &convo(100), &state).await;
        assert_eq!(backend.summarized.lock().unwrap().len(), 1, "G1/E2");
        assert!(RunCompactionMarker::is_recorded(&second.state, "r"));
        assert_eq!(injected(&second).len(), 1, "G1/E2");
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
