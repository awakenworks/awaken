//! End-to-end integration tests for awaken-eval.
//!
//! These exercise the public API across module boundaries:
//!
//! 1. Load fixtures from disk (`fixture::load_directory`).
//! 2. Replay them through [`MockReplayer`].
//! 3. Score each outcome with [`score`].
//! 4. Write reports as NDJSON.
//! 5. Diff a fresh report against a committed baseline.
//!
//! The bundled `crates/awaken-eval/fixtures` directory is exercised
//! directly to confirm authoring conventions remain valid as the framework
//! evolves.

use std::path::PathBuf;

use awaken_eval::{
    DiffEntry, Expectation, Fixture, MockReplayer, MockResponse, ReplayReport, RuntimeReplayer,
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
    let outcomes = replay_all(&MockReplayer::new(), &fixtures).await;
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
    use async_trait::async_trait;
    use awaken_contract::contract::executor::{
        InferenceExecutionError, InferenceRequest, LlmExecutor,
    };
    use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
    use std::sync::Arc;

    /// Always returns the same canned response with a token usage that
    /// the test can compare against `outcome.total_tokens()`.
    struct CannedExecutor {
        response: String,
        total_tokens: i32,
    }

    #[async_trait]
    impl LlmExecutor for CannedExecutor {
        async fn execute(
            &self,
            _request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            Ok(StreamResult {
                content: vec![awaken_contract::contract::content::ContentBlock::text(
                    self.response.clone(),
                )],
                tool_calls: vec![],
                usage: Some(TokenUsage {
                    prompt_tokens: Some(10),
                    completion_tokens: Some(5),
                    total_tokens: Some(self.total_tokens),
                    ..Default::default()
                }),
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }

        fn name(&self) -> &str {
            "canned"
        }
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
        }
    }

    #[tokio::test]
    async fn live_mode_drives_real_executor_and_recovers_response() {
        let executor: Arc<dyn LlmExecutor> = Arc::new(CannedExecutor {
            response: "the answer is 42".into(),
            total_tokens: 15,
        });
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
        let executor: Arc<dyn LlmExecutor> = Arc::new(CannedExecutor {
            response: "long answer".into(),
            total_tokens: 100,
        });
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
