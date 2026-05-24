use super::*;
use awaken_ext_observability::{GenAISpan, SpanContext, ToolSpan};

fn span(input: i32, output: i32) -> GenAISpan {
    GenAISpan {
        context: SpanContext::default(),
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
        input_tokens: Some(input),
        output_tokens: Some(output),
        total_tokens: Some(input + output),
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        temperature: None,
        top_p: None,
        max_tokens: None,
        stop_sequences: Vec::new(),
        duration_ms: 1,
        started_at_ms: 0,
        ended_at_ms: 0,
        response_content: None,
        response_tool_calls: None,
        request_messages: None,
    }
}

fn tool(name: &str, error: bool) -> ToolSpan {
    ToolSpan {
        context: SpanContext::default(),
        step_index: None,
        name: name.into(),
        operation: "execute_tool".into(),
        call_id: format!("call-{name}"),
        tool_type: "function".into(),
        call_arguments: None,
        call_result: None,
        error_type: if error { Some("err".into()) } else { None },
        duration_ms: 1,
        started_at_ms: 0,
        ended_at_ms: 0,
    }
}

fn outcome_with(metrics: AgentMetrics, text: &str) -> ReplayOutcome {
    ReplayOutcome {
        fixture_id: "test".into(),
        final_text: text.into(),
        metrics,
        elapsed: Duration::from_millis(123),
        error_type: None,
        inference_error_count: 0,
        runtime_failure: None,
        revision_count: 0,
        judge_score: None,
        judge_reasoning: None,
    }
}

// ── ReplayOutcome ───────────────────────────────────────────────

fn span_with_total(input: Option<i32>, output: Option<i32>, total: Option<i32>) -> GenAISpan {
    let mut s = span(0, 0);
    s.input_tokens = input;
    s.output_tokens = output;
    s.total_tokens = total;
    s
}

#[test]
fn total_tokens_prefers_span_total_field_when_set() {
    // A provider may report only `total_tokens` (no breakdown).
    // Scoring must see it; otherwise `max_tokens_total: 30` against a
    // 200-token reply trivially passes against 0.
    let metrics = AgentMetrics {
        inferences: vec![span_with_total(None, None, Some(200))],
        ..Default::default()
    };
    let o = outcome_with(metrics, "");
    assert_eq!(o.total_tokens(), 200);
}

#[test]
fn total_tokens_falls_back_to_input_plus_output_when_no_total() {
    let metrics = AgentMetrics {
        inferences: vec![span_with_total(Some(7), Some(3), None)],
        ..Default::default()
    };
    let o = outcome_with(metrics, "");
    assert_eq!(o.total_tokens(), 10);
}

#[test]
fn total_tokens_sums_across_spans_using_total_field() {
    let metrics = AgentMetrics {
        inferences: vec![
            span_with_total(None, None, Some(100)),
            span_with_total(None, None, Some(50)),
        ],
        ..Default::default()
    };
    let o = outcome_with(metrics, "");
    assert_eq!(o.total_tokens(), 150);
}

#[test]
fn total_tokens_mixes_span_total_and_input_output_fallback() {
    // Mixed shape: span 1 only reports `total_tokens`, span 2 only
    // reports `input_tokens + output_tokens`. The earlier
    // implementation returned 20 here (early-exit at first non-zero
    // total) and silently dropped span 2 from the budget.
    let metrics = AgentMetrics {
        inferences: vec![
            span_with_total(None, None, Some(20)),
            span_with_total(Some(100), Some(50), None),
        ],
        ..Default::default()
    };
    let o = outcome_with(metrics, "");
    assert_eq!(o.total_tokens(), 170);
}

#[test]
fn total_tokens_treats_negative_span_values_as_zero() {
    // AgentMetrics permits i32 so a misbehaving provider could
    // report a negative total. Per-span fallback should clamp
    // instead of underflowing or polluting the sum.
    let metrics = AgentMetrics {
        inferences: vec![
            span_with_total(None, None, Some(-5)),
            span_with_total(Some(-7), Some(3), None),
        ],
        ..Default::default()
    };
    let o = outcome_with(metrics, "");
    assert_eq!(o.total_tokens(), 3);
}

#[test]
fn total_tokens_sums_input_and_output() {
    let metrics = AgentMetrics {
        inferences: vec![span(10, 5), span(20, 7)],
        ..Default::default()
    };
    let o = outcome_with(metrics, "");
    assert_eq!(o.total_tokens(), 42);
}

#[test]
fn total_tokens_zero_when_no_inferences() {
    let o = outcome_with(AgentMetrics::default(), "");
    assert_eq!(o.total_tokens(), 0);
}

#[test]
fn trace_run_id_prefers_first_inference_span() {
    // Inference and tool spans both carry run_ids; the inference one
    // wins because it's the primary observable for an LLM call.
    let mut inf = span(1, 1);
    inf.context.run_id = "RUN-INF".into();
    let mut tl = tool("a", false);
    tl.context.run_id = "RUN-TOOL".into();
    let metrics = AgentMetrics {
        inferences: vec![inf],
        tools: vec![tl],
        ..Default::default()
    };
    let o = outcome_with(metrics, "");
    assert_eq!(o.trace_run_id(), Some("RUN-INF"));
}

#[test]
fn trace_run_id_falls_back_to_tool_when_no_inferences() {
    // Failure-path runs may emit only handoff/tool spans before
    // erroring — surface the tool span's run_id so the admin UI
    // can still link back to the trace.
    let mut tl = tool("a", false);
    tl.context.run_id = "RUN-TOOL-ONLY".into();
    let metrics = AgentMetrics {
        inferences: vec![],
        tools: vec![tl],
        ..Default::default()
    };
    let o = outcome_with(metrics, "");
    assert_eq!(o.trace_run_id(), Some("RUN-TOOL-ONLY"));
}

#[test]
fn trace_run_id_none_when_no_spans_emitted() {
    let o = outcome_with(AgentMetrics::default(), "");
    assert!(o.trace_run_id().is_none());
}

#[test]
fn trace_run_id_skips_empty_run_id_on_inference() {
    // A misconfigured plugin could emit a span with an empty
    // run_id. Fall through to the next candidate rather than
    // returning Some("") which would yield a broken trace link.
    let inf = span(1, 1); // default SpanContext → empty run_id
    let mut tl = tool("a", false);
    tl.context.run_id = "RUN-T".into();
    let metrics = AgentMetrics {
        inferences: vec![inf],
        tools: vec![tl],
        ..Default::default()
    };
    let o = outcome_with(metrics, "");
    assert_eq!(o.trace_run_id(), Some("RUN-T"));
}

#[test]
fn tool_sequence_preserves_record_order() {
    let metrics = AgentMetrics {
        tools: vec![tool("a", false), tool("b", false), tool("a", false)],
        ..Default::default()
    };
    let o = outcome_with(metrics, "");
    assert_eq!(o.tool_sequence(), vec!["a", "b", "a"]);
}

#[test]
fn tool_sequence_empty_when_no_tools() {
    let o = outcome_with(AgentMetrics::default(), "");
    assert!(o.tool_sequence().is_empty());
}

// ── ReplayReport ────────────────────────────────────────────────

#[test]
fn report_passes_when_failures_empty() {
    let o = outcome_with(AgentMetrics::default(), "ok");
    let r = ReplayReport::from_outcome(&o, Vec::new());
    assert!(r.passed);
    assert!(r.failures.is_empty());
    assert_eq!(r.fixture_id, "test");
    assert_eq!(r.final_text, "ok");
    assert_eq!(r.elapsed_ms, 123);
}

#[test]
fn report_fails_when_any_failure_present() {
    let o = outcome_with(AgentMetrics::default(), "");
    let r = ReplayReport::from_outcome(
        &o,
        vec![Failure::AnswerMissingPhrase { phrase: "x".into() }],
    );
    assert!(!r.passed);
    assert_eq!(r.failures.len(), 1);
}

#[test]
fn report_aggregates_metrics_correctly() {
    let metrics = AgentMetrics {
        inferences: vec![span(10, 5), span(20, 10)],
        tools: vec![tool("a", false), tool("b", true), tool("c", false)],
        session_duration_ms: 9999,
        ..Default::default()
    };
    let o = outcome_with(metrics, "yo");
    let r = ReplayReport::from_outcome(&o, Vec::new());
    assert_eq!(r.inference_count, 2);
    assert_eq!(r.tool_count, 3);
    assert_eq!(r.tool_failures, 1);
    assert_eq!(r.total_input_tokens, 30);
    assert_eq!(r.total_output_tokens, 15);
    assert_eq!(r.session_duration_ms, 9999);
}

#[test]
fn report_token_breakdowns_clamp_negative_values_per_span() {
    let metrics = AgentMetrics {
        inferences: vec![
            span_with_total(Some(-10), Some(2), None),
            span_with_total(Some(20), Some(-5), None),
        ],
        ..Default::default()
    };
    let o = outcome_with(metrics, "yo");
    let r = ReplayReport::from_outcome(&o, Vec::new());
    assert_eq!(r.total_input_tokens, 20);
    assert_eq!(r.total_output_tokens, 2);
    assert_eq!(r.total_tokens, 22);
}

#[test]
fn report_serde_roundtrip() {
    let o = outcome_with(AgentMetrics::default(), "answer");
    let mut r = ReplayReport::from_outcome(
        &o,
        vec![Failure::TokenBudgetExceeded {
            budget: 100,
            actual: 200,
        }],
    );
    let json = serde_json::to_string(&r).unwrap();
    let parsed: ReplayReport = serde_json::from_str(&json).unwrap();
    // `elapsed_ms` is intentionally not serialised, so it round-trips
    // back as 0 (its `#[serde(default)]`).
    r.elapsed_ms = 0;
    assert_eq!(parsed, r);
}

#[test]
fn report_elapsed_ms_saturates_at_u64_max() {
    let o = ReplayOutcome {
        fixture_id: "saturate".into(),
        final_text: String::new(),
        metrics: AgentMetrics::default(),
        elapsed: Duration::from_secs(u64::MAX / 1000),
        error_type: None,
        inference_error_count: 0,
        runtime_failure: None,
        revision_count: 0,
        judge_score: None,
        judge_reasoning: None,
    };
    let r = ReplayReport::from_outcome(&o, Vec::new());
    // We just need to confirm it doesn't panic; exact value depends on
    // platform but must be finite (u64).
    let _ = r.elapsed_ms;
}

#[test]
fn report_omits_elapsed_ms_from_serialised_form() {
    // elapsed_ms varies per-host, so it must not pollute the committed
    // baseline. The in-memory field stays for tooling that needs it.
    let o = outcome_with(AgentMetrics::default(), "");
    let r = ReplayReport::from_outcome(&o, Vec::new());
    let json = serde_json::to_string(&r).unwrap();
    assert!(!json.contains("elapsed_ms"));
    // session_duration_ms remains so duration assertions stay
    // observable in the baseline.
    assert!(json.contains("session_duration_ms"));
}

#[test]
fn report_round_trips_error_type_through_serde() {
    let o = ReplayOutcome {
        fixture_id: "err".into(),
        final_text: String::new(),
        metrics: AgentMetrics::default(),
        elapsed: Duration::from_millis(0),
        error_type: Some("rate_limit".into()),
        inference_error_count: 1,
        runtime_failure: None,
        revision_count: 0,
        judge_score: None,
        judge_reasoning: None,
    };
    let r = ReplayReport::from_outcome(&o, Vec::new());
    let json = serde_json::to_string(&r).unwrap();
    assert!(json.contains(r#""error_type":"rate_limit""#));
    let parsed: ReplayReport = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.error_type.as_deref(), Some("rate_limit"));
}

#[test]
fn report_inference_error_count_round_trips_when_non_zero() {
    let o = ReplayOutcome {
        fixture_id: "err".into(),
        final_text: String::new(),
        metrics: AgentMetrics::default(),
        elapsed: Duration::from_millis(0),
        error_type: Some("rate_limit".into()),
        inference_error_count: 2,
        runtime_failure: None,
        revision_count: 0,
        judge_score: None,
        judge_reasoning: None,
    };
    let r = ReplayReport::from_outcome(&o, Vec::new());
    let json = serde_json::to_string(&r).unwrap();
    assert!(json.contains(r#""inference_error_count":2"#));
    let parsed: ReplayReport = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.inference_error_count, 2);
}

#[test]
fn report_omits_inference_error_count_when_zero() {
    let o = outcome_with(AgentMetrics::default(), "");
    let r = ReplayReport::from_outcome(&o, Vec::new());
    let json = serde_json::to_string(&r).unwrap();
    assert!(!json.contains("inference_error_count"));
}

#[test]
fn report_runtime_failure_round_trips_script_exhausted() {
    let o = ReplayOutcome {
        fixture_id: "exhausted".into(),
        final_text: String::new(),
        metrics: AgentMetrics::default(),
        elapsed: Duration::from_millis(0),
        error_type: Some("rate_limit".into()),
        inference_error_count: 1,
        runtime_failure: Some(ReplayRuntimeFailure::ScriptExhausted { extra_calls: 2 }),
        revision_count: 0,
        judge_score: None,
        judge_reasoning: None,
    };
    let r = ReplayReport::from_outcome(&o, Vec::new());
    let json = serde_json::to_string(&r).unwrap();
    assert!(json.contains(r#""runtime_failure":{"kind":"script_exhausted","extra_calls":2}"#));
    let parsed: ReplayReport = serde_json::from_str(&json).unwrap();
    assert_eq!(
        parsed.runtime_failure,
        Some(ReplayRuntimeFailure::ScriptExhausted { extra_calls: 2 })
    );
}

#[test]
fn report_omits_error_type_when_none() {
    let o = outcome_with(AgentMetrics::default(), "");
    let r = ReplayReport::from_outcome(&o, Vec::new());
    let json = serde_json::to_string(&r).unwrap();
    assert!(!json.contains("error_type"));
}

// ── tool_calls_by_agent ────────────────────────────────────────

fn tool_for(agent_id: &str, name: &str, error: bool) -> awaken_ext_observability::ToolSpan {
    awaken_ext_observability::ToolSpan {
        context: awaken_ext_observability::SpanContext {
            run_id: "r".into(),
            thread_id: "t".into(),
            agent_id: agent_id.into(),
            parent_run_id: None,
            parent_tool_call_id: None,
            ..Default::default()
        },
        step_index: None,
        name: name.into(),
        operation: "execute_tool".into(),
        call_id: format!("call-{name}-{agent_id}"),
        tool_type: "function".into(),
        call_arguments: None,
        call_result: None,
        error_type: if error { Some("err".into()) } else { None },
        duration_ms: 1,
        started_at_ms: 0,
        ended_at_ms: 0,
    }
}

#[test]
fn report_tool_calls_by_agent_empty_when_no_tools() {
    let o = outcome_with(AgentMetrics::default(), "");
    let r = ReplayReport::from_outcome(&o, Vec::new());
    assert!(r.tool_calls_by_agent.is_empty());
}

#[test]
fn report_tool_calls_by_agent_aggregates_per_pair() {
    let metrics = AgentMetrics {
        tools: vec![
            tool_for("planner", "search", false),
            tool_for("planner", "search", true),
            tool_for("worker", "search", false),
            tool_for("worker", "write", false),
        ],
        ..Default::default()
    };
    let o = outcome_with(metrics, "");
    let r = ReplayReport::from_outcome(&o, Vec::new());
    assert_eq!(r.tool_calls_by_agent.len(), 3);
    let planner_search = r
        .tool_calls_by_agent
        .iter()
        .find(|s| s.agent_id == "planner" && s.tool == "search")
        .unwrap();
    assert_eq!(planner_search.call_count, 2);
    assert_eq!(planner_search.failure_count, 1);
}

#[test]
fn report_serde_with_tool_calls_by_agent_roundtrips() {
    let metrics = AgentMetrics {
        tools: vec![tool_for("a", "search", false)],
        ..Default::default()
    };
    let o = outcome_with(metrics, "ok");
    let mut r = ReplayReport::from_outcome(&o, Vec::new());
    let json = serde_json::to_string(&r).unwrap();
    assert!(json.contains(r#""tool_calls_by_agent""#));
    let parsed: ReplayReport = serde_json::from_str(&json).unwrap();
    r.elapsed_ms = 0;
    assert_eq!(parsed, r);
}

#[test]
fn report_serde_omits_field_when_empty() {
    let o = outcome_with(AgentMetrics::default(), "");
    let r = ReplayReport::from_outcome(&o, Vec::new());
    let json = serde_json::to_string(&r).unwrap();
    // skip_serializing_if = "Vec::is_empty" must keep older baselines
    // exactly the same shape they had pre-M9.2.
    assert!(!json.contains("tool_calls_by_agent"));
}

#[test]
fn report_deserializes_legacy_ndjson_without_field() {
    // Pre-M9.2 baseline line. Must round-trip via deserialise +
    // re-serialise without losing fields or panicking.
    let legacy = r#"{
            "fixture_id": "legacy",
            "passed": true,
            "failures": [],
            "final_text": "ok",
            "inference_count": 1,
            "tool_count": 0,
            "tool_failures": 0,
            "total_input_tokens": 0,
            "total_output_tokens": 0,
            "session_duration_ms": 0,
            "elapsed_ms": 0
        }"#;
    let parsed: ReplayReport = serde_json::from_str(legacy).unwrap();
    assert_eq!(parsed.fixture_id, "legacy");
    assert!(parsed.tool_calls_by_agent.is_empty());
    // total_tokens is `#[serde(default)]` — legacy lines without it
    // must still parse and default to 0.
    assert_eq!(parsed.total_tokens, 0);
    assert_eq!(parsed.inference_error_count, 0);
    assert!(parsed.runtime_failure.is_none());
}
