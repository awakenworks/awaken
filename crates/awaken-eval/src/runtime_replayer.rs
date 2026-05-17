//! Runtime-backed [`Replayer`] that builds a real [`AgentRuntime`] with a
//! per-fixture [`ScriptedLlmExecutor`] and harvests the resulting
//! observability spans into a [`ReplayOutcome`].
//!
//! Unlike [`crate::replay::MockReplayer`], which synthesised a single
//! `GenAISpan` from heuristics, `RuntimeReplayer` exercises the full agent
//! loop and lets `awaken-ext-observability` record real spans. For
//! fixtures driven by an explicit `provider_script`, every token count
//! and stop reason in the resulting `AgentMetrics` comes straight from
//! the script. Legacy `mock_response: { kind: "text" }` fixtures still
//! load through [`Fixture::effective_script`], which seeds `TokenUsage`
//! with a `chars / 4` estimate to preserve the original
//! `max_tokens_total` semantics until those fixtures migrate.
//!
//! ## Determinism contract
//!
//! Eval replays must be reproducible across CI hosts. The replayer
//! therefore:
//!
//! - **Disables LLM retries** (`max_retries = 0`). A scripted `Error`
//!   event is consumed exactly once, so a `rate_limit` fixture cannot be
//!   silently turned into "first attempt errors, retries exhaust the
//!   script, runtime reports a different failure".
//! - **Asserts the `provider_script` is fully consumed** after the run,
//!   unless the fixture opts in to `allow_unused_provider_script`. This
//!   catches scripts where the runtime stopped early — e.g. a tool round
//!   that never fired or an expected retry that never happened.
//! - **Pins `upstream_model`** to [`Fixture::source_model_id`] when set,
//!   binding the scripted provider to that model and guarding every
//!   `InferenceRequest` through the executor.
//! - **Surfaces `error_type`** of the first scripted error event into
//!   [`ReplayOutcome::error_type`]. Without this, the runtime's
//!   `AgentLoopError::InferenceFailed(String)` would flatten the error
//!   variant and `05_error_path`-style fixtures would silently pass on
//!   "final text doesn't contain success".
//!
//! This is the eval framework's single source of truth for "what would
//! happen in production" once ADR-0032 is wired end-to-end.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use awaken_contract::contract::message::Message;
use awaken_contract::registry_spec::AgentSpec;
use awaken_ext_observability::{InMemorySink, ObservabilityPlugin};
use awaken_runtime::builder::AgentRuntimeBuilder;
use awaken_runtime::engine::{LlmRetryPolicy, RetryConfigKey, ScriptedLlmExecutor};
use awaken_runtime::registry::traits::ModelBinding;
use awaken_runtime::{AgentRuntime, RunRequest};
use awaken_stores::memory::InMemoryStore;

use crate::fixture::Fixture;
use crate::outcome::{ReplayOutcome, ReplayRuntimeFailure};
use crate::replay::Replayer;

/// Identifier the scripted provider registers under.
const SCRIPTED_PROVIDER_ID: &str = "scripted";
/// Identifier for the model binding the agent spec points at.
const SCRIPTED_MODEL_ID: &str = "scripted-model";
/// Default upstream model name used when the fixture does not pin
/// [`Fixture::source_model_id`].
const SCRIPTED_UPSTREAM_MODEL_DEFAULT: &str = "scripted";
/// Identifier of the synthetic agent driven by the replay.
const DEFAULT_AGENT_ID: &str = "default";
/// Static system prompt the synthetic agent uses.
const DEFAULT_SYSTEM_PROMPT: &str = "You are a test assistant.";

/// Replayer that drives a real [`AgentRuntime`] using the fixture's
/// `provider_script` (or the legacy `mock_response` shim).
pub struct RuntimeReplayer {
    max_rounds_floor: usize,
}

impl RuntimeReplayer {
    pub fn new() -> Self {
        Self {
            max_rounds_floor: 4,
        }
    }

    /// Override the minimum `max_rounds` applied to the synthetic
    /// agent spec. The effective value is `max(floor, script.len() + 1)`
    /// so scripts that emit several tool calls before a final response
    /// don't get clipped by the loop runner.
    #[must_use]
    pub fn with_max_rounds_floor(mut self, floor: usize) -> Self {
        self.max_rounds_floor = floor;
        self
    }
}

impl Default for RuntimeReplayer {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Replayer for RuntimeReplayer {
    async fn replay(&self, fixture: &Fixture) -> ReplayOutcome {
        let script = fixture.effective_script();
        let sink = InMemorySink::new();
        let plugin = ObservabilityPlugin::new(sink.clone()).with_provider(SCRIPTED_PROVIDER_ID);

        let store = Arc::new(InMemoryStore::new());
        let max_rounds = std::cmp::max(self.max_rounds_floor, script.len().saturating_add(1));

        let upstream_model = fixture
            .source_model_id
            .clone()
            .unwrap_or_else(|| SCRIPTED_UPSTREAM_MODEL_DEFAULT.to_string());

        // Pin the executor to the fixture's source model when one was
        // captured. The guard rejects mismatched `InferenceRequest`s with
        // `InvalidRequest` *without* consuming a scripted event.
        let executor = {
            let exec = ScriptedLlmExecutor::new(script);
            match &fixture.source_model_id {
                Some(model) => Arc::new(exec.with_expected_upstream_model(model.clone())),
                None => Arc::new(exec),
            }
        };

        // Disable LLM retries: a `rate_limit` scripted event is a single
        // explicit error, not an invitation for the runtime to retry into
        // a different failure mode. Anyone needing scripted retries must
        // express them as additional `Error` events.
        let agent_spec = AgentSpec {
            id: DEFAULT_AGENT_ID.into(),
            model_id: SCRIPTED_MODEL_ID.into(),
            system_prompt: DEFAULT_SYSTEM_PROMPT.into(),
            max_rounds,
            plugin_ids: vec!["observability".into()],
            ..Default::default()
        }
        .with_config::<RetryConfigKey>(LlmRetryPolicy::no_retry())
        .expect("LlmRetryPolicy serialises into AgentSpec.sections[\"retry\"]");

        let runtime: Arc<AgentRuntime> = Arc::new(
            AgentRuntimeBuilder::new()
                .with_provider(SCRIPTED_PROVIDER_ID, executor.clone())
                .with_model_binding(
                    SCRIPTED_MODEL_ID,
                    ModelBinding {
                        provider_id: SCRIPTED_PROVIDER_ID.into(),
                        upstream_model: upstream_model.clone(),
                    },
                )
                .with_thread_run_store(store.clone())
                .with_agent_spec(agent_spec)
                .with_plugin("observability", Arc::new(plugin))
                .build()
                .expect("scripted runtime builds"),
        );

        let request = RunRequest::new(
            format!("eval-thread-{}", fixture.id),
            vec![Message::user(&fixture.user_input)],
        )
        .with_agent_id(DEFAULT_AGENT_ID);

        let start = Instant::now();
        let outcome = runtime.run_to_completion(request).await;
        let elapsed = start.elapsed();

        let final_text = match &outcome {
            Ok(result) => result.response.clone(),
            Err(_) => String::new(),
        };

        let scripted_error = executor.first_error();
        let error_type = match &outcome {
            Ok(_) => None,
            // Prefer the *fixture-author-supplied* error_type captured by
            // the executor before the variant got flattened into
            // `AgentLoopError::InferenceFailed(String)`.
            Err(_) => scripted_error.as_ref().map(|(kind, _msg)| kind.clone()),
        };

        let runtime_failure = decide_runtime_failure(
            executor.exhausted_calls(),
            executor.remaining(),
            outcome.as_ref().err().map(|e| e.to_string()),
            scripted_error.is_some(),
            fixture.allow_unused_provider_script,
        );

        ReplayOutcome {
            fixture_id: fixture.id.clone(),
            final_text,
            metrics: sink.metrics(),
            elapsed,
            error_type,
            inference_error_count: executor.error_calls(),
            runtime_failure,
        }
    }
}

/// Pick the single most diagnostic [`ReplayRuntimeFailure`] for a
/// completed replay. Precedence (highest first):
///
///  1. **`ScriptExhausted`** — the executor was called when its script
///     was empty. Proves the runtime asked for more events than the
///     fixture promised; outranks everything because it points directly
///     at the runtime contract violation.
///  2. **`RuntimeError`** — the run returned `Err` and no scripted event
///     captured it. Catches non-scripted failures (model-guard mismatch,
///     resolver error, internal bug). Must outrank
///     `ProviderScriptUnused`: a `RuntimeError` often leaves the script
///     untouched (e.g. upstream_model guard rejects before popping), so
///     reporting "script unused" would hide the real cause.
///  3. **`ProviderScriptUnused`** — the run completed or failed via a
///     *scripted* error without consuming the whole script. Genuine
///     "runtime stopped early" territory.
fn decide_runtime_failure(
    exhausted_calls: usize,
    remaining: usize,
    runtime_error_message: Option<String>,
    has_scripted_error: bool,
    allow_unused: bool,
) -> Option<ReplayRuntimeFailure> {
    if exhausted_calls > 0 {
        return Some(ReplayRuntimeFailure::ScriptExhausted {
            extra_calls: exhausted_calls,
        });
    }
    if let Some(message) = runtime_error_message {
        if !has_scripted_error {
            return Some(ReplayRuntimeFailure::RuntimeError { message });
        }
        // Scripted error captured — the run failed *as expected*; only
        // unused script remains a fixture-contract concern.
    }
    if remaining > 0 && !allow_unused {
        return Some(ReplayRuntimeFailure::ProviderScriptUnused { remaining });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expectation::Expectation;
    use crate::fixture::MockResponse;
    use awaken_contract::contract::inference::{StopReason, TokenUsage};
    use awaken_runtime::engine::ProviderScriptEvent;

    fn text_fixture(id: &str, prompt: &str, response: &str) -> Fixture {
        Fixture {
            id: id.into(),
            description: None,
            user_input: prompt.into(),
            provider_script: Vec::new(),
            source_run_id: None,
            source_model_id: None,
            allow_unused_provider_script: false,
            mock_response: MockResponse::Text {
                text: response.into(),
            },
            expect: Expectation::default(),
        }
    }

    fn scripted_fixture(id: &str, prompt: &str, script: Vec<ProviderScriptEvent>) -> Fixture {
        Fixture {
            id: id.into(),
            description: None,
            user_input: prompt.into(),
            provider_script: script,
            source_run_id: None,
            source_model_id: None,
            allow_unused_provider_script: false,
            mock_response: MockResponse::default(),
            expect: Expectation::default(),
        }
    }

    #[tokio::test]
    async fn replay_chat_response_surfaces_scripted_answer() {
        let fx = text_fixture("rt-chat", "What is 2+2?", "the answer is 4");
        let outcome = RuntimeReplayer::new().replay(&fx).await;

        assert_eq!(outcome.fixture_id, "rt-chat");
        assert!(
            outcome.final_text.contains("the answer is 4"),
            "final_text {:?} did not contain scripted answer",
            outcome.final_text
        );
    }

    #[tokio::test]
    async fn replay_records_exactly_one_inference_for_single_turn() {
        let fx = text_fixture("rt-one", "p", "ok");
        let outcome = RuntimeReplayer::new().replay(&fx).await;

        assert_eq!(outcome.metrics.inference_count(), 1);
        assert_eq!(outcome.metrics.tool_count(), 0);
    }

    #[tokio::test]
    async fn replay_token_counts_come_from_provider_script() {
        let fx = scripted_fixture(
            "rt-tokens",
            "p",
            vec![ProviderScriptEvent::ChatResponse {
                content: "ok".into(),
                tokens: TokenUsage {
                    prompt_tokens: Some(12),
                    completion_tokens: Some(5),
                    total_tokens: Some(17),
                    ..Default::default()
                },
                finish_reason: StopReason::EndTurn,
            }],
        );

        let outcome = RuntimeReplayer::new().replay(&fx).await;

        assert_eq!(outcome.metrics.total_input_tokens(), 12);
        assert_eq!(outcome.metrics.total_output_tokens(), 5);
        // No `approximate_tokens` heuristic: 17 came straight from the script.
        assert_eq!(outcome.total_tokens(), 17);
    }

    #[tokio::test]
    async fn replay_error_event_surfaces_error_type_and_empty_text() {
        let fx = scripted_fixture(
            "rt-err",
            "p",
            vec![ProviderScriptEvent::Error {
                error_type: "rate_limit".into(),
                message: "429".into(),
            }],
        );
        let outcome = RuntimeReplayer::new().replay(&fx).await;

        assert_eq!(outcome.fixture_id, "rt-err");
        assert!(outcome.final_text.is_empty());
        // Review v1 #2: error_type must travel into the outcome so
        // scoring can assert on it instead of silently passing.
        assert_eq!(outcome.error_type.as_deref(), Some("rate_limit"));
        // Review v2 #2: failure path must report at least one
        // inference-error event so it doesn't look like "0 inferences
        // happened" in the report.
        assert_eq!(outcome.inference_error_count, 1);
        assert!(outcome.runtime_failure.is_none());
    }

    #[tokio::test]
    async fn replay_error_event_does_not_retry_under_default_policy() {
        // Review v2 #1: prove the runtime really makes exactly one call.
        // A second scripted ChatResponse sits behind the Error event; if
        // a retry fires we'd either consume `"would-be-retry"` (visible
        // through error_calls + consumed_calls) or exhaust into an extra
        // InvalidRequest call (visible through runtime_failure). Both
        // cases surface structurally instead of being inferred from
        // error_type alone.
        let fx = scripted_fixture(
            "rt-err-no-retry",
            "p",
            vec![
                ProviderScriptEvent::Error {
                    error_type: "rate_limit".into(),
                    message: "429".into(),
                },
                ProviderScriptEvent::ChatResponse {
                    content: "would-be-retry".into(),
                    tokens: TokenUsage::default(),
                    finish_reason: StopReason::EndTurn,
                },
            ],
        );
        let mut fx_allow = fx.clone();
        // Opt in so the trailing ChatResponse isn't itself flagged as
        // "unused script" — we want the assertion to focus on whether
        // it was *consumed*, not on whether it's left over.
        fx_allow.allow_unused_provider_script = true;

        let outcome = RuntimeReplayer::new().replay(&fx_allow).await;
        assert_eq!(outcome.error_type.as_deref(), Some("rate_limit"));
        assert_eq!(outcome.inference_error_count, 1);
        assert!(
            outcome.runtime_failure.is_none(),
            "no script exhaustion / runtime error expected, got {:?}",
            outcome.runtime_failure
        );
        // The decisive check: the ChatResponse retry-bait was *not*
        // consumed. If retry had fired this would be empty text from the
        // ChatResponse and inference_error_count would still be 1, but
        // final_text would change. More importantly the would-be-retry
        // event would have been popped — final_text would now be
        // "would-be-retry".
        assert!(
            !outcome.final_text.contains("would-be-retry"),
            "second event must not be consumed, got final_text {:?}",
            outcome.final_text
        );
    }

    #[tokio::test]
    async fn replay_surfaces_script_exhausted_when_runtime_overcalls() {
        // Build a fixture whose script has just one Error event but
        // disables the no-retry safeguard so the runtime would normally
        // retry. We can't easily flip the retry policy from outside
        // RuntimeReplayer, so this test instead manually feeds a
        // ScriptedLlmExecutor through more execute calls than it has
        // events for — proving the executor surfaces exhaustion in a
        // way RuntimeReplayer's mapping (`exhausted_calls > 0` →
        // ScriptExhausted) can pick up. The end-to-end mapping is
        // exercised by every other replay test indirectly: if the
        // mapping breaks, those tests' runtime_failure assertions
        // become noisy.
        let executor =
            awaken_runtime::engine::ScriptedLlmExecutor::new([ProviderScriptEvent::Error {
                error_type: "rate_limit".into(),
                message: "429".into(),
            }]);
        let req = awaken_contract::contract::executor::InferenceRequest {
            upstream_model: "scripted".into(),
            messages: vec![awaken_contract::contract::message::Message::user("p")],
            tools: vec![],
            system: vec![],
            overrides: None,
            enable_prompt_cache: false,
        };
        use awaken_contract::contract::executor::LlmExecutor;
        let _ = executor.execute(req.clone()).await.unwrap_err();
        let _ = executor.execute(req.clone()).await.unwrap_err();
        let _ = executor.execute(req).await.unwrap_err();
        assert_eq!(executor.exhausted_calls(), 2);
        assert_eq!(executor.error_calls(), 1);
        assert_eq!(executor.consumed_calls(), 1);
    }

    #[tokio::test]
    async fn replay_source_model_id_pins_upstream_model() {
        // Review #6: when source_model_id is set, both the registered
        // model binding and the ScriptedLlmExecutor's expected upstream
        // model must agree on it. Mismatches are exercised at the
        // executor seam in
        // `scripted::tests::expected_upstream_model_mismatch_does_not_consume_event`;
        // this test asserts the end-to-end happy path doesn't drop the
        // pin on the floor (which is what the legacy
        // `SCRIPTED_PROVIDER_ID.into()` upstream_model did).
        let mut fx = scripted_fixture(
            "rt-model-guard",
            "p",
            vec![ProviderScriptEvent::ChatResponse {
                content: "ok".into(),
                tokens: TokenUsage::default(),
                finish_reason: StopReason::EndTurn,
            }],
        );
        fx.source_model_id = Some("claude-opus-4-7".into());

        let outcome = RuntimeReplayer::new().replay(&fx).await;
        assert_eq!(outcome.final_text, "ok");
    }

    // ── decide_runtime_failure precedence ────────────────────────────

    #[test]
    fn decide_script_exhausted_outranks_everything() {
        let f = decide_runtime_failure(
            /* exhausted_calls */ 2,
            /* remaining */ 3,
            /* runtime_error_message */ Some("boom".into()),
            /* has_scripted_error */ true,
            /* allow_unused */ false,
        );
        assert_eq!(
            f,
            Some(ReplayRuntimeFailure::ScriptExhausted { extra_calls: 2 })
        );
    }

    #[test]
    fn decide_runtime_error_outranks_provider_script_unused() {
        // Review v3 #3: model-guard mismatch errors before consuming any
        // script event; old code reported ProviderScriptUnused and hid
        // the real failure. New precedence surfaces RuntimeError first.
        let f = decide_runtime_failure(
            0,
            /* remaining */ 1,
            Some("upstream_model mismatch".into()),
            /* has_scripted_error */ false,
            /* allow_unused */ false,
        );
        assert_eq!(
            f,
            Some(ReplayRuntimeFailure::RuntimeError {
                message: "upstream_model mismatch".into()
            })
        );
    }

    #[test]
    fn decide_scripted_error_plus_unused_script_falls_through_to_unused() {
        // Run failed via a *scripted* error — that's the intended path,
        // so don't promote it to RuntimeError. But the script also has
        // leftover events: that IS a fixture-contract concern.
        let f = decide_runtime_failure(
            0,
            /* remaining */ 2,
            Some("inference failed: rate limited".into()),
            /* has_scripted_error */ true,
            /* allow_unused */ false,
        );
        assert_eq!(
            f,
            Some(ReplayRuntimeFailure::ProviderScriptUnused { remaining: 2 })
        );
    }

    #[test]
    fn decide_clean_run_returns_none() {
        assert!(decide_runtime_failure(0, 0, None, false, false).is_none());
    }

    #[test]
    fn decide_allow_unused_suppresses_provider_script_unused() {
        let f = decide_runtime_failure(0, 5, None, false, /* allow_unused */ true);
        assert!(f.is_none());
    }

    #[test]
    fn decide_allow_unused_does_not_suppress_runtime_error() {
        let f = decide_runtime_failure(
            0,
            5,
            Some("boom".into()),
            false,
            /* allow_unused */ true,
        );
        assert_eq!(
            f,
            Some(ReplayRuntimeFailure::RuntimeError {
                message: "boom".into()
            })
        );
    }

    #[tokio::test]
    async fn replay_reports_unused_provider_script_as_runtime_failure() {
        // Review v2 #6: replay must not panic — surface a structured
        // failure so the NDJSON report stays complete and the CLI can
        // still record subsequent fixtures.
        let fx = scripted_fixture(
            "rt-unused",
            "p",
            vec![
                ProviderScriptEvent::ChatResponse {
                    content: "first".into(),
                    tokens: TokenUsage::default(),
                    finish_reason: StopReason::EndTurn,
                },
                // The runtime stops after the first chat response — this
                // second event is never consumed.
                ProviderScriptEvent::ChatResponse {
                    content: "second".into(),
                    tokens: TokenUsage::default(),
                    finish_reason: StopReason::EndTurn,
                },
            ],
        );
        let outcome = RuntimeReplayer::new().replay(&fx).await;
        assert!(outcome.final_text.contains("first"));
        assert_eq!(
            outcome.runtime_failure,
            Some(ReplayRuntimeFailure::ProviderScriptUnused { remaining: 1 })
        );
    }

    #[tokio::test]
    async fn replay_allow_unused_provider_script_opts_out_of_consumption_check() {
        let mut fx = scripted_fixture(
            "rt-unused-ok",
            "p",
            vec![
                ProviderScriptEvent::ChatResponse {
                    content: "first".into(),
                    tokens: TokenUsage::default(),
                    finish_reason: StopReason::EndTurn,
                },
                ProviderScriptEvent::ChatResponse {
                    content: "second".into(),
                    tokens: TokenUsage::default(),
                    finish_reason: StopReason::EndTurn,
                },
            ],
        );
        fx.allow_unused_provider_script = true;
        let outcome = RuntimeReplayer::new().replay(&fx).await;
        assert_eq!(outcome.final_text, "first");
    }

    #[tokio::test]
    async fn replay_inference_span_uses_scripted_provider() {
        let fx = text_fixture("rt-prov", "p", "ok");
        let outcome = RuntimeReplayer::new().replay(&fx).await;
        let span = outcome
            .metrics
            .inferences
            .first()
            .expect("at least one span");
        assert_eq!(span.provider, SCRIPTED_PROVIDER_ID);
        assert!(!span.context.run_id.is_empty());
        assert!(!span.context.thread_id.is_empty());
    }
}
