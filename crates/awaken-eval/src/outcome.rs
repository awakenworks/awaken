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

fn sum_optional_tokens_per_span<I>(values: I) -> u32
where
    I: IntoIterator<Item = Option<i32>>,
{
    let total: i64 = values
        .into_iter()
        .map(|v| i64::from(v.unwrap_or(0)).max(0))
        .sum();
    u32::try_from(total).unwrap_or(u32::MAX)
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
            total_input_tokens: sum_optional_tokens_per_span(
                outcome.metrics.inferences.iter().map(|s| s.input_tokens),
            ),
            total_output_tokens: sum_optional_tokens_per_span(
                outcome.metrics.inferences.iter().map(|s| s.output_tokens),
            ),
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
#[path = "outcome_test.rs"]
mod tests;
