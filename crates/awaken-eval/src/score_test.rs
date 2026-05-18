use super::*;
use crate::outcome::ReplayOutcome;
use awaken_ext_observability::{AgentMetrics, GenAISpan, SpanContext, ToolSpan};
use std::time::Duration;

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

fn tool(name: &str) -> ToolSpan {
    ToolSpan {
        context: SpanContext::default(),
        step_index: None,
        name: name.into(),
        operation: "execute_tool".into(),
        call_id: format!("call-{name}"),
        tool_type: "function".into(),
        call_arguments: None,
        call_result: None,
        error_type: None,
        duration_ms: 1,
        started_at_ms: 0,
        ended_at_ms: 0,
    }
}

fn tool_with_args(name: &str, args: serde_json::Value) -> ToolSpan {
    let mut t = tool(name);
    t.call_arguments = Some(args);
    t
}

fn tool_with_args_and_result(
    name: &str,
    args: serde_json::Value,
    result: serde_json::Value,
) -> ToolSpan {
    let mut t = tool_with_args(name, args);
    t.call_result = Some(result);
    t
}

fn outcome(metrics: AgentMetrics, text: &str) -> ReplayOutcome {
    ReplayOutcome {
        fixture_id: "test".into(),
        final_text: text.into(),
        metrics,
        elapsed: Duration::from_millis(0),
        error_type: None,
        inference_error_count: 0,
        runtime_failure: None,
        revision_count: 0,
        judge_score: None,
    }
}

// ── Empty expectation ───────────────────────────────────────────

#[test]
fn empty_expectation_passes_anything() {
    let o = outcome(
        AgentMetrics {
            inferences: vec![span(1000, 1000)],
            tools: vec![tool("any")],
            session_duration_ms: 999_999,
            ..Default::default()
        },
        "anything goes",
    );
    assert!(score(&o, &Expectation::default()).is_empty());
}

// ── final_answer_contains ───────────────────────────────────────

#[test]
fn answer_contains_pass_when_phrase_present() {
    let o = outcome(AgentMetrics::default(), "the answer is 42");
    let expect = Expectation {
        final_answer_contains: vec!["42".into()],
        ..Expectation::default()
    };
    assert!(score(&o, &expect).is_empty());
}

#[test]
fn answer_contains_fails_when_phrase_absent() {
    let o = outcome(AgentMetrics::default(), "no number");
    let expect = Expectation {
        final_answer_contains: vec!["42".into()],
        ..Expectation::default()
    };
    let failures = score(&o, &expect);
    assert_eq!(failures.len(), 1);
    match &failures[0] {
        Failure::AnswerMissingPhrase { phrase } => assert_eq!(phrase, "42"),
        other => panic!("unexpected failure: {other:?}"),
    }
}

#[test]
fn answer_contains_reports_one_failure_per_missing_phrase() {
    let o = outcome(AgentMetrics::default(), "alpha");
    let expect = Expectation {
        final_answer_contains: vec!["alpha".into(), "beta".into(), "gamma".into()],
        ..Expectation::default()
    };
    let failures = score(&o, &expect);
    assert_eq!(failures.len(), 2);
    let phrases: Vec<&str> = failures
        .iter()
        .filter_map(|f| match f {
            Failure::AnswerMissingPhrase { phrase } => Some(phrase.as_str()),
            _ => None,
        })
        .collect();
    assert!(phrases.contains(&"beta"));
    assert!(phrases.contains(&"gamma"));
}

// ── final_answer_excludes ───────────────────────────────────────

#[test]
fn answer_excludes_fails_when_phrase_present() {
    let o = outcome(AgentMetrics::default(), "leaked secret token");
    let expect = Expectation {
        final_answer_excludes: vec!["secret".into()],
        ..Expectation::default()
    };
    let failures = score(&o, &expect);
    assert!(matches!(
        failures.as_slice(),
        [Failure::AnswerContainsExcludedPhrase { phrase }] if phrase == "secret"
    ));
}

#[test]
fn answer_excludes_passes_when_clean() {
    let o = outcome(AgentMetrics::default(), "all good");
    let expect = Expectation {
        final_answer_excludes: vec!["bad".into()],
        ..Expectation::default()
    };
    assert!(score(&o, &expect).is_empty());
}

// ── tool_sequence ───────────────────────────────────────────────

#[test]
fn tool_sequence_pass_when_match() {
    let o = outcome(
        AgentMetrics {
            tools: vec![tool("search"), tool("write")],
            ..Default::default()
        },
        "",
    );
    let expect = Expectation {
        tool_sequence: vec!["search".into(), "write".into()],
        ..Expectation::default()
    };
    assert!(score(&o, &expect).is_empty());
}

#[test]
fn tool_sequence_fail_when_order_wrong() {
    let o = outcome(
        AgentMetrics {
            tools: vec![tool("write"), tool("search")],
            ..Default::default()
        },
        "",
    );
    let expect = Expectation {
        tool_sequence: vec!["search".into(), "write".into()],
        ..Expectation::default()
    };
    let failures = score(&o, &expect);
    assert!(matches!(
        failures.as_slice(),
        [Failure::ToolSequenceMismatch { expected, actual, mode: ToolSequenceMode::Exact }]
            if expected == &["search".to_string(), "write".to_string()]
                && actual == &["write".to_string(), "search".to_string()]
    ));
}

#[test]
fn tool_sequence_fail_when_missing() {
    let o = outcome(AgentMetrics::default(), "");
    let expect = Expectation {
        tool_sequence: vec!["needed".into()],
        ..Expectation::default()
    };
    assert_eq!(score(&o, &expect).len(), 1);
}

#[test]
fn tool_sequence_no_constraint_when_empty() {
    let o = outcome(
        AgentMetrics {
            tools: vec![tool("anything")],
            ..Default::default()
        },
        "",
    );
    assert!(score(&o, &Expectation::default()).is_empty());
}

// ── forbidden_tools ─────────────────────────────────────────────

#[test]
fn forbidden_tools_fail_per_invocation() {
    let o = outcome(
        AgentMetrics {
            tools: vec![tool("rm"), tool("ok"), tool("drop")],
            ..Default::default()
        },
        "",
    );
    let expect = Expectation {
        forbidden_tools: vec!["rm".into(), "drop".into()],
        ..Expectation::default()
    };
    let failures = score(&o, &expect);
    assert_eq!(failures.len(), 2);
    assert!(
        failures
            .iter()
            .all(|f| matches!(f, Failure::ForbiddenToolUsed { .. }))
    );
}

#[test]
fn forbidden_tools_pass_when_unused() {
    let o = outcome(
        AgentMetrics {
            tools: vec![tool("safe")],
            ..Default::default()
        },
        "",
    );
    let expect = Expectation {
        forbidden_tools: vec!["rm".into()],
        ..Expectation::default()
    };
    assert!(score(&o, &expect).is_empty());
}

// ── max_tokens_total ────────────────────────────────────────────

#[test]
fn token_budget_pass_when_within() {
    let o = outcome(
        AgentMetrics {
            inferences: vec![span(50, 50)],
            ..Default::default()
        },
        "",
    );
    let expect = Expectation {
        max_tokens_total: Some(200),
        ..Expectation::default()
    };
    assert!(score(&o, &expect).is_empty());
}

#[test]
fn token_budget_fail_when_exceeded() {
    let o = outcome(
        AgentMetrics {
            inferences: vec![span(150, 150)],
            ..Default::default()
        },
        "",
    );
    let expect = Expectation {
        max_tokens_total: Some(200),
        ..Expectation::default()
    };
    let failures = score(&o, &expect);
    assert!(matches!(
        failures.as_slice(),
        [Failure::TokenBudgetExceeded {
            budget: 200,
            actual: 300
        }]
    ));
}

#[test]
fn token_budget_boundary_inclusive() {
    let o = outcome(
        AgentMetrics {
            inferences: vec![span(100, 100)],
            ..Default::default()
        },
        "",
    );
    let expect = Expectation {
        max_tokens_total: Some(200),
        ..Expectation::default()
    };
    // 200 == budget, must not fail.
    assert!(score(&o, &expect).is_empty());
}

// ── max_duration_ms ─────────────────────────────────────────────

#[test]
fn duration_pass_when_within() {
    let o = outcome(
        AgentMetrics {
            session_duration_ms: 500,
            ..Default::default()
        },
        "",
    );
    let expect = Expectation {
        max_duration_ms: Some(1000),
        ..Expectation::default()
    };
    assert!(score(&o, &expect).is_empty());
}

#[test]
fn duration_fail_when_exceeded() {
    let o = outcome(
        AgentMetrics {
            session_duration_ms: 1500,
            ..Default::default()
        },
        "",
    );
    let expect = Expectation {
        max_duration_ms: Some(1000),
        ..Expectation::default()
    };
    let failures = score(&o, &expect);
    assert!(matches!(
        failures.as_slice(),
        [Failure::DurationExceeded {
            budget_ms: 1000,
            actual_ms: 1500
        }]
    ));
}

// ── multi-criterion combinations ────────────────────────────────

#[test]
fn multiple_criteria_report_all_failures() {
    let o = outcome(
        AgentMetrics {
            inferences: vec![span(500, 500)],
            tools: vec![tool("rm")],
            session_duration_ms: 5000,
            ..Default::default()
        },
        "missing",
    );
    let expect = Expectation {
        final_answer_contains: vec!["banana".into()],
        tool_sequence: vec!["read".into()],
        forbidden_tools: vec!["rm".into()],
        max_tokens_total: Some(100),
        max_duration_ms: Some(1000),
        ..Expectation::default()
    };
    let failures = score(&o, &expect);
    assert!(
        failures.len() >= 5,
        "got {} failures: {:?}",
        failures.len(),
        failures
    );
    let kinds: std::collections::HashSet<&str> = failures.iter().map(Failure::kind).collect();
    for required in [
        "answer_missing_phrase",
        "tool_sequence_mismatch",
        "forbidden_tool_used",
        "token_budget_exceeded",
        "duration_exceeded",
    ] {
        assert!(kinds.contains(required), "missing failure kind {required}");
    }
}

// ── expected_error_type ─────────────────────────────────────────

#[test]
fn expected_error_type_pass_when_match() {
    let mut o = outcome(AgentMetrics::default(), "");
    o.error_type = Some("rate_limit".into());
    let expect = Expectation {
        expected_error_type: Some("rate_limit".into()),
        ..Expectation::default()
    };
    assert!(score(&o, &expect).is_empty());
}

#[test]
fn expected_error_type_fail_when_run_succeeded() {
    // Run produced no error_type — silently passing was the original
    // 05_error_path bug. Make sure scoring catches it now.
    let o = outcome(AgentMetrics::default(), "");
    let expect = Expectation {
        expected_error_type: Some("rate_limit".into()),
        ..Expectation::default()
    };
    let failures = score(&o, &expect);
    assert!(matches!(
        failures.as_slice(),
        [Failure::ExpectedErrorMissing { expected }] if expected == "rate_limit"
    ));
}

#[test]
fn expected_error_type_fail_when_kind_differs() {
    let mut o = outcome(AgentMetrics::default(), "");
    o.error_type = Some("timeout".into());
    let expect = Expectation {
        expected_error_type: Some("rate_limit".into()),
        ..Expectation::default()
    };
    let failures = score(&o, &expect);
    assert!(matches!(
        failures.as_slice(),
        [Failure::ErrorTypeMismatch { expected, actual }]
            if expected == "rate_limit" && actual == "timeout"
    ));
}

// ── runtime_failure ─────────────────────────────────────────────

#[test]
fn runtime_failure_is_always_emitted_as_failure() {
    use crate::outcome::ReplayRuntimeFailure;
    let mut o = outcome(AgentMetrics::default(), "");
    o.runtime_failure = Some(ReplayRuntimeFailure::ScriptExhausted { extra_calls: 1 });
    let failures = score(&o, &Expectation::default());
    assert!(matches!(
        failures.as_slice(),
        [Failure::ReplayRuntimeFailure {
            failure: ReplayRuntimeFailure::ScriptExhausted { extra_calls: 1 }
        }]
    ));
}

#[test]
fn runtime_failure_does_not_short_circuit_other_failures() {
    use crate::outcome::ReplayRuntimeFailure;
    let mut o = outcome(AgentMetrics::default(), "no match");
    o.runtime_failure = Some(ReplayRuntimeFailure::ProviderScriptUnused { remaining: 2 });
    let expect = Expectation {
        final_answer_contains: vec!["banana".into()],
        ..Expectation::default()
    };
    let failures = score(&o, &expect);
    assert_eq!(failures.len(), 2);
    let kinds: Vec<&str> = failures.iter().map(Failure::kind).collect();
    assert!(kinds.contains(&"answer_missing_phrase"));
    assert!(kinds.contains(&"replay_runtime_failure"));
}

// ── ordered subsequence mode ────────────────────────────────────

#[test]
fn tool_sequence_ordered_subseq_pass_with_intermediate_calls() {
    let o = outcome(
        AgentMetrics {
            tools: vec![tool("plan"), tool("search"), tool("think"), tool("write")],
            ..Default::default()
        },
        "",
    );
    let expect = Expectation {
        tool_sequence: vec!["search".into(), "write".into()],
        tool_sequence_mode: ToolSequenceMode::OrderedSubseq,
        ..Expectation::default()
    };
    assert!(score(&o, &expect).is_empty());
}

#[test]
fn tool_sequence_ordered_subseq_fail_when_out_of_order() {
    let o = outcome(
        AgentMetrics {
            tools: vec![tool("write"), tool("search")],
            ..Default::default()
        },
        "",
    );
    let expect = Expectation {
        tool_sequence: vec!["search".into(), "write".into()],
        tool_sequence_mode: ToolSequenceMode::OrderedSubseq,
        ..Expectation::default()
    };
    let failures = score(&o, &expect);
    assert!(matches!(
        failures.as_slice(),
        [Failure::ToolSequenceMismatch {
            mode: ToolSequenceMode::OrderedSubseq,
            ..
        }]
    ));
}

#[test]
fn tool_sequence_ordered_subseq_fail_when_required_call_missing() {
    let o = outcome(
        AgentMetrics {
            tools: vec![tool("search")],
            ..Default::default()
        },
        "",
    );
    let expect = Expectation {
        tool_sequence: vec!["search".into(), "write".into()],
        tool_sequence_mode: ToolSequenceMode::OrderedSubseq,
        ..Expectation::default()
    };
    assert_eq!(score(&o, &expect).len(), 1);
}

// ── required_tool_calls ─────────────────────────────────────────

#[test]
fn required_tool_calls_name_only_passes_when_invoked() {
    use crate::expectation::ToolCallMatcher;
    let o = outcome(
        AgentMetrics {
            tools: vec![tool("search"), tool("write")],
            ..Default::default()
        },
        "",
    );
    let expect = Expectation {
        required_tool_calls: vec![ToolCallMatcher {
            name: "search".into(),
            args_subset: None,
            result_subset: None,
            count: None,
        }],
        ..Expectation::default()
    };
    assert!(score(&o, &expect).is_empty());
}

#[test]
fn required_tool_calls_name_only_fails_when_not_invoked() {
    use crate::expectation::ToolCallMatcher;
    let o = outcome(
        AgentMetrics {
            tools: vec![tool("write")],
            ..Default::default()
        },
        "",
    );
    let expect = Expectation {
        required_tool_calls: vec![ToolCallMatcher {
            name: "search".into(),
            args_subset: None,
            result_subset: None,
            count: None,
        }],
        ..Expectation::default()
    };
    let failures = score(&o, &expect);
    assert!(matches!(
        failures.as_slice(),
        [Failure::RequiredToolCallNotSatisfied {
            name,
            actual_matches: 0,
            args_filter_set: false,
            ..
        }] if name == "search"
    ));
}

#[test]
fn required_tool_calls_args_subset_matches_object() {
    use crate::expectation::ToolCallMatcher;
    let o = outcome(
        AgentMetrics {
            tools: vec![tool_with_args(
                "search",
                serde_json::json!({"query": "banana", "limit": 10}),
            )],
            ..Default::default()
        },
        "",
    );
    let expect = Expectation {
        required_tool_calls: vec![ToolCallMatcher {
            name: "search".into(),
            args_subset: Some(serde_json::json!({"query": "banana"})),
            result_subset: None,
            count: None,
        }],
        ..Expectation::default()
    };
    assert!(score(&o, &expect).is_empty());
}

#[test]
fn required_tool_calls_args_subset_fails_when_value_differs() {
    use crate::expectation::ToolCallMatcher;
    let o = outcome(
        AgentMetrics {
            tools: vec![tool_with_args(
                "search",
                serde_json::json!({"query": "apple"}),
            )],
            ..Default::default()
        },
        "",
    );
    let expect = Expectation {
        required_tool_calls: vec![ToolCallMatcher {
            name: "search".into(),
            args_subset: Some(serde_json::json!({"query": "banana"})),
            result_subset: None,
            count: None,
        }],
        ..Expectation::default()
    };
    let failures = score(&o, &expect);
    assert!(matches!(
        failures.as_slice(),
        [Failure::RequiredToolCallNotSatisfied {
            actual_matches: 0,
            args_filter_set: true,
            ..
        }]
    ));
}

#[test]
fn required_tool_calls_args_subset_fails_when_call_args_not_captured() {
    // Capture policy left args unrecorded; matcher with args_subset
    // can't verify and therefore counts the call as non-matching.
    use crate::expectation::ToolCallMatcher;
    let o = outcome(
        AgentMetrics {
            tools: vec![tool("search")],
            ..Default::default()
        },
        "",
    );
    let expect = Expectation {
        required_tool_calls: vec![ToolCallMatcher {
            name: "search".into(),
            args_subset: Some(serde_json::json!({"query": "x"})),
            result_subset: None,
            count: None,
        }],
        ..Expectation::default()
    };
    assert_eq!(score(&o, &expect).len(), 1);
}

#[test]
fn required_tool_calls_count_exactly_enforced() {
    use crate::expectation::{CountConstraint, ToolCallMatcher};
    let o = outcome(
        AgentMetrics {
            tools: vec![tool("search"), tool("search"), tool("search")],
            ..Default::default()
        },
        "",
    );
    let expect = Expectation {
        required_tool_calls: vec![ToolCallMatcher {
            name: "search".into(),
            args_subset: None,
            result_subset: None,
            count: Some(CountConstraint::Exactly(2)),
        }],
        ..Expectation::default()
    };
    let failures = score(&o, &expect);
    assert!(matches!(
        failures.as_slice(),
        [Failure::RequiredToolCallNotSatisfied {
            actual_matches: 3,
            expected_count: Some(CountConstraint::Exactly(2)),
            ..
        }]
    ));
}

#[test]
fn required_tool_calls_count_at_most_zero_with_args_filter_acts_as_forbidden() {
    // Use case: "the agent may call `rm`, but never with --force".
    use crate::expectation::{CountConstraint, ToolCallMatcher};
    let o = outcome(
        AgentMetrics {
            tools: vec![tool_with_args("rm", serde_json::json!({"force": true}))],
            ..Default::default()
        },
        "",
    );
    let expect = Expectation {
        required_tool_calls: vec![ToolCallMatcher {
            name: "rm".into(),
            args_subset: Some(serde_json::json!({"force": true})),
            result_subset: None,
            count: Some(CountConstraint::AtMost(0)),
        }],
        ..Expectation::default()
    };
    let failures = score(&o, &expect);
    assert!(matches!(
        failures.as_slice(),
        [Failure::RequiredToolCallNotSatisfied {
            actual_matches: 1,
            expected_count: Some(CountConstraint::AtMost(0)),
            args_filter_set: true,
            ..
        }]
    ));
}

// ── required_tool_calls: result_subset (outcome-state matcher) ──

#[test]
fn required_tool_calls_result_subset_matches_object() {
    use crate::expectation::ToolCallMatcher;
    let o = outcome(
        AgentMetrics {
            tools: vec![tool_with_args_and_result(
                "search",
                serde_json::json!({"query": "banana"}),
                serde_json::json!({"hits": [{"title": "Banana", "score": 0.9}], "count": 1}),
            )],
            ..Default::default()
        },
        "",
    );
    let expect = Expectation {
        required_tool_calls: vec![ToolCallMatcher {
            name: "search".into(),
            args_subset: None,
            result_subset: Some(serde_json::json!({"count": 1})),
            count: None,
        }],
        ..Expectation::default()
    };
    assert!(score(&o, &expect).is_empty());
}

#[test]
fn required_tool_calls_result_subset_fails_when_result_mismatch() {
    use crate::expectation::ToolCallMatcher;
    let o = outcome(
        AgentMetrics {
            tools: vec![tool_with_args_and_result(
                "search",
                serde_json::json!({"query": "banana"}),
                serde_json::json!({"count": 0}),
            )],
            ..Default::default()
        },
        "",
    );
    let expect = Expectation {
        required_tool_calls: vec![ToolCallMatcher {
            name: "search".into(),
            args_subset: None,
            result_subset: Some(serde_json::json!({"count": 1})),
            count: None,
        }],
        ..Expectation::default()
    };
    let failures = score(&o, &expect);
    assert!(matches!(
        failures.as_slice(),
        [Failure::RequiredToolCallNotSatisfied {
            actual_matches: 0,
            result_filter_set: true,
            args_filter_set: false,
            ..
        }]
    ));
}

#[test]
fn required_tool_calls_result_subset_fails_when_result_not_captured() {
    // Capture policy left call_result unrecorded; matcher with
    // result_subset can't verify and therefore counts the call as
    // non-matching — surfaces missing ToolIoCapture as a failed
    // assertion rather than a silent pass.
    use crate::expectation::ToolCallMatcher;
    let o = outcome(
        AgentMetrics {
            tools: vec![tool_with_args("search", serde_json::json!({"q": "x"}))], // call_result = None
            ..Default::default()
        },
        "",
    );
    let expect = Expectation {
        required_tool_calls: vec![ToolCallMatcher {
            name: "search".into(),
            args_subset: None,
            result_subset: Some(serde_json::json!({"count": 1})),
            count: None,
        }],
        ..Expectation::default()
    };
    assert_eq!(score(&o, &expect).len(), 1);
}

#[test]
fn required_tool_calls_args_and_result_both_required() {
    // Matcher with both filters: a call must satisfy BOTH args_subset
    // AND result_subset to count. One match with right args + wrong
    // result still fails.
    use crate::expectation::ToolCallMatcher;
    let o = outcome(
        AgentMetrics {
            tools: vec![tool_with_args_and_result(
                "search",
                serde_json::json!({"query": "banana"}),
                serde_json::json!({"count": 0}),
            )],
            ..Default::default()
        },
        "",
    );
    let expect = Expectation {
        required_tool_calls: vec![ToolCallMatcher {
            name: "search".into(),
            args_subset: Some(serde_json::json!({"query": "banana"})),
            result_subset: Some(serde_json::json!({"count": 1})),
            count: None,
        }],
        ..Expectation::default()
    };
    let failures = score(&o, &expect);
    assert!(matches!(
        failures.as_slice(),
        [Failure::RequiredToolCallNotSatisfied {
            actual_matches: 0,
            args_filter_set: true,
            result_filter_set: true,
            ..
        }]
    ));
}

#[test]
fn passing_run_returns_empty_failures() {
    let o = outcome(
        AgentMetrics {
            inferences: vec![span(10, 10)],
            tools: vec![tool("search"), tool("write")],
            session_duration_ms: 100,
            ..Default::default()
        },
        "the answer contains banana",
    );
    let expect = Expectation {
        final_answer_contains: vec!["banana".into()],
        final_answer_excludes: vec!["secret".into()],
        tool_sequence: vec!["search".into(), "write".into()],
        forbidden_tools: vec!["rm".into()],
        max_tokens_total: Some(1000),
        max_duration_ms: Some(1000),
        ..Expectation::default()
    };
    assert!(score(&o, &expect).is_empty());
}
