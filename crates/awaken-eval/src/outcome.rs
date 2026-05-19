//! What a replay produced and how it compared against an expectation.

use std::time::Duration;

use awaken_ext_observability::{AgentMetrics, AgentToolStats};
use serde::{Deserialize, Serialize};

use crate::expectation::Failure;

/// Replay-time failures that aren't fixture-vs-expectation mismatches but
/// signal that the replay itself was malformed or the runtime misbehaved.
/// Surfaced through [`ReplayOutcome::runtime_failure`] and turned into
/// [`Failure::ReplayRuntimeFailure`] by scoring so the NDJSON report stays
/// complete (vs. aborting the whole batch via panic).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReplayRuntimeFailure {
    /// The runtime called the scripted executor more times than the
    /// fixture provided events for. Non-zero `extra_calls` means a retry
    /// fired, an extra tool round was attempted, or the fixture under-
    /// specifies the script.
    ScriptExhausted { extra_calls: usize },
    /// The fixture's `provider_script` had events left when the runtime
    /// stopped. Catches dropped rounds / missed tool calls / absent
    /// retries that would otherwise pass silently on a "final_text
    /// happens to look right" expectation.
    ProviderScriptUnused { remaining: usize },
    /// The runtime returned an error that didn't originate from a
    /// scripted `Error` event (resolver failure, internal bug, etc.).
    /// `error_type` would be `None` here — surface the raw message so
    /// the CLI report still names the wiring failure.
    RuntimeError { message: String },
}

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
    /// When the run terminated because inference returned an error, the
    /// fixture-author-supplied `error_type` of the *first* such error
    /// (e.g. `"rate_limit"`). `None` for runs that completed without
    /// raising an inference error.
    ///
    /// `AgentLoopError::InferenceFailed(String)` flattens the upstream
    /// `InferenceExecutionError` variant, so the eval framework captures
    /// the structured type at the scripted-executor seam instead.
    pub error_type: Option<String>,
    /// Count of scripted `Error` events that fired during the run.
    /// Failure-path replays would otherwise look like "0 inferences
    /// happened" because the runtime's observability hook doesn't run
    /// on the `Err(_)` branch of `LlmExecutor::execute`.
    pub inference_error_count: usize,
    /// Replay-time failure that isn't an expectation mismatch (script
    /// exhausted, unused script, runtime error). Scoring promotes this
    /// into a [`Failure::ReplayRuntimeFailure`] so the NDJSON report
    /// stays complete.
    pub runtime_failure: Option<ReplayRuntimeFailure>,
    /// Number of "reprocess on judge fail" iterations the replayer ran
    /// past the initial attempt. `0` for runs without revise configured
    /// or runs whose initial answer cleared the judge threshold.
    /// Mirrors Anthropic Outcomes' rework count.
    pub revision_count: u32,
    /// Final judge score for runs that went through the revise loop —
    /// cached so the scorer can apply `min_judge_score` without a
    /// second judge call. `None` when no judge was invoked by the
    /// replayer (caller will judge externally if desired).
    pub judge_score: Option<f32>,
    /// Final judge reasoning string, paired with [`Self::judge_score`].
    /// Persisted so [`crate::judge::score_with_judge`] can serve a
    /// cache hit without losing the prose explanation — otherwise the
    /// "skip re-judging" path would silently strip reasoning callers
    /// expect to render in the UI.
    pub judge_reasoning: Option<String>,
}

impl ReplayOutcome {
    /// Synthetic outcome for a cell whose wall-clock budget expired.
    /// Used by callers that wrap replay in `tokio::time::timeout` so a
    /// stuck provider doesn't pin a request slot: the cell yields a
    /// real `EvalRunItem` carrying `runtime_failure`, which scoring
    /// promotes to `Failure::ReplayRuntimeFailure` in the report.
    pub fn timeout_failure(fixture_id: String, walltime_secs: u64) -> Self {
        Self {
            fixture_id,
            final_text: String::new(),
            metrics: AgentMetrics::default(),
            elapsed: Duration::from_secs(walltime_secs),
            error_type: None,
            inference_error_count: 0,
            runtime_failure: Some(ReplayRuntimeFailure::RuntimeError {
                message: format!("cell walltime exceeded: max {walltime_secs}s"),
            }),
            revision_count: 0,
            judge_score: None,
            judge_reasoning: None,
        }
    }

    /// Total tokens consumed across all inferences. Per-span fallback:
    /// each span contributes its own `total_tokens` when set, otherwise
    /// its `input_tokens + output_tokens`. Mixing the two within one
    /// run (e.g. a first turn that reports only `total_tokens` and a
    /// second turn that reports only `input/output`) sums correctly
    /// instead of falling off the cliff at the all-or-nothing seam.
    ///
    /// Negative underlying values (`AgentMetrics` permits `i32`) are
    /// clamped to zero per span.
    pub fn total_tokens(&self) -> u32 {
        let total: i64 = self
            .metrics
            .inferences
            .iter()
            .map(|s| {
                if let Some(t) = s.total_tokens {
                    i64::from(t).max(0)
                } else {
                    let input = i64::from(s.input_tokens.unwrap_or(0)).max(0);
                    let output = i64::from(s.output_tokens.unwrap_or(0)).max(0);
                    input + output
                }
            })
            .sum();
        u32::try_from(total).unwrap_or(u32::MAX)
    }

    /// Names of tools invoked, in record order.
    pub fn tool_sequence(&self) -> Vec<String> {
        self.metrics.tools.iter().map(|t| t.name.clone()).collect()
    }

    /// The `run_id` the runtime assigned to this replay, taken from the
    /// first recorded span. Returns `None` when no spans were emitted
    /// (e.g. a misconfigured executor that erred before any inference).
    /// Used by the server's eval-run service to link an `EvalRunItem`
    /// back to the `TraceStore` entry written by the tee sink.
    pub fn trace_run_id(&self) -> Option<&str> {
        // Inference spans are the most common; fall back to tool spans
        // if the run errored before any inference completed (still
        // possible for failure-path fixtures with an immediate Error
        // event — the runtime emits a handoff/tool span at startup).
        if let Some(s) = self.metrics.inferences.first()
            && !s.context.run_id.is_empty()
        {
            return Some(&s.context.run_id);
        }
        if let Some(s) = self.metrics.tools.first()
            && !s.context.run_id.is_empty()
        {
            return Some(&s.context.run_id);
        }
        None
    }
}

// `saturating_i32_to_u32`: clamp i32 token sentinel to u32 (negative ⇒ 0).
const fn saturating_i32_to_u32(v: i32) -> u32 {
    if v <= 0 { 0 } else { v as u32 }
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
    /// What [`crate::score`] actually compares against `max_tokens_total`.
    /// Per-span: `span.total_tokens` when set, otherwise
    /// `input + output`. Surfaced on the report so baseline diff sees
    /// the same value the scorer used — otherwise a fixture that only
    /// reports `TokenUsage.total_tokens` could drift without
    /// `total_input/output_tokens` changing.
    ///
    /// `#[serde(default)]` so pre-existing baselines (without the field)
    /// still deserialise cleanly.
    #[serde(default)]
    pub total_tokens: u32,
    pub session_duration_ms: u64,
    /// Wall-clock duration of [`crate::replay`]. Excluded from the
    /// serialised baseline because it varies per-host and would otherwise
    /// dirty the committed `baseline.ndjson` on every regeneration.
    /// `session_duration_ms` is the deterministic counterpart used for
    /// scoring (see [`crate::score`]).
    #[serde(default, skip_serializing)]
    pub elapsed_ms: u64,
    /// Per-(agent, tool) tool-call counts. Empty when the run had no tool
    /// invocations or when no `agent_id` is on the spans. Populated by
    /// [`ReplayReport::from_outcome`] from
    /// [`AgentMetrics::stats_by_agent_and_tool`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls_by_agent: Vec<AgentToolStats>,
    /// Mirrors [`ReplayOutcome::error_type`]. Captures the fixture's
    /// upstream-error variant when an inference error tripped the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
    /// Mirrors [`ReplayOutcome::inference_error_count`]. Lets baseline
    /// diff catch a failure path silently degrading into "0 errors
    /// because the runtime didn't even call inference".
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub inference_error_count: usize,
    /// Mirrors [`ReplayOutcome::runtime_failure`]. Serialised so a
    /// regenerated baseline records the kind of failure (script
    /// exhausted, unused script, runtime error) and `diff_against_baseline`
    /// can flag drift.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_failure: Option<ReplayRuntimeFailure>,
    /// Cost of the replay in USD. Computed server-side from the
    /// resolved `ModelBindingSpec` pricing × token counts; left `None`
    /// when the binding has no pricing or the run was scripted. Drift
    /// detection treats this as an observable so a silent price bump
    /// surfaces in regression diffs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Number of reprocess-on-judge-fail iterations the replayer ran
    /// (0 = initial attempt cleared the threshold or revise wasn't
    /// configured). Lets diffs surface "agent now needs 2 retries
    /// where it used to need 0".
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub revision_count: u32,
    /// Final judge score recorded by the replayer's revise loop.
    /// `None` when no judge ran inside the replayer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge_score: Option<f32>,
    /// Final judge reasoning string (paired with `judge_score`). Carried
    /// through serialisation so admin UI can render "why the score is
    /// what it is" without re-invoking the judge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge_reasoning: Option<String>,
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

fn is_zero_usize(n: &usize) -> bool {
    *n == 0
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
            total_input_tokens: saturating_i32_to_u32(outcome.metrics.total_input_tokens()),
            total_output_tokens: saturating_i32_to_u32(outcome.metrics.total_output_tokens()),
            total_tokens: outcome.total_tokens(),
            session_duration_ms: outcome.metrics.session_duration_ms,
            elapsed_ms: u64::try_from(outcome.elapsed.as_millis()).unwrap_or(u64::MAX),
            tool_calls_by_agent: outcome.metrics.stats_by_agent_and_tool(),
            error_type: outcome.error_type.clone(),
            inference_error_count: outcome.inference_error_count,
            runtime_failure: outcome.runtime_failure.clone(),
            cost_usd: None,
            revision_count: outcome.revision_count,
            judge_score: outcome.judge_score,
            judge_reasoning: outcome.judge_reasoning.clone(),
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
}
