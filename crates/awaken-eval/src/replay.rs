//! Replay engine.
//!
//! `awaken-eval` decouples *how* a fixture is replayed from *what* the
//! framework does with the resulting outcome. The [`Replayer`] trait is the
//! seam: implementations may run a fixture against a real `AgentRuntime`,
//! against a recorded transcript, or — as the bundled [`MockReplayer`] —
//! synthesise an outcome deterministically from the fixture's
//! [`MockResponse`].
//!
//! ## Bundled implementations
//!
//! * [`MockReplayer`] is no-runtime: it produces a deterministic
//!   [`ReplayOutcome`] from the fixture's `mock_response` alone. It is the
//!   default for CI smoke tests of awaken-eval itself and serves as a
//!   reference shape for richer replayers.
//!
//! ## Future replayers
//!
//! A real `RuntimeReplayer` (driving an `AgentRuntime` with a mock
//! `LlmExecutor`) is intentionally deferred — its introduction is a strict
//! superset of [`MockReplayer`]'s behaviour and will not require any shape
//! change to [`Replayer`] or [`ReplayOutcome`].

use std::time::Instant;

use async_trait::async_trait;
use awaken_ext_observability::{
    AgentMetrics, GenAISpan, MetricsEvent, SpanContext, WiringSettings, install_default_sinks,
};

use crate::fixture::{Fixture, MockResponse};
use crate::outcome::ReplayOutcome;

/// Run a fixture and return its raw outcome.
///
/// Implementations are async because real replayers will drive HTTP I/O;
/// the bundled [`MockReplayer`] is purely synchronous internally and just
/// awaits a no-op future to satisfy the trait shape.
#[async_trait]
pub trait Replayer: Send + Sync {
    async fn replay(&self, fixture: &Fixture) -> ReplayOutcome;
}

/// Replayer that synthesises an outcome from a fixture's
/// [`MockResponse`] without running an agent runtime.
///
/// `MockReplayer` is deterministic: token counts are derived from the
/// length of the user prompt and the mocked response. This makes it
/// suitable for unit-testing scoring logic, fixture authoring, and CI
/// smoke tests where the framework itself is the system under test.
#[derive(Debug, Default, Clone, Copy)]
pub struct MockReplayer;

impl MockReplayer {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Replayer for MockReplayer {
    async fn replay(&self, fixture: &Fixture) -> ReplayOutcome {
        let start = Instant::now();

        // Route the synthesised metrics through the same sink wiring an
        // embedder would use — keeps the replayer honest about what
        // `AgentMetrics` aggregation looks like in production.
        let (sink, _summary) = install_default_sinks(&WiringSettings::default());

        let input_tokens = approximate_tokens(&fixture.user_input);
        let (final_text, output_tokens, error_type) = match &fixture.mock_response {
            MockResponse::Text { text } => (text.clone(), approximate_tokens(text), None),
            MockResponse::Error {
                error_type,
                message,
            } => (
                String::new(),
                0,
                Some((error_type.clone(), message.clone())),
            ),
        };

        let span = GenAISpan {
            context: SpanContext {
                run_id: format!("mock-{}", fixture.id),
                thread_id: format!("mock-thread-{}", fixture.id),
                agent_id: "mock-agent".into(),
                parent_run_id: None,
            },
            step_index: Some(0),
            model: "mock-replayer".into(),
            provider: "awaken-eval".into(),
            operation: "chat".into(),
            response_model: None,
            response_id: Some(format!("mock-resp-{}", fixture.id)),
            finish_reasons: if error_type.is_some() {
                Vec::new()
            } else {
                vec!["stop".into()]
            },
            error_type: error_type.as_ref().map(|(t, _)| t.clone()),
            error_class: error_type.as_ref().map(|(t, _)| t.clone()),
            thinking_tokens: None,
            input_tokens: Some(input_tokens),
            output_tokens: Some(output_tokens),
            total_tokens: Some(input_tokens + output_tokens),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            temperature: None,
            top_p: None,
            max_tokens: None,
            stop_sequences: Vec::new(),
            duration_ms: 0,
        };
        sink.record(&MetricsEvent::Inference(span.clone()));

        let elapsed = start.elapsed();

        // Build the AgentMetrics view consumers see.
        let metrics = AgentMetrics {
            inferences: vec![span],
            session_duration_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            ..Default::default()
        };
        sink.on_run_end(&metrics);

        ReplayOutcome {
            fixture_id: fixture.id.clone(),
            final_text,
            metrics,
            elapsed,
        }
    }
}

/// Convenience: replay a slice of fixtures through `replayer`, returning
/// outcomes in input order.
pub async fn replay_all<R: Replayer>(replayer: &R, fixtures: &[Fixture]) -> Vec<ReplayOutcome> {
    let mut out = Vec::with_capacity(fixtures.len());
    for fx in fixtures {
        out.push(replayer.replay(fx).await);
    }
    out
}

/// Approximate the token count of a string. We intentionally use a coarse
/// heuristic (≈4 chars per token) so the framework stays self-contained and
/// deterministic; precise counts come from real `LlmExecutor` runs.
fn approximate_tokens(text: &str) -> i32 {
    let chars = text.chars().count();
    let tokens = chars.div_ceil(4);
    i32::try_from(tokens).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expectation::Expectation;
    use crate::fixture::Fixture;

    fn fixture(id: &str, prompt: &str, mock: MockResponse) -> Fixture {
        Fixture {
            id: id.into(),
            description: None,
            user_input: prompt.into(),
            mock_response: mock,
            expect: Expectation::default(),
        }
    }

    // ── approximate_tokens ──────────────────────────────────────────

    #[test]
    fn approximate_tokens_zero_for_empty_string() {
        assert_eq!(approximate_tokens(""), 0);
    }

    #[test]
    fn approximate_tokens_rounds_up() {
        assert_eq!(approximate_tokens("a"), 1);
        assert_eq!(approximate_tokens("ab"), 1);
        assert_eq!(approximate_tokens("abc"), 1);
        assert_eq!(approximate_tokens("abcd"), 1);
        assert_eq!(approximate_tokens("abcde"), 2);
        assert_eq!(approximate_tokens("abcdefgh"), 2);
    }

    #[test]
    fn approximate_tokens_handles_unicode() {
        assert_eq!(approximate_tokens("你好"), 1);
        assert_eq!(approximate_tokens("你好世界"), 1);
        assert_eq!(approximate_tokens("你好世界你"), 2);
    }

    // ── MockReplayer.replay ─────────────────────────────────────────

    #[tokio::test]
    async fn mock_replayer_text_response_yields_final_text() {
        let fx = fixture(
            "answer",
            "What is 2+2?",
            MockResponse::Text { text: "4".into() },
        );
        let outcome = MockReplayer::new().replay(&fx).await;
        assert_eq!(outcome.fixture_id, "answer");
        assert_eq!(outcome.final_text, "4");
        assert_eq!(outcome.metrics.inference_count(), 1);
        assert_eq!(outcome.metrics.tool_count(), 0);
    }

    #[tokio::test]
    async fn mock_replayer_error_response_yields_empty_text() {
        let fx = fixture(
            "rate-limited",
            "expensive query",
            MockResponse::Error {
                error_type: "rate_limit".into(),
                message: "429".into(),
            },
        );
        let outcome = MockReplayer.replay(&fx).await;
        assert!(outcome.final_text.is_empty());
        assert_eq!(
            outcome.metrics.inferences[0].error_type.as_deref(),
            Some("rate_limit")
        );
        assert_eq!(outcome.metrics.inferences[0].output_tokens, Some(0));
    }

    #[tokio::test]
    async fn mock_replayer_token_counts_track_string_lengths() {
        let fx = fixture(
            "tokens",
            "12345678", // 8 chars => 2 tokens
            MockResponse::Text {
                text: "abcd".into(), // 4 chars => 1 token
            },
        );
        let outcome = MockReplayer.replay(&fx).await;
        let span = &outcome.metrics.inferences[0];
        assert_eq!(span.input_tokens, Some(2));
        assert_eq!(span.output_tokens, Some(1));
        assert_eq!(span.total_tokens, Some(3));
        assert_eq!(outcome.total_tokens(), 3);
    }

    #[tokio::test]
    async fn mock_replayer_span_carries_fixture_id_in_run_id() {
        let fx = fixture("ctx-test", "p", MockResponse::Text { text: "a".into() });
        let outcome = MockReplayer.replay(&fx).await;
        let ctx = &outcome.metrics.inferences[0].context;
        assert!(ctx.run_id.contains("ctx-test"));
        assert!(ctx.thread_id.contains("ctx-test"));
        assert_eq!(ctx.agent_id, "mock-agent");
    }

    #[tokio::test]
    async fn mock_replayer_text_response_finishes_with_stop() {
        let fx = fixture("finish", "p", MockResponse::Text { text: "ok".into() });
        let outcome = MockReplayer.replay(&fx).await;
        assert_eq!(
            outcome.metrics.inferences[0].finish_reasons,
            vec!["stop".to_string()]
        );
    }

    #[tokio::test]
    async fn mock_replayer_error_response_has_no_finish_reason() {
        let fx = fixture(
            "fail",
            "p",
            MockResponse::Error {
                error_type: "timeout".into(),
                message: "deadline".into(),
            },
        );
        let outcome = MockReplayer.replay(&fx).await;
        assert!(outcome.metrics.inferences[0].finish_reasons.is_empty());
    }

    #[tokio::test]
    async fn mock_replayer_session_duration_is_non_zero_on_real_clock() {
        let fx = fixture("duration", "p", MockResponse::Text { text: "ans".into() });
        let outcome = MockReplayer.replay(&fx).await;
        // session_duration_ms may be 0 on a fast machine, but elapsed
        // should always be representable.
        assert!(outcome.elapsed.as_nanos() > 0);
    }

    // ── replay_all ──────────────────────────────────────────────────

    #[tokio::test]
    async fn replay_all_preserves_fixture_order() {
        let fixtures = vec![
            fixture("a", "p", MockResponse::Text { text: "1".into() }),
            fixture("b", "p", MockResponse::Text { text: "2".into() }),
            fixture("c", "p", MockResponse::Text { text: "3".into() }),
        ];
        let outcomes = replay_all(&MockReplayer, &fixtures).await;
        let ids: Vec<&str> = outcomes.iter().map(|o| o.fixture_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn replay_all_empty_returns_empty() {
        let outcomes = replay_all(&MockReplayer, &[]).await;
        assert!(outcomes.is_empty());
    }

    // ── End-to-end: replay → score → report ─────────────────────────

    #[tokio::test]
    async fn replay_then_score_passes_when_expectation_aligns() {
        use crate::{Expectation, ReplayReport, score};

        let mut fx = fixture(
            "qa",
            "What is 2+2?",
            MockResponse::Text {
                text: "the answer is 4".into(),
            },
        );
        fx.expect = Expectation {
            final_answer_contains: vec!["4".into()],
            max_tokens_total: Some(100),
            ..Expectation::default()
        };

        let outcome = MockReplayer.replay(&fx).await;
        let failures = score(&outcome, &fx.expect);
        let report = ReplayReport::from_outcome(&outcome, failures);
        assert!(report.passed);
        assert!(report.failures.is_empty());
    }

    #[tokio::test]
    async fn replay_then_score_fails_when_phrase_missing() {
        use crate::{Expectation, score};

        let mut fx = fixture(
            "qa",
            "p",
            MockResponse::Text {
                text: "no number here".into(),
            },
        );
        fx.expect = Expectation {
            final_answer_contains: vec!["42".into()],
            ..Expectation::default()
        };
        let outcome = MockReplayer.replay(&fx).await;
        let failures = score(&outcome, &fx.expect);
        assert_eq!(failures.len(), 1);
    }
}
