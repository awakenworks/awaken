//! Replay engine.
//!
//! `awaken-eval` decouples *how* a fixture is replayed from *what* the
//! framework does with the resulting outcome. The [`Replayer`] trait is the
//! seam: implementations may run a fixture against a real `AgentRuntime`
//! (see [`crate::runtime_replayer::RuntimeReplayer`]), against a recorded
//! transcript, or against any other backend.
//!
//! ## Bundled implementation
//!
//! [`MockReplayer`] now delegates to [`RuntimeReplayer`] so its outcomes
//! come from a real agent loop driven by a
//! [`ScriptedLlmExecutor`](awaken_runtime::engine::ScriptedLlmExecutor)
//! over the fixture's `provider_script`. The historical name is kept as a
//! one-line shim for downstream callers and will be removed once they
//! migrate to [`RuntimeReplayer`] directly (ADR-0032 D8).

use async_trait::async_trait;

use crate::fixture::Fixture;
use crate::outcome::ReplayOutcome;
use crate::runtime_replayer::RuntimeReplayer;

/// Run a fixture and return its raw outcome.
#[async_trait]
pub trait Replayer: Send + Sync {
    async fn replay(&self, fixture: &Fixture) -> ReplayOutcome;
}

/// Compat alias for the bundled replayer. Delegates to
/// [`RuntimeReplayer`].
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
        RuntimeReplayer::new().replay(fixture).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expectation::Expectation;
    use crate::fixture::{Fixture, MockResponse};

    fn fixture(id: &str, prompt: &str, mock: MockResponse) -> Fixture {
        Fixture {
            id: id.into(),
            description: None,
            user_input: prompt.into(),
            provider_script: Vec::new(),
            source_run_id: None,
            source_model_id: None,
            allow_unused_provider_script: false,
            mock_response: mock,
            expect: Expectation::default(),
            continued_turns: vec![],
        }
    }

    #[tokio::test]
    async fn mock_replayer_text_response_round_trips_fixture_id_and_text() {
        let fx = fixture(
            "answer",
            "What is 2+2?",
            MockResponse::Text {
                text: "the answer is 4".into(),
            },
        );
        let outcome = MockReplayer::new().replay(&fx).await;
        assert_eq!(outcome.fixture_id, "answer");
        assert!(outcome.final_text.contains("the answer is 4"));
        assert_eq!(outcome.metrics.inference_count(), 1);
        assert_eq!(outcome.metrics.tool_count(), 0);
    }

    #[tokio::test]
    async fn mock_replayer_session_duration_is_finite() {
        let fx = fixture("duration", "p", MockResponse::Text { text: "ans".into() });
        let outcome = MockReplayer.replay(&fx).await;
        assert!(outcome.elapsed.as_nanos() > 0);
    }

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
