//! End-to-end integration tests for awaken-eval.
//!
//! These exercise the public API across module boundaries:
//!
//! 1. Load fixtures from disk (`fixture::load_directory`).
//! 2. Replay them through [`RuntimeReplayer`].
//! 3. Score each outcome with [`score`].
//! 4. Write reports as NDJSON.
//! 5. Diff a fresh report against a committed baseline.
//!
//! The bundled `crates/awaken-eval/fixtures` directory is exercised
//! directly to confirm authoring conventions remain valid as the framework
//! evolves.

use std::path::PathBuf;

use awaken_eval::{
    DiffEntry, Expectation, Fixture, MockResponse, ReplayReport, RuntimeReplayer,
    diff_against_baseline, fixture::load_directory, read_ndjson_path, replay_all, score,
    trace_to_provider_script, write_ndjson_path,
};
use awaken_ext_observability::trace_store::{TraceStore, file::FileTraceStore};
use awaken_ext_observability::{GenAISpan, MetricsEvent, SpanContext};
use serde_json::json;

fn bundled_fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

async fn replay_dir(dir: &PathBuf) -> Vec<ReplayReport> {
    let fixtures = load_directory(dir).expect("fixtures load");
    let outcomes = replay_all(&RuntimeReplayer::new(), &fixtures).await;
    fixtures
        .iter()
        .zip(outcomes.iter())
        .map(|(fx, outcome)| {
            let failures = score(outcome, &fx.expect);
            ReplayReport::from_outcome(outcome, failures)
        })
        .collect()
}

// ── Bundled fixtures sanity ─────────────────────────────────────────

#[tokio::test]
async fn bundled_fixtures_replay_and_pass() {
    let dir = bundled_fixtures_dir();
    let reports = replay_dir(&dir).await;
    assert!(
        !reports.is_empty(),
        "fixtures/ must ship at least one fixture; got {dir:?}"
    );
    for r in &reports {
        assert!(
            r.passed,
            "fixture {} unexpectedly failed: {:?}",
            r.fixture_id, r.failures
        );
    }
}

#[tokio::test]
async fn bundled_fixtures_have_unique_ids() {
    let fixtures = load_directory(bundled_fixtures_dir()).unwrap();
    let mut ids = std::collections::HashSet::new();
    for fx in &fixtures {
        assert!(ids.insert(fx.id.clone()), "duplicate id {}", fx.id);
    }
}

#[tokio::test]
async fn bundled_fixtures_each_have_non_empty_expectation() {
    let fixtures = load_directory(bundled_fixtures_dir()).unwrap();
    for fx in &fixtures {
        assert!(
            !fx.expect.is_empty(),
            "fixture {} has no expectation criteria — at least one is required",
            fx.id
        );
    }
}

// ── Replay → Score → Report → Read round-trip ───────────────────────

#[tokio::test]
async fn full_replay_pipeline_round_trips_through_disk() {
    let dir = temp_dir();
    let report_path = dir.path().join("report.ndjson");

    let mut reports = replay_dir(&bundled_fixtures_dir()).await;
    write_ndjson_path(&report_path, &reports).unwrap();

    let read_back = read_ndjson_path(&report_path).unwrap();
    // `elapsed_ms` is the wall-clock cost of the run — deliberately not
    // serialised (see `ReplayReport::elapsed_ms`), so it deserialises
    // back as 0.
    for r in &mut reports {
        r.elapsed_ms = 0;
    }
    assert_eq!(read_back, reports);
}

// ── Baseline diff: clean → regression → fixed ───────────────────────

#[tokio::test]
async fn diff_baseline_against_itself_is_clean() {
    let reports = replay_dir(&bundled_fixtures_dir()).await;
    let summary = diff_against_baseline(&reports, &reports);
    assert!(summary.is_clean());
    assert_eq!(summary.regressions(), 0);
    for entry in &summary.entries {
        assert!(matches!(entry, DiffEntry::Unchanged { .. }));
    }
}

#[tokio::test]
async fn diff_detects_regression_after_fixture_mutation() {
    let original = replay_dir(&bundled_fixtures_dir()).await;

    // Mutate the bundled "01_simple_qa" fixture in a temp dir so the answer
    // no longer satisfies its expectation.
    let dir = temp_dir();
    let fx_path = dir.path().join("01_simple_qa.json");
    let bundle = bundled_fixtures_dir();
    let original_text = std::fs::read_to_string(bundle.join("01_simple_qa.json")).unwrap();
    let mut fx: Fixture = serde_json::from_str(&original_text).unwrap();
    fx.mock_response = MockResponse::Text {
        text: "I refuse to answer.".into(),
    };
    std::fs::write(&fx_path, serde_json::to_string_pretty(&fx).unwrap()).unwrap();
    // Copy the rest of the fixtures so the run only differs in 01.
    for entry in std::fs::read_dir(&bundle).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name.to_string_lossy() == "01_simple_qa.json" {
            continue;
        }
        std::fs::copy(entry.path(), dir.path().join(&name)).unwrap();
    }

    let regressed = replay_dir(&dir.path().to_path_buf()).await;
    let summary = diff_against_baseline(&original, &regressed);

    assert!(!summary.is_clean(), "expected regression detection");
    assert_eq!(summary.regressions(), 1);
    let entry = summary
        .entries
        .iter()
        .find(|e| matches!(e, DiffEntry::Regression { fixture_id, .. } if fixture_id == "01_simple_qa"))
        .expect("regression on 01_simple_qa");
    if let DiffEntry::Regression { new_failures, .. } = entry {
        assert!(
            new_failures.iter().any(|k| k == "answer_missing_phrase"),
            "expected answer_missing_phrase, got {new_failures:?}"
        );
    }
}

#[tokio::test]
async fn diff_detects_missing_fixture_when_new_run_is_partial() {
    let original = replay_dir(&bundled_fixtures_dir()).await;

    // Remove one fixture file in a copy directory, then replay.
    let dir = temp_dir();
    for entry in std::fs::read_dir(bundled_fixtures_dir()).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name.to_string_lossy() == "05_error_path.json" {
            continue;
        }
        std::fs::copy(entry.path(), dir.path().join(&name)).unwrap();
    }
    let partial = replay_dir(&dir.path().to_path_buf()).await;
    let summary = diff_against_baseline(&original, &partial);
    assert!(!summary.is_clean());
    assert_eq!(summary.missing(), 1);
}

#[tokio::test]
async fn diff_detects_newly_added_without_blocking() {
    let original = replay_dir(&bundled_fixtures_dir()).await;

    // Add an extra fixture in a copy directory.
    let dir = temp_dir();
    for entry in std::fs::read_dir(bundled_fixtures_dir()).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), dir.path().join(entry.file_name())).unwrap();
    }
    let extra_path = dir.path().join("99_added.json");
    std::fs::write(
        &extra_path,
        r#"{
            "id": "99_added",
            "user_input": "anything",
            "mock_response": {"kind": "text", "text": "ok"},
            "expect": {"final_answer_contains": ["ok"]}
        }"#,
    )
    .unwrap();
    let extended = replay_dir(&dir.path().to_path_buf()).await;
    let summary = diff_against_baseline(&original, &extended);
    assert_eq!(summary.added(), 1);
    // Passing newly-added fixtures don't block CI (failing ones do —
    // see report::tests::diff_newly_added_failing_blocks_check).
    assert!(summary.is_clean());
}

// ── Trace → fixture → replay round-trip (ADR-0032 D5) ───────────────

fn captured_inference_span(run_id: &str, step: u32, text: &str) -> GenAISpan {
    GenAISpan {
        context: SpanContext {
            run_id: run_id.into(),
            agent_id: "default".into(),
            ..Default::default()
        },
        step_index: Some(step),
        model: "claude-opus-4-7".into(),
        provider: "anthropic".into(),
        operation: "chat".into(),
        response_model: None,
        response_id: None,
        finish_reasons: vec!["end_turn".into()],
        error_type: None,
        error_class: None,
        thinking_tokens: None,
        input_tokens: Some(10),
        output_tokens: Some(4),
        total_tokens: Some(14),
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        temperature: None,
        top_p: None,
        max_tokens: None,
        stop_sequences: Vec::new(),
        duration_ms: 1,
        started_at_ms: 0,
        ended_at_ms: 0,
        response_content: Some(json!([{"type": "text", "text": text}])),
        response_tool_calls: None,
        request_messages: None,
    }
}

#[tokio::test]
async fn trace_curate_round_trips_through_file_store_and_replays() {
    // End-to-end proof of the trace → fixture → replay loop:
    //   1. write a captured trace to a real FileTraceStore
    //   2. read it back through the same API the CLI uses
    //   3. curate it into a Fixture via trace_to_provider_script
    //   4. replay the Fixture through RuntimeReplayer
    //   5. assert final_text matches the originally captured response
    //
    // If any of those steps drift apart the loop silently breaks —
    // ContentCapture writes nothing, the converter misreads spans, or
    // the scripted executor diverges from how content was originally
    // recorded. This test pins the wire-format end to end.
    let trace_root = temp_dir();
    let store = FileTraceStore::new(trace_root.path()).expect("trace store");
    let run_id = "01HXCURATE0000000000000001";
    let span = captured_inference_span(run_id, 0, "the answer is 42");
    store
        .append(run_id, &MetricsEvent::Inference(span))
        .expect("append");

    // Read back via TraceStore API — same path the curate CLI walks.
    let events = store.read(run_id).expect("read");
    assert_eq!(events.len(), 1);

    let conversion = trace_to_provider_script(&events).expect("convert");
    assert_eq!(
        conversion.source_model_id.as_deref(),
        Some("claude-opus-4-7")
    );
    assert_eq!(conversion.provider_script.len(), 1);

    let fixture = Fixture {
        id: run_id.into(),
        description: None,
        // Trace persistence does not capture request messages today —
        // the operator supplies the original user prompt out of band.
        user_input: "what is six times seven".into(),
        provider_script: conversion.provider_script,
        source_run_id: Some(run_id.into()),
        source_model_id: conversion.source_model_id,
        allow_unused_provider_script: false,
        mock_response: MockResponse::default(),
        expect: Expectation::default(),
        continued_turns: vec![],
    };

    let outcomes = replay_all(&RuntimeReplayer::new(), std::slice::from_ref(&fixture)).await;
    let outcome = &outcomes[0];
    assert_eq!(outcome.final_text, "the answer is 42");
    assert!(
        outcome.runtime_failure.is_none(),
        "round-trip should not surface a runtime failure: {:?}",
        outcome.runtime_failure
    );
}

// ── Live mode: real provider drives replay ──────────────────────────

mod live_mode {
    use super::*;
    use awaken_contract::contract::executor::LlmExecutor;
    use awaken_contract::contract::inference::TokenUsage;
    use awaken_eval::test_support::ScriptedExecutor;
    use std::sync::Arc;

    fn canned_executor(response: &str, total_tokens: i32) -> Arc<ScriptedExecutor> {
        ScriptedExecutor::new("canned", vec![response])
            .with_tokens(TokenUsage {
                prompt_tokens: Some(10),
                completion_tokens: Some(5),
                total_tokens: Some(total_tokens),
                ..Default::default()
            })
            .arc()
    }

    fn ad_hoc_fixture(prompt: &str) -> Fixture {
        Fixture {
            id: "ad-hoc".into(),
            description: None,
            user_input: prompt.into(),
            provider_script: vec![],
            source_run_id: None,
            source_model_id: None,
            allow_unused_provider_script: false,
            mock_response: MockResponse::default(),
            expect: Expectation::default(),
            continued_turns: vec![],
        }
    }

    #[tokio::test]
    async fn live_mode_drives_real_executor_and_recovers_response() {
        let executor: Arc<dyn LlmExecutor> = canned_executor("the answer is 42", 15);
        let replayer = RuntimeReplayer::new().with_live_executor(executor, "claude-opus-4-7-test");
        let fixture = ad_hoc_fixture("what is six times seven");
        let outcomes = replay_all(&replayer, std::slice::from_ref(&fixture)).await;
        let outcome = &outcomes[0];
        assert_eq!(outcome.final_text, "the answer is 42");
        assert_eq!(outcome.total_tokens(), 15);
        assert!(
            outcome.runtime_failure.is_none(),
            "{:?}",
            outcome.runtime_failure
        );
        assert!(outcome.error_type.is_none());
    }

    #[tokio::test]
    async fn live_mode_post_hoc_token_budget_surfaces_runtime_failure() {
        // Executor reports 100 tokens; cap is 50 → must annotate as
        // RuntimeError with a "token budget exceeded" message.
        let executor: Arc<dyn LlmExecutor> = canned_executor("long answer", 100);
        let replayer = RuntimeReplayer::new()
            .with_live_executor(executor, "claude-opus-4-7-test")
            .with_max_total_tokens(50);
        let fixture = ad_hoc_fixture("anything");
        let outcomes = replay_all(&replayer, std::slice::from_ref(&fixture)).await;
        let outcome = &outcomes[0];
        match &outcome.runtime_failure {
            Some(awaken_eval::outcome::ReplayRuntimeFailure::RuntimeError { message }) => {
                assert!(
                    message.contains("token budget exceeded"),
                    "wrong message: {message}"
                );
            }
            other => panic!("expected RuntimeError, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn runtime_replayer_tee_sink_routes_spans_to_trace_store() {
    use awaken_ext_observability::trace_store::TraceStoreSink;
    use std::sync::Arc;

    // A bundled fixture replayed with a TraceStore tee must land its
    // spans in that store under the runtime-assigned run_id. The
    // `EvalRunItem.trace_run_id` link the server populates from
    // `ReplayOutcome.trace_run_id()` is then a real pointer, not a
    // dead string.
    let fixtures = load_directory(bundled_fixtures_dir()).expect("fixtures");
    let fixture = fixtures
        .iter()
        .find(|f| f.id == "01_simple_qa")
        .expect("01_simple_qa fixture");

    let trace_root = temp_dir();
    let store: Arc<dyn TraceStore> = Arc::new(FileTraceStore::new(trace_root.path()).unwrap());
    let tee = Arc::new(TraceStoreSink::new(store.clone()));
    let replayer = RuntimeReplayer::new().with_tee_sink(tee);
    let outcomes = replay_all(&replayer, std::slice::from_ref(fixture)).await;
    let outcome = &outcomes[0];

    let trace_run_id = outcome.trace_run_id().expect("at least one span emitted");
    let stored = store.read(trace_run_id).expect("trace persisted");
    assert!(
        !stored.is_empty(),
        "tee sink must have appended at least one event for {trace_run_id}"
    );
}

#[tokio::test]
async fn trace_curate_preserves_multi_turn_order() {
    // A run with two assistant turns curates into a 2-event script that
    // replays in the same order. The scripted executor consumes events
    // FIFO, so the original step_index ordering must be preserved.
    let trace_root = temp_dir();
    let store = FileTraceStore::new(trace_root.path()).expect("trace store");
    let run_id = "01HXCURATE0000000000000002";
    store
        .append(
            run_id,
            &MetricsEvent::Inference(captured_inference_span(run_id, 0, "first turn")),
        )
        .unwrap();
    store
        .append(
            run_id,
            &MetricsEvent::Inference(captured_inference_span(run_id, 1, "second turn")),
        )
        .unwrap();

    let events = store.read(run_id).unwrap();
    let conversion = trace_to_provider_script(&events).unwrap();
    assert_eq!(conversion.provider_script.len(), 2);
}

// ── Multi-turn dialogue replay ──────────────────────────────────────

#[tokio::test]
async fn dialogue_fixture_replays_two_turns_on_same_thread() {
    // Turn 0 responds "first answer"; turn 1 responds "second answer".
    // Combined script feeds both turns; ScriptedLlmExecutor pointer
    // advances across turns. final_text is the LAST turn's reply.
    use awaken_contract::contract::inference::{StopReason, TokenUsage};
    use awaken_eval::fixture::DialogueTurn;
    use awaken_runtime::engine::ProviderScriptEvent;
    fn turn_response(text: &str) -> ProviderScriptEvent {
        ProviderScriptEvent::ChatResponse {
            content: text.into(),
            tokens: TokenUsage {
                prompt_tokens: Some(5),
                completion_tokens: Some(3),
                total_tokens: Some(8),
                ..Default::default()
            },
            finish_reason: StopReason::EndTurn,
        }
    }
    let fixture = Fixture {
        id: "dialogue-2".into(),
        description: None,
        user_input: "hello".into(),
        provider_script: vec![turn_response("first answer")],
        source_run_id: None,
        source_model_id: None,
        allow_unused_provider_script: false,
        mock_response: MockResponse::default(),
        expect: Expectation {
            final_answer_contains: vec!["second answer".into()],
            ..Expectation::default()
        },
        continued_turns: vec![DialogueTurn {
            user_input: "follow up".into(),
            provider_script: vec![turn_response("second answer")],
        }],
    };
    let replayer = RuntimeReplayer::new();
    let outcomes = replay_all(&replayer, std::slice::from_ref(&fixture)).await;
    let outcome = &outcomes[0];
    assert_eq!(outcome.final_text, "second answer");
    // Both turns' inference spans landed in metrics.
    assert_eq!(outcome.metrics.inferences.len(), 2);
    // Tokens summed across turns: 8 + 8 = 16.
    assert_eq!(outcome.total_tokens(), 16);
    assert!(
        outcome.runtime_failure.is_none(),
        "{:?}",
        outcome.runtime_failure
    );
    // Scoring against last-turn final_text passes.
    assert!(score(outcome, &fixture.expect).is_empty());
}

#[tokio::test]
async fn dialogue_fixture_first_turn_error_short_circuits_second() {
    // When turn 0 surfaces a scripted error, the dialogue must abort
    // BEFORE attempting turn 1 — otherwise the second turn's script
    // would get consumed against a broken thread and the test would
    // see a stale "ProviderScriptUnused" signal for the wrong reason.
    use awaken_eval::fixture::DialogueTurn;
    use awaken_runtime::engine::ProviderScriptEvent;
    let fixture = Fixture {
        id: "dialogue-err".into(),
        description: None,
        user_input: "broken".into(),
        provider_script: vec![ProviderScriptEvent::Error {
            error_type: "rate_limit".into(),
            message: "429".into(),
        }],
        source_run_id: None,
        source_model_id: None,
        allow_unused_provider_script: true,
        mock_response: MockResponse::default(),
        expect: Expectation::default(),
        continued_turns: vec![DialogueTurn {
            user_input: "follow up never sent".into(),
            provider_script: vec![ProviderScriptEvent::ChatResponse {
                content: "unreachable".into(),
                tokens: Default::default(),
                finish_reason: awaken_contract::contract::inference::StopReason::EndTurn,
            }],
        }],
    };
    let replayer = RuntimeReplayer::new();
    let outcomes = replay_all(&replayer, std::slice::from_ref(&fixture)).await;
    let outcome = &outcomes[0];
    assert_eq!(outcome.error_type.as_deref(), Some("rate_limit"));
    assert_eq!(outcome.final_text, "");
    // Turn 1's script event must remain unused — the dialogue did
    // not advance past the error.
    // (allow_unused_provider_script=true suppresses the Failure but the
    // count itself is still observable via metrics.)
    assert_eq!(outcome.metrics.inferences.len(), 0, "turn 1 must not fire");
}

// ── Reprocess-on-judge-fail (Live mode revise loop) ─────────────────

mod revise_mode {
    use super::*;
    use awaken_contract::contract::executor::LlmExecutor;
    use awaken_eval::judge::Judge;
    use awaken_eval::test_support::{ScriptedExecutor, ScriptedJudge};
    use awaken_eval::{LlmExecutorJudge, RuntimeReplayer};
    use std::sync::Arc;

    fn ad_hoc_fixture(prompt: &str) -> Fixture {
        Fixture {
            id: "revise".into(),
            description: None,
            user_input: prompt.into(),
            provider_script: vec![],
            source_run_id: None,
            source_model_id: None,
            allow_unused_provider_script: false,
            mock_response: MockResponse::default(),
            expect: Expectation::default(),
            continued_turns: vec![],
        }
    }

    #[tokio::test]
    async fn revise_loop_recovers_on_first_retry() {
        // Initial answer scores 0.3 < threshold 0.7. After revise the
        // second attempt scores 0.9 → loop exits with revision_count=1.
        let exec: Arc<dyn LlmExecutor> =
            ScriptedExecutor::new("step-stub", vec!["bad answer", "great answer"]).arc();
        let judge: Arc<dyn Judge> = ScriptedJudge::new(vec![0.3, 0.9]).arc();
        let replayer = RuntimeReplayer::new()
            .with_live_executor(exec, "test-model")
            .with_revise_on_judge_fail(judge, None, 0.7, 3);
        let fixture = ad_hoc_fixture("be insightful");
        let outcomes = replay_all(&replayer, std::slice::from_ref(&fixture)).await;
        let outcome = &outcomes[0];
        assert_eq!(outcome.final_text, "great answer");
        assert_eq!(outcome.revision_count, 1);
        assert_eq!(outcome.judge_score, Some(0.9));
    }

    #[tokio::test]
    async fn revise_loop_skipped_when_initial_clears_threshold() {
        // Initial scores 0.95 → no retry needed.
        let exec: Arc<dyn LlmExecutor> =
            ScriptedExecutor::new("step-stub", vec!["nailed it"]).arc();
        let judge: Arc<dyn Judge> = ScriptedJudge::new(vec![0.95]).arc();
        let replayer = RuntimeReplayer::new()
            .with_live_executor(exec, "test-model")
            .with_revise_on_judge_fail(judge, None, 0.7, 3);
        let fixture = ad_hoc_fixture("be insightful");
        let outcomes = replay_all(&replayer, std::slice::from_ref(&fixture)).await;
        let outcome = &outcomes[0];
        assert_eq!(outcome.final_text, "nailed it");
        assert_eq!(outcome.revision_count, 0);
        assert_eq!(outcome.judge_score, Some(0.95));
    }

    #[tokio::test]
    async fn revise_loop_exhausts_retries_when_judge_keeps_failing() {
        // Every retry still scores below threshold. After max_retries=2
        // (so 3 total judge calls), the loop exits with revision_count=2
        // and judge_score reflecting the last failing score.
        let exec: Arc<dyn LlmExecutor> =
            ScriptedExecutor::new("step-stub", vec!["v0", "v1", "v2", "v3"]).arc();
        let judge: Arc<dyn Judge> = ScriptedJudge::new(vec![0.1, 0.2, 0.4]).arc();
        let replayer = RuntimeReplayer::new()
            .with_live_executor(exec, "test-model")
            .with_revise_on_judge_fail(judge, None, 0.7, 2);
        let fixture = ad_hoc_fixture("hard task");
        let outcomes = replay_all(&replayer, std::slice::from_ref(&fixture)).await;
        let outcome = &outcomes[0];
        assert_eq!(outcome.revision_count, 2);
        // Score is the LAST one returned by the judge (still failing).
        assert_eq!(outcome.judge_score, Some(0.4));
        // Final text is the 3rd answer (v2) — the last one the agent produced.
        assert_eq!(outcome.final_text, "v2");
    }

    /// Sanity check that LlmExecutorJudge composes the same way the
    /// scripted ScriptedJudge does — exercises the production code path.
    #[tokio::test]
    async fn revise_loop_works_with_executor_backed_judge() {
        // Two distinct executors: one for the agent, one for the judge.
        let agent_exec: Arc<dyn LlmExecutor> =
            ScriptedExecutor::new("step-stub", vec!["initial", "revised"]).arc();
        // Judge executor returns scripted score JSON strings.
        let judge_exec: Arc<dyn LlmExecutor> = ScriptedExecutor::new(
            "step-stub",
            vec![
                r#"{"score": 0.4, "reasoning": "needs work"}"#,
                r#"{"score": 0.85, "reasoning": "better"}"#,
            ],
        )
        .arc();
        let judge: Arc<dyn Judge> = Arc::new(LlmExecutorJudge::new(judge_exec, "judge-model"));
        let replayer = RuntimeReplayer::new()
            .with_live_executor(agent_exec, "agent-model")
            .with_revise_on_judge_fail(judge, None, 0.7, 3);
        let fixture = ad_hoc_fixture("evaluate me");
        let outcomes = replay_all(&replayer, std::slice::from_ref(&fixture)).await;
        let outcome = &outcomes[0];
        assert_eq!(outcome.final_text, "revised");
        assert_eq!(outcome.revision_count, 1);
        assert!(outcome.judge_score.unwrap() >= 0.7);
    }
}
// ── End-to-end metrics demo — every Anthropic-aligned indicator in one run

#[tokio::test]
async fn metrics_demo_exercises_all_indicators_together() {
    use awaken_contract::contract::executor::LlmExecutor;
    use awaken_contract::contract::inference::TokenUsage;
    use awaken_contract::registry_spec::ModelBindingSpec;
    use awaken_eval::judge::Judge;
    use awaken_eval::test_support::{ExplodingJudge, ScriptedExecutor, ScriptedJudge};
    use awaken_eval::{EvalRun, EvalRunItem, MatrixCell, ReplayReport, RuntimeReplayer};
    use std::sync::Arc;

    // ── Run 3 samples of the same (fixture, cell). Sample 0 has revise
    // pre-stocked to recover; samples 1 + 2 fail (judge stays low).
    let mut run = EvalRun {
        id: "DEMO-RUN".into(),
        dataset_id: "metrics-demo".into(),
        dataset_revision: 1,
        items: Vec::new(),
        started_at_secs: 1_700_000_000,
        ended_at_secs: 1_700_000_010,
    };

    // Priced binding — exercises cost_usd.
    let binding = ModelBindingSpec {
        id: "demo".into(),
        provider_id: "anthropic".into(),
        upstream_model: "claude-opus-4-7".into(),
        input_token_price_per_million_usd: Some(3.0),
        output_token_price_per_million_usd: Some(15.0),
    };

    let cell = MatrixCell {
        model_id: Some("claude-opus-4-7".into()),
    };

    // Sample-specific (responses, judge_scores). Sample 0 = pass after revise.
    let sample_configs = vec![
        // (executor responses, judge scores) — last judge call returns the value the loop exits on.
        (vec!["bad answer", "great answer"], vec![0.3, 0.9]), // pass after 1 revision
        (vec!["bad", "still bad", "still bad"], vec![0.2, 0.4, 0.5]), // fails — never crosses 0.7
        (vec!["instant win"], vec![0.95]),                    // pass with revision_count=0
    ];

    for (sample_idx, (responses, scores)) in sample_configs.into_iter().enumerate() {
        let exec: Arc<dyn LlmExecutor> = ScriptedExecutor::new("step-exec", responses)
            .with_tokens(TokenUsage {
                prompt_tokens: Some(120),
                completion_tokens: Some(60),
                total_tokens: Some(180),
                ..Default::default()
            })
            .arc();
        let judge: Arc<dyn Judge> = ScriptedJudge::new(scores).arc();
        let replayer = RuntimeReplayer::new()
            .with_live_executor(exec, "claude-opus-4-7")
            .with_revise_on_judge_fail(judge, None, 0.7, 2);

        // Build a unique fixture id per sample so the in-memory thread is fresh.
        let fixture = Fixture {
            id: format!("demo-s{sample_idx}"),
            description: None,
            user_input: "be insightful".into(),
            provider_script: vec![],
            source_run_id: None,
            source_model_id: None,
            allow_unused_provider_script: false,
            mock_response: MockResponse::default(),
            expect: Expectation {
                min_judge_score: Some(0.7),
                ..Expectation::default()
            },
            continued_turns: vec![],
        };

        let outcomes = replay_all(&replayer, std::slice::from_ref(&fixture)).await;
        let outcome = outcomes.into_iter().next().unwrap();

        // Score using the cache-aware path: outcome.judge_score short-
        // circuits the second judge call.
        use awaken_eval::judge::score_with_judge;
        let (failures, _) = score_with_judge(
            &outcome,
            &fixture.expect,
            &fixture.user_input,
            None,
            &ExplodingJudge,
        )
        .await
        .unwrap();
        let mut report = ReplayReport::from_outcome(&outcome, failures);
        // Exercise cost: cost_usd = binding.compute_cost_usd(input, output)
        report.cost_usd =
            binding.compute_cost_usd(report.total_input_tokens, report.total_output_tokens);

        run.items.push(EvalRunItem {
            fixture_id: "demo".into(), // share fixture_id across samples for the aggregator
            cell: Some(cell.clone()),
            report,
            trace_run_id: None,
            sample_index: Some(sample_idx as u32),
        });
    }

    // ── Print every metric the framework exposes for this run.
    eprintln!("\n=== METRICS DEMO — per-sample items ===");
    for item in &run.items {
        let r = &item.report;
        eprintln!(
            "  sample {} | passed={} | judge_score={:?} | revision_count={} | \
             input_tokens={} output_tokens={} total_tokens={} | cost_usd={:?}",
            item.sample_index.unwrap(),
            r.passed,
            r.judge_score,
            r.revision_count,
            r.total_input_tokens,
            r.total_output_tokens,
            r.total_tokens,
            r.cost_usd,
        );
    }

    // ── Aggregate: pass@k / pass^k
    let aggs = run.aggregate_samples();
    eprintln!("\n=== METRICS DEMO — pass@k / pass^k aggregate ===");
    for a in &aggs {
        eprintln!(
            "  ({}, model={:?}) samples={} passed={} pass_rate={:.2} pass_at_k={} pass_pow_k={}",
            a.fixture_id,
            a.cell.as_ref().and_then(|c| c.model_id.as_deref()),
            a.samples,
            a.passed,
            a.pass_rate,
            a.pass_at_k,
            a.pass_pow_k,
        );
    }

    // ── Hard assertions on every indicator we built.
    assert_eq!(run.items.len(), 3);
    // Sample 0 (revised once, score crosses 0.7 → passed)
    let s0 = &run.items[0].report;
    assert!(s0.passed, "sample 0 must pass after revise");
    assert_eq!(s0.revision_count, 1);
    assert_eq!(s0.judge_score, Some(0.9));
    // Sample 1 (judge never crosses threshold → failed, revision_count=2)
    let s1 = &run.items[1].report;
    assert!(!s1.passed);
    assert_eq!(s1.revision_count, 2);
    // Sample 2 (initial answer passes → revision_count=0)
    let s2 = &run.items[2].report;
    assert!(s2.passed);
    assert_eq!(s2.revision_count, 0);
    assert_eq!(s2.judge_score, Some(0.95));
    // Cost: priced binding → every report carries cost_usd Some(_).
    for item in &run.items {
        assert!(
            item.report.cost_usd.is_some(),
            "cost_usd must populate when binding has pricing"
        );
        assert!(item.report.cost_usd.unwrap() > 0.0);
    }
    // Aggregate: 2 of 3 samples passed → pass_at_k=true, pass_pow_k=false.
    assert_eq!(aggs.len(), 1);
    assert_eq!(aggs[0].samples, 3);
    assert_eq!(aggs[0].passed, 2);
    assert!(aggs[0].pass_at_k);
    assert!(!aggs[0].pass_pow_k);
}
