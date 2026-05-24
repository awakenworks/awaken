//! Tests for eval_cell shared live-cell helpers.

use super::*;
use awaken_eval::{Expectation, ReplayReport};

#[test]
fn cell_timeout_outcome_reports_as_failed() {
    // Regression: pairing the timeout outcome with `Vec::new()`
    // failures would let `passed = failures.is_empty()` flip true,
    // silently dressing a timed-out cell as a green report. The
    // helper must promote the runtime_failure into a real Failure.
    let expect = Expectation::default();
    let (outcome, failures) = cell_timeout_outcome("fx".into(), 5, &expect);
    assert!(outcome.runtime_failure.is_some());
    assert!(!failures.is_empty(), "timeout must produce failures");
    let report = ReplayReport::from_outcome(&outcome, failures);
    assert!(!report.passed, "timeout cell must report passed=false");
    assert!(
        report
            .failures
            .iter()
            .any(|f| matches!(f, awaken_eval::Failure::ReplayRuntimeFailure { .. })),
        "expected ReplayRuntimeFailure in failures: {:?}",
        report.failures
    );
}

#[test]
fn cell_error_outcome_reports_as_failed() {
    // Per-cell judge/scoring errors must surface as a per-cell
    // failure, never bubble up and discard sibling cells' reports.
    let expect = Expectation::default();
    let outcome = ReplayOutcome {
        fixture_id: "fx".into(),
        final_text: String::new(),
        metrics: awaken_ext_observability::AgentMetrics::default(),
        elapsed: std::time::Duration::ZERO,
        error_type: None,
        inference_error_count: 0,
        runtime_failure: None,
        revision_count: 0,
        judge_score: None,
        judge_reasoning: None,
    };
    let (outcome, failures) =
        cell_error_outcome(outcome, "judge returned non-JSON".into(), &expect);
    let report = ReplayReport::from_outcome(&outcome, failures);
    assert!(!report.passed);
    assert!(report.runtime_failure.is_some());
}

#[test]
fn cell_error_outcome_preserves_real_outcome_data() {
    // Regression: when judge/scoring errors but the replay itself
    // succeeded, the per-cell report must still carry the real
    // `final_text`, token usage, trace run_id, elapsed, and revision
    // count. Rebuilding an empty outcome would (1) fabricate phantom
    // deterministic failures like `AnswerMissingPhrase` for expects
    // the model actually satisfied, (2) zero out tokens that were
    // really burned (breaking cost accounting), and (3) drop the
    // trace link the admin UI uses to explain "why did judge fail".
    use awaken_eval::Failure;
    use awaken_ext_observability::{AgentMetrics, GenAISpan, SpanContext};

    let expect = Expectation {
        final_answer_contains: vec!["42".into()],
        ..Expectation::default()
    };
    let mut inf_span = GenAISpan {
        context: SpanContext {
            run_id: "RUN-REAL".into(),
            ..SpanContext::default()
        },
        step_index: None,
        model: "m".into(),
        provider: "p".into(),
        operation: "chat".into(),
        response_model: None,
        response_id: None,
        finish_reasons: Vec::new(),
        error_type: None,
        error_class: None,
        thinking_tokens: None,
        input_tokens: Some(10),
        output_tokens: Some(20),
        total_tokens: Some(30),
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        temperature: None,
        top_p: None,
        max_tokens: None,
        stop_sequences: Vec::new(),
        duration_ms: 5,
        started_at_ms: 0,
        ended_at_ms: 5,
        response_content: None,
        response_tool_calls: None,
        request_messages: None,
    };
    inf_span.context.run_id = "RUN-REAL".into();
    let metrics = AgentMetrics {
        inferences: vec![inf_span],
        session_duration_ms: 42,
        ..Default::default()
    };
    let real_outcome = ReplayOutcome {
        fixture_id: "fx".into(),
        final_text: "the answer is 42".into(),
        metrics,
        elapsed: std::time::Duration::from_millis(123),
        error_type: None,
        inference_error_count: 0,
        runtime_failure: None,
        revision_count: 2,
        judge_score: None,
        judge_reasoning: None,
    };

    let (outcome, failures) = cell_error_outcome(
        real_outcome,
        "scoring failed: judge timeout".into(),
        &expect,
    );

    // The deterministic expectation `final_answer_contains: ["42"]`
    // was satisfied by the real reply, so scoring must NOT emit a
    // phantom `AnswerMissingPhrase` failure.
    assert!(
        !failures
            .iter()
            .any(|f| matches!(f, Failure::AnswerMissingPhrase { .. })),
        "must not fabricate AnswerMissingPhrase from a blanked final_text: {failures:?}",
    );
    // The runtime failure must be present (drives passed=false).
    assert!(matches!(
        outcome.runtime_failure,
        Some(awaken_eval::ReplayRuntimeFailure::RuntimeError { .. })
    ));

    let report = ReplayReport::from_outcome(&outcome, failures);
    assert!(!report.passed);
    // Real replay observables preserved end-to-end into the report.
    assert_eq!(report.final_text, "the answer is 42");
    assert_eq!(report.total_input_tokens, 10);
    assert_eq!(report.total_output_tokens, 20);
    assert_eq!(report.total_tokens, 30);
    assert_eq!(report.inference_count, 1);
    assert_eq!(report.session_duration_ms, 42);
    assert_eq!(report.elapsed_ms, 123);
    assert_eq!(report.revision_count, 2);
    assert_eq!(outcome.trace_run_id(), Some("RUN-REAL"));
    assert!(report.runtime_failure.is_some());
}

#[test]
fn cell_error_outcome_preserves_existing_runtime_failure() {
    // Regression: when replay itself already set a runtime_failure
    // (e.g. token budget exceeded), a downstream scoring error must
    // NOT overwrite it. The original cause is the load-bearing one
    // for ops triage; the scoring error is downstream noise. Both
    // reasons must reach the per-cell report.
    use awaken_eval::{Failure, ReplayRuntimeFailure};

    let expect = Expectation::default();
    let mut real_outcome = ReplayOutcome {
        fixture_id: "fx".into(),
        final_text: "partial reply".into(),
        metrics: awaken_ext_observability::AgentMetrics::default(),
        elapsed: std::time::Duration::from_millis(50),
        error_type: None,
        inference_error_count: 0,
        runtime_failure: None,
        revision_count: 0,
        judge_score: None,
        judge_reasoning: None,
    };
    real_outcome.runtime_failure = Some(ReplayRuntimeFailure::RuntimeError {
        message: "token budget exceeded".into(),
    });

    let (outcome, failures) = cell_error_outcome(
        real_outcome,
        "scoring failed: judge returned non-JSON".into(),
        &expect,
    );

    // Primary (replay) runtime_failure preserved verbatim — NOT
    // replaced by the scoring error message.
    match &outcome.runtime_failure {
        Some(ReplayRuntimeFailure::RuntimeError { message }) => {
            assert_eq!(message, "token budget exceeded");
        }
        other => panic!("expected preserved RuntimeError, got {other:?}"),
    }

    // Both reasons must be in the failures vec — the replay
    // failure (emitted by score()) and the scoring error
    // (appended by cell_error_outcome).
    let runtime_failure_messages: Vec<String> = failures
        .iter()
        .filter_map(|f| match f {
            Failure::ReplayRuntimeFailure {
                failure: ReplayRuntimeFailure::RuntimeError { message },
            } => Some(message.clone()),
            _ => None,
        })
        .collect();
    assert!(
        runtime_failure_messages
            .iter()
            .any(|m| m == "token budget exceeded"),
        "expected original replay runtime_failure to remain in failures list: {runtime_failure_messages:?}",
    );
    assert!(
        runtime_failure_messages
            .iter()
            .any(|m| m == "scoring failed: judge returned non-JSON"),
        "expected scoring error to be appended to failures list: {runtime_failure_messages:?}",
    );
}

#[tokio::test]
async fn score_outcome_returns_judge_result_so_caller_can_stamp_outcome() {
    // Regression: `score_outcome` used to discard the JudgeResult
    // from `score_with_judge`, so non-revise min_judge_score runs
    // produced a passed/failed report whose `judge_score` /
    // `judge_reasoning` were always `None`. The baseline diff now
    // compares `judge_score`, so this silently masked LLM-grade
    // drift on every comparison.
    use awaken_eval::test_support::ScriptedExecutor;
    use awaken_eval::{Failure, Fixture, LlmExecutorJudge, MockResponse};

    let judge_executor = ScriptedExecutor::new(
        "judge-stub",
        vec![r#"{"score": 0.91, "reasoning": "thorough"}"#],
    )
    .arc();
    let context = JudgeContext {
        judge: LlmExecutorJudge::new(judge_executor, "judge-model"),
        rubric: None,
        revise_max_retries: None,
    };

    let fixture = Fixture {
        id: "fx".into(),
        description: None,
        user_input: "explain it".into(),
        provider_script: vec![],
        provider_script_error: None,
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

    // Outcome arrives at scoring with NO cached judge score — the
    // non-revise path. score_outcome must invoke the judge AND
    // hand the result back so the caller can stamp the outcome.
    let outcome = ReplayOutcome {
        fixture_id: "fx".into(),
        final_text: "agent answer".into(),
        metrics: awaken_ext_observability::AgentMetrics::default(),
        elapsed: std::time::Duration::from_millis(10),
        error_type: None,
        inference_error_count: 0,
        runtime_failure: None,
        revision_count: 0,
        judge_score: None,
        judge_reasoning: None,
    };

    let (failures, judge_result) = score_outcome(&outcome, &fixture, Some(&context))
        .await
        .expect("judge invocation succeeded");
    assert!(
        !failures
            .iter()
            .any(|f| matches!(f, Failure::JudgeBelowThreshold { .. })),
        "0.91 is above threshold; no JudgeBelowThreshold should fire"
    );
    let jr = judge_result.expect("score_outcome must surface the JudgeResult to the caller");
    assert!((jr.score - 0.91).abs() < f32::EPSILON);
    assert_eq!(jr.reasoning.as_deref(), Some("thorough"));

    // Apply the documented caller pattern and verify the report
    // round-trips both fields. Without this stamp the report would
    // be `judge_score: None` even though the judge actually ran.
    let mut outcome = outcome;
    outcome.judge_score = Some(jr.score);
    outcome.judge_reasoning = jr.reasoning;
    let report = ReplayReport::from_outcome(&outcome, failures);
    assert_eq!(report.judge_score, Some(0.91));
    assert_eq!(report.judge_reasoning.as_deref(), Some("thorough"));
    assert!(report.passed, "above threshold and no other failures");
}

#[test]
fn revise_tuple_for_treats_zero_retries_as_disabled() {
    use awaken_eval::LlmExecutorJudge;
    use awaken_eval::test_support::ScriptedExecutor;

    let context = JudgeContext {
        judge: LlmExecutorJudge::new(ScriptedExecutor::new("judge", vec!["{}"]).arc(), "judge"),
        rubric: Some("grade".into()),
        revise_max_retries: Some(0),
    };
    let expect = Expectation {
        min_judge_score: Some(0.8),
        ..Expectation::default()
    };

    assert!(
        revise_tuple_for(Some(&context), &expect).is_none(),
        "0 retries means no replay-phase revise loop; judge remains in scoring phase"
    );
}

#[tokio::test]
async fn zero_retry_judge_timeout_preserves_replay_outcome() {
    use async_trait::async_trait;
    use awaken_contract::contract::content::ContentBlock;
    use awaken_contract::contract::executor::{
        InferenceExecutionError, InferenceRequest, LlmExecutor,
    };
    use awaken_contract::contract::inference::{StopReason, StreamResult, TokenUsage};
    use awaken_contract::registry_spec::ModelSpec;
    use awaken_eval::test_support::ScriptedExecutor;
    use awaken_eval::{Fixture, LlmExecutorJudge, MatrixCell, MockResponse, ReplayRuntimeFailure};

    struct SlowJudgeExecutor;

    #[async_trait]
    impl LlmExecutor for SlowJudgeExecutor {
        async fn execute(
            &self,
            _request: InferenceRequest,
        ) -> Result<StreamResult, InferenceExecutionError> {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            Ok(StreamResult {
                content: vec![ContentBlock::text(r#"{"score":1.0,"reasoning":"late"}"#)],
                tool_calls: vec![],
                usage: Some(TokenUsage::default()),
                stop_reason: Some(StopReason::EndTurn),
                has_incomplete_tool_calls: false,
            })
        }

        fn name(&self) -> &str {
            "slow-judge"
        }
    }

    let fixture = Fixture {
        id: "fx".into(),
        description: None,
        user_input: "say hi".into(),
        provider_script: vec![],
        provider_script_error: None,
        source_run_id: None,
        source_model_id: None,
        allow_unused_provider_script: false,
        mock_response: MockResponse::default(),
        expect: Expectation {
            final_answer_contains: vec!["hello".into()],
            min_judge_score: Some(0.8),
            ..Expectation::default()
        },
        continued_turns: vec![],
    };
    let live_executor = ScriptedExecutor::new("live", vec!["hello"])
        .with_tokens(TokenUsage {
            prompt_tokens: Some(1),
            completion_tokens: Some(1),
            total_tokens: Some(2),
            ..Default::default()
        })
        .arc();
    let resolved = ResolvedCell {
        cell: MatrixCell {
            model_id: Some("m".into()),
        },
        executor: live_executor,
        upstream_model: "upstream".into(),
        spec: ModelSpec::new("m", "provider", "upstream"),
    };
    let judge = JudgeContext {
        judge: LlmExecutorJudge::new(std::sync::Arc::new(SlowJudgeExecutor), "judge-model"),
        rubric: Some("grade correctness".into()),
        revise_max_retries: Some(0),
    };

    let items = run_live_eval_cells(
        &[fixture],
        &[resolved],
        LiveCellOptions {
            samples: 1,
            max_concurrent: 1,
            max_walltime_secs: 1,
            agent_base: None,
            agent_overrides: None,
            judge: Some(judge),
            max_total_tokens: None,
            trace_sink: None,
            trace_store: None,
            task_context: "test cell",
        },
    )
    .await
    .unwrap();

    let report = &items[0].report;
    assert_eq!(report.final_text, "hello");
    assert_eq!(report.total_tokens, 2);
    assert!(!report.passed);
    assert!(matches!(
        report.runtime_failure,
        Some(ReplayRuntimeFailure::RuntimeError { ref message })
            if message.contains("scoring timed out")
    ));
}

#[tokio::test]
async fn run_live_eval_cells_enforces_shared_token_budget() {
    use awaken_contract::contract::inference::TokenUsage;
    use awaken_contract::registry_spec::ModelSpec;
    use awaken_eval::test_support::ScriptedExecutor;
    use awaken_eval::{Fixture, MatrixCell, MockResponse, ReplayRuntimeFailure};

    let fixture = Fixture {
        id: "fx".into(),
        description: None,
        user_input: "say hi".into(),
        provider_script: vec![],
        provider_script_error: None,
        source_run_id: None,
        source_model_id: None,
        allow_unused_provider_script: false,
        mock_response: MockResponse::default(),
        expect: Expectation::default(),
        continued_turns: vec![],
    };
    let executor = ScriptedExecutor::new("live", vec!["hello"])
        .with_tokens(TokenUsage {
            prompt_tokens: Some(3),
            completion_tokens: Some(2),
            total_tokens: Some(5),
            ..Default::default()
        })
        .arc();
    let resolved = ResolvedCell {
        cell: MatrixCell {
            model_id: Some("m".into()),
        },
        executor,
        upstream_model: "upstream".into(),
        spec: ModelSpec::new("m", "provider", "upstream"),
    };

    let items = run_live_eval_cells(
        &[fixture],
        &[resolved],
        LiveCellOptions {
            samples: 1,
            max_concurrent: 1,
            max_walltime_secs: 5,
            agent_base: None,
            agent_overrides: None,
            judge: None,
            max_total_tokens: Some(1),
            trace_sink: None,
            trace_store: None,
            task_context: "test cell",
        },
    )
    .await
    .unwrap();

    assert_eq!(items.len(), 1);
    let report = &items[0].report;
    assert!(!report.passed);
    assert_eq!(report.total_tokens, 5);
    assert!(matches!(
        report.runtime_failure,
        Some(ReplayRuntimeFailure::RuntimeError { ref message })
            if message.contains("token budget exceeded")
    ));
}
