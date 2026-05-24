//! Per-cell composition primitives shared by the dataset matrix runner
//! (`eval_run_service::run_matrix_cells`) and the ad-hoc online runner
//! (`online_eval_service`). Lifting them out of `eval_run_service` keeps
//! that file under the lefthook line cap and makes the contract between
//! the two drivers explicit instead of cross-module inline duplication.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use awaken_contract::agent_spec_patch::AgentSpecPatch;
use awaken_contract::contract::executor::LlmExecutor;
use awaken_contract::registry_spec::{AgentSpec, ModelBindingSpec};
use awaken_eval::{
    EvalRun, EvalRunExecutionMode, EvalRunItem, Expectation, Failure, Fixture, LlmExecutorJudge,
    MatrixCell, ReplayOutcome, ReplayReport, RuntimeReplayer, replay_all, score, score_with_judge,
};
use awaken_ext_observability::MetricsSink;
use awaken_ext_observability::trace_store::TraceStore;

use crate::error::ApiError;

/// Shared default for live eval token budgets. Both dataset matrix runs and
/// ad-hoc online runs should start with the same cost guardrail unless the
/// caller explicitly overrides it.
pub(crate) const DEFAULT_MAX_TOTAL_TOKENS: u32 = 10_000;

/// Resolved judge config carried through the replay loop. Replaces the
/// `(LlmExecutorJudge, Option<String>, Option<u32>)` tuple this code used
/// to thread — at the call sites that tuple was opaque enough that
/// renaming or reordering a field was a search-and-replace minefield.
#[derive(Clone)]
pub(crate) struct JudgeContext {
    pub judge: LlmExecutorJudge,
    pub rubric: Option<String>,
    pub revise_max_retries: Option<u32>,
}

/// Per-cell revise loop config: `(judge, rubric, threshold, max_retries)`.
/// `None` when any required piece (judge, fixture threshold, retry budget)
/// is missing or when `revise_max_retries == 0` — same gating in dataset
/// matrix runs and online ad-hoc runs.
pub(crate) type ReviseTuple = (Arc<dyn awaken_eval::judge::Judge>, Option<String>, f32, u32);

/// One pre-resolved live matrix cell. Model/provider resolution happens
/// before any replay starts so missing registry entries fail fast and
/// sibling cells can share the same executor/billing metadata.
#[derive(Clone)]
pub(crate) struct ResolvedCell {
    pub cell: MatrixCell,
    pub executor: Arc<dyn LlmExecutor>,
    pub upstream_model: String,
    pub binding: ModelBindingSpec,
}

/// Tunables for [`run_live_eval_cells`]. Dataset matrix runs and ad-hoc
/// online runs share the same replay/scoring/timeout semantics; keeping
/// the knobs in one struct prevents the two handlers from drifting.
pub(crate) struct LiveCellOptions {
    pub samples: u32,
    pub max_concurrent: usize,
    pub max_walltime_secs: u64,
    pub agent_base: Option<AgentSpec>,
    pub agent_overrides: Option<AgentSpecPatch>,
    pub judge: Option<JudgeContext>,
    pub max_total_tokens: Option<u32>,
    pub trace_sink: Option<Arc<dyn MetricsSink>>,
    pub trace_store: Option<Arc<dyn TraceStore>>,
    pub task_context: &'static str,
}

/// Build the per-cell revise tuple, applying the all-three-pieces gating
/// rule both eval services share. A configured retry budget of `0` means
/// "do not revise"; the judge still runs in the scoring phase where
/// timeout/error promotion can preserve the real replay outcome.
pub(crate) fn revise_tuple_for(
    judge: Option<&JudgeContext>,
    expect: &Expectation,
) -> Option<ReviseTuple> {
    match (judge, expect.min_judge_score) {
        (
            Some(JudgeContext {
                judge: j,
                rubric,
                revise_max_retries: Some(retries),
            }),
            Some(threshold),
        ) if *retries > 0 => Some((
            Arc::new(j.clone()) as Arc<dyn awaken_eval::judge::Judge>,
            rubric.clone(),
            threshold,
            *retries,
        )),
        _ => None,
    }
}

pub(crate) fn validate_baseline_sample_count(
    baseline: &EvalRun,
    new_run_samples: Option<u32>,
) -> Result<(), ApiError> {
    let Some(new_samples) = new_run_samples else {
        return Ok(());
    };
    let Some(baseline_samples) = live_run_sample_count(baseline)? else {
        return Ok(());
    };
    if baseline_samples != new_samples {
        return Err(ApiError::BadRequest(format!(
            "cannot diff live runs with different sample counts: baseline {} has {}, new run has {}",
            baseline.id, baseline_samples, new_samples
        )));
    }
    Ok(())
}

pub(crate) fn live_run_sample_count(run: &EvalRun) -> Result<Option<u32>, ApiError> {
    if run.execution_mode != EvalRunExecutionMode::Live {
        return Ok(None);
    }
    let mut groups: BTreeMap<(String, MatrixCell), BTreeSet<u32>> = BTreeMap::new();
    for item in &run.items {
        groups
            .entry((
                item.fixture_id.clone(),
                item.cell.clone().unwrap_or_default(),
            ))
            .or_default()
            .insert(item.sample_index.unwrap_or(0));
    }
    let mut sample_count = None;
    for ((fixture_id, cell), indexes) in groups {
        let actual: Vec<u32> = indexes.into_iter().collect();
        let expected: Vec<u32> = (0..actual.len() as u32).collect();
        if actual != expected {
            return Err(ApiError::BadRequest(format!(
                "run {} has non-contiguous samples for fixture {} cell {:?}: {:?}",
                run.id, fixture_id, cell, actual
            )));
        }
        let n = actual.len() as u32;
        if let Some(previous) = sample_count
            && previous != n
        {
            return Err(ApiError::BadRequest(format!(
                "run {} has inconsistent sample counts across live cells",
                run.id
            )));
        }
        sample_count = Some(n);
    }
    Ok(sample_count)
}

/// Apply the three optional decorators every cell shares: agent overrides,
/// tee-sink for trace fan-out, and the revise-on-judge-fail loop.
pub(crate) fn apply_cell_decorators(
    mut replayer: RuntimeReplayer,
    overrides: Option<AgentSpecPatch>,
    trace_sink: Option<Arc<dyn awaken_ext_observability::MetricsSink>>,
    revise: Option<ReviseTuple>,
) -> RuntimeReplayer {
    if let Some(p) = overrides {
        replayer = replayer.with_agent_overrides(p);
    }
    if let Some(sink) = trace_sink {
        replayer = replayer.with_tee_sink(sink);
    }
    if let Some((j, rubric, threshold, retries)) = revise {
        replayer = replayer.with_revise_on_judge_fail(j, rubric, threshold, retries);
    }
    replayer
}

/// Reject duplicate model ids in the request `models` axis. Both eval
/// services would otherwise spawn the same cell twice, producing
/// duplicate `(fixture_id, cell, sample_index)` keys that `diff_eval_items`
/// would later collapse silently — caught by the diff guard but too
/// late: the duplicate already persisted to the EvalRun store.
pub(crate) fn validate_unique_models(models: &[String]) -> Result<(), ApiError> {
    use std::collections::HashSet;
    let mut seen: HashSet<&str> = HashSet::with_capacity(models.len());
    for m in models {
        if !seen.insert(m.as_str()) {
            return Err(ApiError::BadRequest(format!(
                "duplicate model id in models axis: {m}"
            )));
        }
    }
    Ok(())
}

/// Claude eval guidance treats LLM grading as an explicit automated
/// grading method with a clear rubric. If a fixture sets
/// `min_judge_score`, silently falling back to deterministic scoring or
/// a vague default rubric would make that success criterion look
/// satisfied without actually evaluating it. Fail fast instead.
pub(crate) fn validate_judge_required_for_expectation(
    expect: &Expectation,
    label: &str,
    live_mode: bool,
    has_judge: bool,
    judge_rubric: Option<&str>,
) -> Result<(), ApiError> {
    if expect.min_judge_score.is_none() {
        return Ok(());
    }
    awaken_eval::validate_min_judge_score(expect, label).map_err(ApiError::BadRequest)?;
    if !live_mode {
        return Err(ApiError::BadRequest(format!(
            "{label} sets expect.min_judge_score; LLM grading requires mode=\"live\" with `judge`"
        )));
    }
    if !has_judge {
        return Err(ApiError::BadRequest(format!(
            "{label} sets expect.min_judge_score; provide `judge` so the LLM grading criterion is actually evaluated"
        )));
    }
    match judge_rubric {
        Some(rubric) if !rubric.trim().is_empty() => {}
        _ => {
            return Err(ApiError::BadRequest(format!(
                "{label} sets expect.min_judge_score; provide non-empty `judge.rubric` so the LLM grading criterion has an explicit rubric"
            )));
        }
    }
    Ok(())
}

/// Compute cell-level `cost_usd`, but only when the report carries an
/// actual input/output token breakdown. Providers that only fill the
/// aggregate `total_tokens` would otherwise yield `compute_cost_usd(0, 0)
/// = Some(0.0)`, silently presenting "$0" cost for runs that genuinely
/// burned tokens. Returning `None` makes the cost-missing case explicit
/// to downstream consumers (admin UI, baseline diff, billing exports).
pub(crate) fn cost_usd_for(report: &ReplayReport, binding: &ModelBindingSpec) -> Option<f64> {
    if report.total_input_tokens == 0 && report.total_output_tokens == 0 {
        return None;
    }
    binding.compute_cost_usd(report.total_input_tokens, report.total_output_tokens)
}

/// Return the replay trace id only when it is actually readable from the
/// configured TraceStore. `ReplayOutcome::trace_run_id()` is derived from
/// in-memory metrics; without this guard an append/indexing failure in the
/// tee sink would persist a dead `EvalRunItem.trace_run_id` pointer.
pub(crate) fn persisted_trace_run_id(
    trace_store: Option<&dyn TraceStore>,
    outcome: &ReplayOutcome,
) -> Option<String> {
    let run_id = outcome.trace_run_id()?;
    let Some(store) = trace_store else {
        tracing::warn!(
            run_id = %run_id,
            "dropping eval trace_run_id because no TraceStore is configured"
        );
        return None;
    };
    match store.read(run_id) {
        Ok(events) if !events.is_empty() => Some(run_id.to_string()),
        Ok(_) => {
            tracing::warn!(
                run_id = %run_id,
                "dropping eval trace_run_id because TraceStore returned no events"
            );
            None
        }
        Err(err) => {
            tracing::warn!(
                run_id = %run_id,
                error = %err,
                "dropping eval trace_run_id because TraceStore read failed"
            );
            None
        }
    }
}

/// Shared Live-mode cell runner used by dataset matrix evals and
/// `/v1/eval/online`. The function owns the "one cell deadline split
/// across replay and scoring" policy, per-cell judge failure promotion,
/// trace link verification, and cost attribution.
pub(crate) async fn run_live_eval_cells(
    fixtures: &[Fixture],
    resolved_cells: &[ResolvedCell],
    options: LiveCellOptions,
) -> Result<Vec<EvalRunItem>, ApiError> {
    let LiveCellOptions {
        samples,
        max_concurrent,
        max_walltime_secs,
        agent_base,
        agent_overrides,
        judge,
        max_total_tokens,
        trace_sink,
        trace_store,
        task_context,
    } = options;
    let walltime = std::time::Duration::from_secs(max_walltime_secs);
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent));
    let mut handles = Vec::with_capacity(fixtures.len() * resolved_cells.len() * samples as usize);
    let emit_sample_index = samples > 1;

    for fixture in fixtures {
        for resolved in resolved_cells {
            for sample in 0..samples {
                let fixture = fixture.clone();
                let fixture_id = fixture.id.clone();
                let cell = resolved.cell.clone();
                let executor = resolved.executor.clone();
                let upstream_model = resolved.upstream_model.clone();
                let binding = resolved.binding.clone();
                let overrides = agent_overrides.clone();
                let base = agent_base.clone();
                let trace_sink = trace_sink.clone();
                let judge_for_task = judge.clone();
                let revise_for_task = revise_tuple_for(judge.as_ref(), &fixture.expect);
                let permit = semaphore.clone().acquire_owned().await.expect("semaphore");
                handles.push(tokio::spawn(async move {
                    let _permit = permit;
                    let mut builder =
                        RuntimeReplayer::new().with_live_executor(executor, upstream_model);
                    if let Some(max) = max_total_tokens {
                        builder = builder.with_max_total_tokens(max);
                    }
                    if let Some(b) = base {
                        builder = builder.with_agent_base(b);
                    }
                    let replayer =
                        apply_cell_decorators(builder, overrides, trace_sink, revise_for_task);
                    let deadline = tokio::time::Instant::now() + walltime;
                    let walltime_secs = walltime.as_secs();
                    let outcome = match tokio::time::timeout_at(deadline, async {
                        let outcomes =
                            replay_all(&replayer, std::slice::from_ref(&fixture)).await;
                        outcomes
                            .into_iter()
                            .next()
                            .expect("one fixture → one outcome")
                    })
                    .await
                    {
                        Ok(o) => o,
                        Err(_) => {
                            let (o, f) = cell_timeout_outcome(
                                fixture_id,
                                walltime_secs,
                                &fixture.expect,
                            );
                            return Ok::<_, ApiError>((fixture.id, cell, sample, o, f, binding));
                        }
                    };
                    let (outcome, failures) = match tokio::time::timeout_at(
                        deadline,
                        score_outcome(&outcome, &fixture, judge_for_task.as_ref()),
                    )
                    .await
                    {
                        Ok(Ok((failures, judge_result))) => {
                            // Stamp the LLM grade back onto the outcome
                            // so ReplayReport (and the baseline diff that
                            // compares `judge_score`) carry the score and
                            // reasoning — not just the derived pass/fail.
                            let mut outcome = outcome;
                            if let Some(jr) = judge_result {
                                outcome.judge_score = Some(jr.score);
                                outcome.judge_reasoning = jr.reasoning;
                            }
                            (outcome, failures)
                        }
                        Ok(Err(err)) => cell_error_outcome(
                            outcome,
                            format!("scoring failed: {err}"),
                            &fixture.expect,
                        ),
                        Err(_) => cell_error_outcome(
                            outcome,
                            format!(
                                "scoring timed out after {walltime_secs}s wall-clock (replay completed)"
                            ),
                            &fixture.expect,
                        ),
                    };
                    Ok::<_, ApiError>((fixture.id, cell, sample, outcome, failures, binding))
                }));
            }
        }
    }

    let mut items: Vec<EvalRunItem> = Vec::with_capacity(handles.len());
    for handle in handles {
        let task_result = handle
            .await
            .map_err(|err| ApiError::Internal(format!("{task_context} task panicked: {err}")))?;
        let (fixture_id, cell, sample, outcome, failures, binding) = task_result?;
        let mut report = ReplayReport::from_outcome(&outcome, failures);
        report.cost_usd = cost_usd_for(&report, &binding);
        items.push(EvalRunItem {
            fixture_id,
            cell: Some(cell),
            report,
            trace_run_id: persisted_trace_run_id(trace_store.as_deref(), &outcome),
            sample_index: if emit_sample_index {
                Some(sample)
            } else {
                None
            },
        });
    }

    Ok(items)
}

/// Synthetic outcome + failures for a cell whose wall-clock budget
/// expired. Pairs the `runtime_failure`-bearing outcome with the
/// failure vector the deterministic scorer derives from it, so
/// `ReplayReport::from_outcome` lands on `passed=false`. Building the
/// outcome alone (with `failures = Vec::new()`) would silently report
/// `passed=true` because `passed = failures.is_empty()`.
pub(crate) fn cell_timeout_outcome(
    fixture_id: String,
    walltime_secs: u64,
    expect: &Expectation,
) -> (ReplayOutcome, Vec<Failure>) {
    let outcome = ReplayOutcome::timeout_failure(fixture_id, walltime_secs);
    let failures = score(&outcome, expect);
    (outcome, failures)
}

/// Per-cell outcome + failures when scoring/judge invocation errored
/// on a cell whose replay itself completed. Preserves the real
/// `ReplayOutcome` (final_text, metrics, token counts, trace run_id,
/// elapsed, revision_count, judge_score) so the per-cell report still
/// reflects what the model actually produced. Discarding the outcome
/// and rebuilding an empty one here would:
///   * blank `final_text`, fabricating phantom `AnswerMissingPhrase`
///     deterministic failures the model didn't actually trip;
///   * zero `metrics` / `total_tokens`, hiding cost that was really
///     burned and breaking `cost_usd_for` accounting;
///   * drop `trace_run_id`, severing the EvalRunItem → TraceStore link
///     the admin UI relies on to surface "why did the judge fail".
///
/// When the outcome ALREADY carries a `runtime_failure` (e.g. replay
/// itself hit `token budget exceeded`), that primary cause is left in
/// place and the scoring error is appended as a separate
/// `Failure::ReplayRuntimeFailure` in the failures list, so both
/// reasons reach the per-cell report instead of the scoring message
/// silently overwriting the upstream replay failure.
pub(crate) fn cell_error_outcome(
    mut outcome: ReplayOutcome,
    message: String,
    expect: &Expectation,
) -> (ReplayOutcome, Vec<Failure>) {
    let had_existing = outcome.runtime_failure.is_some();
    if !had_existing {
        outcome.runtime_failure = Some(awaken_eval::ReplayRuntimeFailure::RuntimeError {
            message: message.clone(),
        });
    }
    let mut failures = score(&outcome, expect);
    if had_existing {
        // `score()` already emitted a Failure for the pre-existing
        // runtime_failure; append the scoring error so both surface.
        failures.push(Failure::ReplayRuntimeFailure {
            failure: awaken_eval::ReplayRuntimeFailure::RuntimeError { message },
        });
    }
    (outcome, failures)
}

/// Pick the scorer based on whether a judge is wired: judge-aware when a
/// `JudgeContext` is present AND the fixture asks for it via
/// `min_judge_score`; otherwise the deterministic scorer.
///
/// Returns `(failures, judge_result)`. When `judge_result` is `Some`, the
/// caller is expected to stamp `judge_score`/`judge_reasoning` back onto
/// the `ReplayOutcome` so the report and downstream diff carry the LLM
/// grade — not just the pass/fail derived from it.
pub(crate) async fn score_outcome(
    outcome: &awaken_eval::ReplayOutcome,
    fixture: &Fixture,
    judge: Option<&JudgeContext>,
) -> Result<(Vec<awaken_eval::Failure>, Option<awaken_eval::JudgeResult>), ApiError> {
    match (judge, fixture.expect.min_judge_score) {
        (
            Some(JudgeContext {
                judge: j, rubric, ..
            }),
            Some(_),
        ) => {
            let (failures, judge_result) = score_with_judge(
                outcome,
                &fixture.expect,
                &fixture.judge_prompt(),
                rubric.as_deref(),
                j,
            )
            .await
            .map_err(|err| ApiError::Internal(format!("judge invocation failed: {err}")))?;
            Ok((failures, judge_result))
        }
        (None, Some(_)) => Err(ApiError::BadRequest(
            "expect.min_judge_score requires `judge`; refusing to ignore the LLM grading criterion"
                .into(),
        )),
        _ => Ok((score(outcome, &fixture.expect), None)),
    }
}

#[cfg(test)]
#[path = "eval_cell_test.rs"]
mod tests;
