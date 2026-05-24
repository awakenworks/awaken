//! Replay-engine trait + slice helper.
//!
//! `awaken-eval` decouples *how* a fixture is replayed from *what* the
//! framework does with the resulting outcome. The [`Replayer`] trait is
//! the seam: bundled impls run a fixture against a real `AgentRuntime`
//! (see [`crate::runtime_replayer::RuntimeReplayer`]); embedders can plug
//! in alternate backends (recorded transcripts, network replayers, etc).

use async_trait::async_trait;

use crate::fixture::Fixture;
use crate::outcome::ReplayOutcome;

/// Run a fixture and return its raw outcome.
#[async_trait]
pub trait Replayer: Send + Sync {
    async fn replay(&self, fixture: &Fixture) -> ReplayOutcome;
}

/// Replay a slice of fixtures through `replayer`, preserving input order.
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
    use crate::runtime_replayer::RuntimeReplayer;

    fn fixture(id: &str, prompt: &str, mock: MockResponse) -> Fixture {
        Fixture {
            id: id.into(),
            description: None,
            user_input: prompt.into(),
            provider_script: Vec::new(),
            provider_script_error: None,
            source_run_id: None,
            source_model_id: None,
            allow_unused_provider_script: false,
            mock_response: mock,
            expect: Expectation::default(),
            continued_turns: vec![],
        }
    }

    #[tokio::test]
    async fn replay_all_preserves_fixture_order() {
        let fixtures = vec![
            fixture("a", "p", MockResponse::Text { text: "1".into() }),
            fixture("b", "p", MockResponse::Text { text: "2".into() }),
            fixture("c", "p", MockResponse::Text { text: "3".into() }),
        ];
        let outcomes = replay_all(&RuntimeReplayer::new(), &fixtures).await;
        let ids: Vec<&str> = outcomes.iter().map(|o| o.fixture_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[tokio::test]
    async fn replay_all_empty_returns_empty() {
        let outcomes = replay_all(&RuntimeReplayer::new(), &[]).await;
        assert!(outcomes.is_empty());
    }
}
