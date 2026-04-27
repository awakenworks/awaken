//! Live TensorZero judge integration test.
//!
//! Boot the gateway via:
//!   ./scripts/e2e-tensorzero.sh
//! and ensure `OPENAI_API_KEY` (or DeepSeek) is exported. Then:
//!   cargo test -p awaken-eval --features llm-judge --test judge_tensorzero -- --ignored

#![cfg(feature = "llm-judge")]

use std::time::Duration;

use awaken_eval::{Expectation, JudgeConfig, ReplayOutcome, TensorZeroJudge, score_with_judge};
use awaken_ext_observability::AgentMetrics;

fn require_gateway_url() -> Option<String> {
    let url = std::env::var("TENSORZERO_GATEWAY_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:3000".to_string());
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .ok()?;
    let health = format!("{}/health", url.trim_end_matches('/'));
    match client.get(&health).send() {
        Ok(resp) if resp.status().is_success() => Some(url),
        _ => {
            eprintln!("[judge-e2e] TensorZero gateway not healthy at {url}; skipping");
            None
        }
    }
}

fn upstream_key_present() -> bool {
    ["DEEPSEEK_API_KEY", "OPENAI_API_KEY"]
        .iter()
        .any(|k| std::env::var(k).is_ok_and(|v| !v.trim().is_empty()))
}

fn make_outcome(text: &str) -> ReplayOutcome {
    ReplayOutcome {
        fixture_id: "judge-int".into(),
        final_text: text.into(),
        metrics: AgentMetrics::default(),
        elapsed: Duration::from_millis(0),
    }
}

#[ignore = "requires running TensorZero stack with judge function + upstream API key"]
#[tokio::test]
async fn judge_returns_score_for_correct_answer() {
    let Some(gateway_url) = require_gateway_url() else {
        return;
    };
    if !upstream_key_present() {
        eprintln!("[judge-e2e] no upstream key set; skipping");
        return;
    }

    let judge = TensorZeroJudge::new(JudgeConfig {
        gateway_url,
        // The judge function in tensorzero.toml uses gpt-4o-mini, which
        // requires OPENAI_API_KEY at the gateway side.
        function_name: "judge".into(),
        submit_feedback: false, // avoid feedback noise during a smoke test
        ..JudgeConfig::default()
    });

    let outcome = make_outcome("4");
    let expect = Expectation {
        min_judge_score: Some(0.5),
        ..Expectation::default()
    };

    let (failures, result) = score_with_judge(&outcome, &expect, "What is 2+2?", None, &judge)
        .await
        .expect("judge call succeeds");
    let result = result.expect("judge result returned");
    assert!(
        (0.0..=1.0).contains(&result.score),
        "score out of range: {}",
        result.score
    );
    // A correct, terse answer should *not* trip the threshold.
    assert!(
        failures.is_empty()
            || failures
                .iter()
                .all(|f| !matches!(f, awaken_eval::Failure::JudgeBelowThreshold { .. })),
        "unexpected judge failure: {failures:?}"
    );
}

#[ignore = "requires running TensorZero stack with judge function + upstream API key"]
#[tokio::test]
async fn judge_flags_obviously_wrong_answer() {
    let Some(gateway_url) = require_gateway_url() else {
        return;
    };
    if !upstream_key_present() {
        eprintln!("[judge-e2e] no upstream key set; skipping");
        return;
    }
    let judge = TensorZeroJudge::new(JudgeConfig {
        gateway_url,
        submit_feedback: false,
        ..JudgeConfig::default()
    });

    let outcome = make_outcome("banana");
    let expect = Expectation {
        min_judge_score: Some(0.7),
        ..Expectation::default()
    };

    let (failures, result) = score_with_judge(
        &outcome,
        &expect,
        "What is 2+2?",
        Some("Score 1.0 only when the answer correctly states 4."),
        &judge,
    )
    .await
    .expect("judge call succeeds");
    let result = result.expect("judge result returned");
    assert!(
        result.score < 0.7,
        "expected low score for wrong answer; got {}",
        result.score
    );
    assert!(
        failures
            .iter()
            .any(|f| matches!(f, awaken_eval::Failure::JudgeBelowThreshold { .. })),
        "expected JudgeBelowThreshold failure, got {failures:?}"
    );
}
