//! Pure scoring: compare a [`ReplayOutcome`] against an [`Expectation`].
//!
//! The function performs no I/O and never panics. It enumerates failures in
//! a stable order so reports diff cleanly across runs.

use crate::expectation::{Expectation, Failure};
use crate::outcome::ReplayOutcome;

/// Score `outcome` against `expect`, returning the (possibly empty) list of
/// reasons the run did not meet expectations.
///
/// An empty result means "all configured criteria passed". An empty
/// expectation (no criteria set) always returns `vec![]`.
pub fn score(outcome: &ReplayOutcome, expect: &Expectation) -> Vec<Failure> {
    let mut failures = Vec::new();

    // Required substrings.
    for phrase in &expect.final_answer_contains {
        if !outcome.final_text.contains(phrase) {
            failures.push(Failure::AnswerMissingPhrase {
                phrase: phrase.clone(),
            });
        }
    }

    // Forbidden substrings.
    for phrase in &expect.final_answer_excludes {
        if outcome.final_text.contains(phrase) {
            failures.push(Failure::AnswerContainsExcludedPhrase {
                phrase: phrase.clone(),
            });
        }
    }

    // Tool sequence: exact order match.
    let actual_tools = outcome.tool_sequence();
    if !expect.tool_sequence.is_empty() && actual_tools != expect.tool_sequence {
        failures.push(Failure::ToolSequenceMismatch {
            expected: expect.tool_sequence.clone(),
            actual: actual_tools.clone(),
        });
    }

    // Forbidden tools.
    for forbidden in &expect.forbidden_tools {
        if actual_tools.iter().any(|t| t == forbidden) {
            failures.push(Failure::ForbiddenToolUsed {
                tool: forbidden.clone(),
            });
        }
    }

    // Token budget.
    if let Some(budget) = expect.max_tokens_total {
        let actual = outcome.total_tokens();
        if actual > budget {
            failures.push(Failure::TokenBudgetExceeded { budget, actual });
        }
    }

    // Duration budget — uses session_duration_ms, not wall-clock elapsed,
    // so judgement is independent of CI host speed.
    if let Some(budget) = expect.max_duration_ms {
        let actual = outcome.metrics.session_duration_ms;
        if actual > budget {
            failures.push(Failure::DurationExceeded {
                budget_ms: budget,
                actual_ms: actual,
            });
        }
    }

    failures
}

#[cfg(test)]
mod tests {
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
        }
    }

    fn outcome(metrics: AgentMetrics, text: &str) -> ReplayOutcome {
        ReplayOutcome {
            fixture_id: "test".into(),
            final_text: text.into(),
            metrics,
            elapsed: Duration::from_millis(0),
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
            [Failure::ToolSequenceMismatch { expected, actual }]
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
        // We expect at least:
        //   answer_missing_phrase, tool_sequence_mismatch, forbidden_tool_used,
        //   token_budget_exceeded, duration_exceeded
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
}
