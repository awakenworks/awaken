//! What a replay produced and how it compared against an expectation.

use std::time::Duration;

use awaken_ext_observability::{AgentMetrics, AgentToolStats};
use serde::{Deserialize, Serialize};

use crate::expectation::Failure;

/// Raw output of a single replay — captured before scoring.
#[derive(Debug, Clone)]
pub struct ReplayOutcome {
    pub fixture_id: String,
    /// Concatenated assistant text across all rounds.
    pub final_text: String,
    /// Agent metrics aggregated by the in-memory observability sink.
    pub metrics: AgentMetrics,
    /// Wall-clock time spent inside [`crate::replay`] (M4.3).
    pub elapsed: Duration,
}

impl ReplayOutcome {
    /// Total tokens (input + output) across all inferences. Negative
    /// underlying values (which `AgentMetrics` permits as `i32`) are
    /// clamped to zero.
    pub fn total_tokens(&self) -> u32 {
        let i = u32::try_from(self.metrics.total_input_tokens()).unwrap_or(0);
        let o = u32::try_from(self.metrics.total_output_tokens()).unwrap_or(0);
        i.saturating_add(o)
    }

    /// Names of tools invoked, in record order.
    pub fn tool_sequence(&self) -> Vec<String> {
        self.metrics.tools.iter().map(|t| t.name.clone()).collect()
    }
}

/// Compact, JSON-friendly view of a [`ReplayOutcome`] paired with its
/// scoring [`Failure`]s.  Each line of the NDJSON report is one of these.
///
/// Older NDJSON reports (pre-`tool_calls_by_agent`) deserialize cleanly
/// thanks to `#[serde(default)]` on the new field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayReport {
    pub fixture_id: String,
    pub passed: bool,
    pub failures: Vec<Failure>,
    pub final_text: String,
    pub inference_count: usize,
    pub tool_count: usize,
    pub tool_failures: usize,
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
    pub session_duration_ms: u64,
    pub elapsed_ms: u64,
    /// Per-(agent, tool) tool-call counts. Empty when the run had no tool
    /// invocations or when no `agent_id` is on the spans. Populated by
    /// [`ReplayReport::from_outcome`] from
    /// [`AgentMetrics::stats_by_agent_and_tool`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls_by_agent: Vec<AgentToolStats>,
}

impl ReplayReport {
    /// Build a report from a raw outcome and the failures returned by
    /// [`crate::score`].
    pub fn from_outcome(outcome: &ReplayOutcome, failures: Vec<Failure>) -> Self {
        Self {
            fixture_id: outcome.fixture_id.clone(),
            passed: failures.is_empty(),
            failures,
            final_text: outcome.final_text.clone(),
            inference_count: outcome.metrics.inference_count(),
            tool_count: outcome.metrics.tool_count(),
            tool_failures: outcome.metrics.tool_failures(),
            total_input_tokens: u32::try_from(outcome.metrics.total_input_tokens()).unwrap_or(0),
            total_output_tokens: u32::try_from(outcome.metrics.total_output_tokens()).unwrap_or(0),
            session_duration_ms: outcome.metrics.session_duration_ms,
            elapsed_ms: u64::try_from(outcome.elapsed.as_millis()).unwrap_or(u64::MAX),
            tool_calls_by_agent: outcome.metrics.stats_by_agent_and_tool(),
        }
    }
}

#[cfg(test)]
mod tests {
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
        }
    }

    // ── ReplayOutcome ───────────────────────────────────────────────

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
    fn report_serde_roundtrip() {
        let o = outcome_with(AgentMetrics::default(), "answer");
        let r = ReplayReport::from_outcome(
            &o,
            vec![Failure::TokenBudgetExceeded {
                budget: 100,
                actual: 200,
            }],
        );
        let json = serde_json::to_string(&r).unwrap();
        let parsed: ReplayReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, r);
    }

    #[test]
    fn report_elapsed_ms_saturates_at_u64_max() {
        let o = ReplayOutcome {
            fixture_id: "saturate".into(),
            final_text: String::new(),
            metrics: AgentMetrics::default(),
            elapsed: Duration::from_secs(u64::MAX / 1000),
        };
        let r = ReplayReport::from_outcome(&o, Vec::new());
        // We just need to confirm it doesn't panic; exact value depends on
        // platform but must be finite (u64).
        let _ = r.elapsed_ms;
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
        let r = ReplayReport::from_outcome(&o, Vec::new());
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains(r#""tool_calls_by_agent""#));
        let parsed: ReplayReport = serde_json::from_str(&json).unwrap();
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
    }
}
