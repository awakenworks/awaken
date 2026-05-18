//! `/v1/eval/runs` — server-side execution of datasets (ADR-0032 D1+D7).
//!
//! ## Endpoints
//!
//!   POST `/v1/eval/runs { dataset_id, baseline_run_id? }`
//!     Loads the dataset, drives every fixture through [`RuntimeReplayer`],
//!     scores each outcome, persists an [`EvalRun`], returns it.
//!
//!   GET `/v1/eval/runs?dataset_id=&limit=` — list run summaries.
//!   GET `/v1/eval/runs/:id` — fetch one run.
//!   GET `/v1/eval/runs/:id?baseline=:baseline_id` — fetch + diff (D7).
//!
//! ## Notes
//!
//! - `RuntimeReplayer` builds a self-contained runtime with an
//!   `InMemoryStore` — replays do not touch the server's thread store.
//!   That keeps eval runs from polluting production data.
//! - Replay does NOT yet wire spans into the server's `TraceStore`;
//!   `EvalRunItem.trace_run_id` is therefore `None` for now. Tracking
//!   the trace-run wiring as a follow-up so this layer stays focused.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use awaken_contract::agent_spec_patch::AgentSpecPatch;
use awaken_contract::config_record::validate_config_record;
use awaken_contract::contract::config_store::extract_meta_revision;
use awaken_eval::{
    DATASETS_NAMESPACE, DatasetSpec, DiffSummary, EvalRun, EvalRunFilter, EvalRunItem,
    EvalRunStore, EvalRunStoreError, EvalRunSummary, Fixture, LlmExecutorJudge, MatrixCell,
    ReplayReport, RuntimeReplayer, SampleAggregate, diff_against_baseline, expand_cells,
    mint_run_id, replay_all, score,
};
use awaken_ext_observability::MetricsSink;
use awaken_ext_observability::trace_store::TraceStoreSink;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::error::ApiError;
use crate::services::eval_common::{
    config_store_or_unavailable, map_storage_error, resolve_live_executor,
};

// `DATASETS_NAMESPACE` re-exported from `awaken_eval::dataset`.
// Caps formerly defined here as `const`s now live on
// `ServerConfig::eval_limits` (see `crate::app::EvalLimits`) so ops can
// tune per deployment instead of forking the source.

// ── Wire types ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartRunRequest {
    pub dataset_id: String,
    /// Optional baseline `EvalRun` id. When set, the response also
    /// carries a diff against the baseline (saves a GET round-trip for
    /// the common "run and compare" flow).
    #[serde(default)]
    pub baseline_run_id: Option<String>,
    /// Optional matrix model axis. When set, each fixture is replayed
    /// once per model in **Live** mode (real provider executors); the
    /// fixture's `provider_script` is ignored. When unset, falls back
    /// to **Scripted** mode (default, current behaviour) — provider_script
    /// drives the LLM, suitable for CI smoke runs.
    #[serde(default)]
    pub models: Option<Vec<String>>,
    /// Registered agent whose `system_prompt` / `allowed_tools` /
    /// sampling params should be used as the base for Live-mode
    /// replays. Without this, the replayer falls back to a synthetic
    /// stub agent — the eval would *not* exercise the real agent's
    /// behaviour. `agent_overrides` (below) merges as a patch on top.
    /// Live mode only; ignored on Scripted runs.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Optional `AgentSpecPatch` applied to every fixture's agent spec
    /// (the registered spec from `agent_id`, or the synthetic stub when
    /// `agent_id` is unset). Live mode only. Reuses `ConfigRecord`'s
    /// `AgentSpecPatch` machinery so operators get the same
    /// `deny_unknown_fields` validation they get on
    /// `PATCH /v1/config/agents`.
    #[serde(default)]
    pub agent_overrides: Option<AgentSpecPatch>,
    /// Per-cell flakiness sample count. Each (fixture, cell) is replayed
    /// `samples` times so the pass_rate / latency distribution becomes
    /// visible instead of being a 1-shot point estimate. Default `None`
    /// = single sample, current behaviour. Only valid in Live (matrix)
    /// mode — scripted replays are deterministic. Capped at
    /// [`MAX_SAMPLES_PER_CELL`]; full unit count (fixtures × cells ×
    /// samples) must stay under [`MAX_CELLS_PER_SYNC_RUN`].
    #[serde(default)]
    pub samples: Option<u32>,
    /// Optional LLM-as-judge config. When set and the fixture's
    /// `expect.min_judge_score` is also set, each replay outcome is
    /// graded by the named model; a score below threshold appends a
    /// `Failure::JudgeBelowThreshold` to the report.
    #[serde(default)]
    pub judge: Option<JudgeRequest>,
}

/// Per-run judge configuration. `model_id` must resolve via the registry
/// the same way replay models do. `rubric` is optional grading
/// instructions; absent uses the built-in generic rubric.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeRequest {
    pub model_id: String,
    #[serde(default)]
    pub rubric: Option<String>,
    /// When set, after each replay the judge scores the outcome; if the
    /// score is below the fixture's `expect.min_judge_score`, the
    /// replayer appends a "revise this" user message on the same thread
    /// and re-runs the agent — up to this many retries. Mirrors
    /// Anthropic Outcomes' reprocess loop. Capped at
    /// [`MAX_JUDGE_REVISIONS`] so a thrashing agent can't drive cost
    /// unbounded.
    #[serde(default)]
    pub revise_max_retries: Option<u32>,
}

// `MAX_JUDGE_REVISIONS` lives on `ServerConfig::eval_limits` so ops can
// tune per deployment.

pub(crate) use super::eval_cell::{
    JudgeContext, apply_cell_decorators, revise_tuple_for, score_outcome,
};

#[derive(Debug, Serialize)]
pub struct EvalRunResponse {
    pub run: EvalRun,
    /// Present only when [`StartRunRequest::baseline_run_id`] or the
    /// `?baseline=` query param resolved to a real prior run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffSummary>,
    /// Per-(fixture, cell) pass@k / pass^k roll-ups. Present only when
    /// the GET request set `?aggregate=samples`. The shape mirrors
    /// Anthropic Managed Agents' pass@k metric so consumers don't have
    /// to fold sample items themselves.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregates: Option<Vec<SampleAggregate>>,
}

#[derive(Debug, Serialize)]
pub struct ListEvalRunsResponse {
    pub runs: Vec<EvalRunSummary>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ListRunsQuery {
    pub dataset_id: Option<String>,
    /// Inclusive lower bound on `started_at_secs`.
    #[serde(default)]
    pub since_secs: Option<u64>,
    /// Exclusive upper bound on `started_at_secs`.
    #[serde(default)]
    pub until_secs: Option<u64>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GetRunQuery {
    /// When set, the response includes a `diff` against the named run.
    /// The baseline must exist or the request 404s (caller passed an
    /// invalid id — silent omission would mask the typo).
    pub baseline: Option<String>,
    /// When set, the response includes per-(fixture,cell) roll-ups of
    /// the requested shape. Unknown values are rejected by serde at
    /// query-deserialize time so a typo surfaces immediately.
    #[serde(default)]
    pub aggregate: Option<RunAggregateKind>,
}

/// Aggregation shape requested via `GET /v1/eval/runs/:id?aggregate=…`.
/// Single variant today (`samples` → pass@k / pass^k); leaving room for
/// future shapes (`cost`, `latency_percentiles`, …) without re-doing
/// the string-matching the previous `Option<String>` form required.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunAggregateKind {
    Samples,
}

// `map_storage_error` lives in `services::eval_common` — same impl
// shared with `dataset_service` so the two can't drift on revision-
// conflict shape.

fn map_eval_run_store_error(err: EvalRunStoreError) -> ApiError {
    match err {
        EvalRunStoreError::NotFound(id) => ApiError::NotFound(format!("eval run not found: {id}")),
        EvalRunStoreError::InvalidRunId(id) => {
            ApiError::BadRequest(format!("invalid eval run id: {id}"))
        }
        err => ApiError::Internal(err.to_string()),
    }
}

fn eval_run_store_or_unavailable(state: &AppState) -> Result<Arc<dyn EvalRunStore>, ApiError> {
    state
        .eval_run_store()
        .ok_or_else(|| ApiError::ServiceUnavailable("eval run store not configured".into()))
}

fn epoch_secs_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ── Handlers ──────────────────────────────────────────────────────────────

/// `POST /v1/eval/runs` — start, run, persist.
///
/// Synchronous: the response returns *after* every fixture has been
/// replayed. Datasets are typically small (5-50 fixtures) so this fits a
/// single HTTP round-trip. A future enhancement could promote to
/// background execution with `GET /v1/eval/runs/:id` polling.
#[tracing::instrument(skip_all, fields(dataset_id = %body.dataset_id))]
pub async fn start_eval_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<StartRunRequest>,
) -> Result<Response, ApiError> {
    crate::config_routes::ensure_admin_auth(&state, &headers)?;
    let config_store = config_store_or_unavailable(&state)?;
    let run_store = eval_run_store_or_unavailable(&state)?;

    let raw = config_store
        .get(DATASETS_NAMESPACE, &body.dataset_id)
        .await
        .map_err(map_storage_error)?
        .ok_or_else(|| ApiError::NotFound(format!("dataset not found: {}", body.dataset_id)))?;
    let dataset_revision = extract_meta_revision(&raw).unwrap_or(0);
    let record = validate_config_record::<DatasetSpec>(raw)
        .map_err(|err| ApiError::BadRequest(format!("invalid dataset: {err}")))?;
    let fixtures: Vec<Fixture> = record.spec.fixtures;
    if fixtures.is_empty() {
        return Err(ApiError::BadRequest(format!(
            "dataset {} has no fixtures to replay",
            body.dataset_id
        )));
    }
    // Expand the matrix (or 1-cell default for non-matrix runs).
    let limits = state.config.eval_limits.clone();
    let models = body.models.clone().unwrap_or_default();
    let cells = expand_cells(&models);
    let samples = body.samples.unwrap_or(1).max(1);
    if samples > limits.max_samples_per_cell {
        return Err(ApiError::BadRequest(format!(
            "samples={samples} exceeds cap {}",
            limits.max_samples_per_cell
        )));
    }
    // Flakiness sampling only makes sense in Live mode — scripted
    // replays are deterministic, so samples > 1 would just duplicate
    // identical results. Reject it explicitly so the operator notices
    // the misconfiguration instead of silently doubling storage.
    if samples > 1 && body.models.is_none() {
        return Err(ApiError::BadRequest(
            "samples > 1 requires `models` (Live mode); scripted replays are deterministic".into(),
        ));
    }
    let total_units = fixtures.len() * cells.len() * samples as usize;
    if total_units > limits.max_cells_per_sync_run {
        return Err(ApiError::BadRequest(format!(
            "dataset {} × matrix × samples expands to {total_units} units \
             (max {} for synchronous run); split the dataset, \
             shrink the matrix, or drop samples",
            body.dataset_id, limits.max_cells_per_sync_run,
        )));
    }

    // Resolve the judge model once (if configured) so a missing binding
    // fails fast before any replay runs. Judge is also Live-only —
    // scripted runs don't need (or have a good user_prompt for) it.
    let judge = if let Some(ref jr) = body.judge {
        if body.models.is_none() {
            return Err(ApiError::BadRequest(
                "judge requires `models` (Live mode); scripted replays don't use a judge".into(),
            ));
        }
        if let Some(n) = jr.revise_max_retries
            && n > limits.max_judge_revisions
        {
            return Err(ApiError::BadRequest(format!(
                "revise_max_retries={n} exceeds cap {}",
                limits.max_judge_revisions
            )));
        }
        let resolved = resolve_live_executor(&state, &jr.model_id).await?;
        Some(JudgeContext {
            judge: LlmExecutorJudge::new(resolved.executor, resolved.upstream_model),
            rubric: jr.rubric.clone(),
            revise_max_retries: jr.revise_max_retries,
        })
    } else {
        None
    };

    let trace_sink: Option<Arc<dyn MetricsSink>> = state
        .trace_store()
        .map(|store| Arc::new(TraceStoreSink::new(store)) as Arc<dyn MetricsSink>);
    let agent_base = match &body.agent_id {
        Some(id) => Some(crate::services::eval_common::resolve_agent_spec(&state, id).await?),
        None => None,
    };
    let items: Vec<EvalRunItem> = if body.models.is_some() {
        run_matrix_cells(
            &state,
            &fixtures,
            &cells,
            MatrixOptions {
                samples,
                max_concurrent: limits.max_concurrent_matrix_cells,
                agent_base,
                agent_overrides: body.agent_overrides.clone(),
                judge,
            },
            trace_sink,
        )
        .await?
    } else {
        run_scripted_fixtures(&fixtures, trace_sink).await
    };

    let started_at_secs = epoch_secs_now();
    let run = EvalRun {
        id: mint_run_id(),
        dataset_id: body.dataset_id.clone(),
        dataset_revision,
        items,
        started_at_secs,
        ended_at_secs: epoch_secs_now(),
    };
    run_store.write(&run).map_err(map_eval_run_store_error)?;

    // `baseline_run_id` is transient: used here to compute the response
    // diff, never persisted onto the EvalRun. The diff endpoint at
    // GET /v1/eval/runs/:id?baseline= can resurface it at any time
    // against any baseline the operator picks.
    let diff = if let Some(ref baseline_id) = body.baseline_run_id {
        Some(compute_diff(run_store.as_ref(), &run, baseline_id)?)
    } else {
        None
    };

    Ok(Json(EvalRunResponse {
        run,
        diff,
        aggregates: None,
    })
    .into_response())
}

/// `GET /v1/eval/runs` — list run summaries.
#[tracing::instrument(skip_all)]
pub async fn list_eval_runs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ListRunsQuery>,
) -> Result<Response, ApiError> {
    crate::config_routes::ensure_admin_auth(&state, &headers)?;
    let store = eval_run_store_or_unavailable(&state)?;
    let filter = EvalRunFilter {
        dataset_id: params.dataset_id,
        since_secs: params.since_secs,
        until_secs: params.until_secs,
        limit: params.limit,
    };
    let runs = store.list(&filter).map_err(map_eval_run_store_error)?;
    Ok(Json(ListEvalRunsResponse { runs }).into_response())
}

/// `GET /v1/eval/runs/:id` (with optional `?baseline=` for D7).
#[tracing::instrument(skip_all, fields(id = %id))]
pub async fn get_eval_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<GetRunQuery>,
) -> Result<Response, ApiError> {
    crate::config_routes::ensure_admin_auth(&state, &headers)?;
    let store = eval_run_store_or_unavailable(&state)?;
    let run = store.read(&id).map_err(map_eval_run_store_error)?;
    let diff = if let Some(baseline_id) = params.baseline {
        Some(compute_diff(store.as_ref(), &run, &baseline_id)?)
    } else {
        None
    };
    let aggregates = params
        .aggregate
        .map(|RunAggregateKind::Samples| run.aggregate_samples());
    Ok(Json(EvalRunResponse {
        run,
        diff,
        aggregates,
    })
    .into_response())
}

fn compute_diff(
    store: &dyn EvalRunStore,
    new_run: &EvalRun,
    baseline_id: &str,
) -> Result<DiffSummary, ApiError> {
    let baseline = store.read(baseline_id).map_err(|err| match err {
        EvalRunStoreError::NotFound(_) => {
            ApiError::NotFound(format!("baseline eval run not found: {baseline_id}"))
        }
        other => map_eval_run_store_error(other),
    })?;
    // Use matrix-aware pairing when either side carries a cell. Single
    // unified call site — `diff_eval_items` handles the cell-less path
    // identically to the report-based diff.
    let has_matrix = baseline.items.iter().any(|i| i.cell.is_some())
        || new_run.items.iter().any(|i| i.cell.is_some());
    if has_matrix {
        Ok(awaken_eval::diff_eval_items(
            &baseline.items,
            &new_run.items,
        ))
    } else {
        let baseline_reports: Vec<ReplayReport> =
            baseline.items.into_iter().map(|i| i.report).collect();
        let new_reports: Vec<ReplayReport> =
            new_run.items.iter().map(|i| i.report.clone()).collect();
        Ok(diff_against_baseline(&baseline_reports, &new_reports))
    }
}

/// Scripted-mode driver — current behaviour. One outcome per fixture,
/// no cell, no real provider. CI smoke path.
async fn run_scripted_fixtures(
    fixtures: &[Fixture],
    trace_sink: Option<Arc<dyn MetricsSink>>,
) -> Vec<EvalRunItem> {
    let mut replayer = RuntimeReplayer::new();
    if let Some(sink) = trace_sink {
        replayer = replayer.with_tee_sink(sink);
    }
    let outcomes = replay_all(&replayer, fixtures).await;
    outcomes
        .iter()
        .zip(fixtures.iter())
        .map(|(outcome, fixture)| {
            let failures = score(outcome, &fixture.expect);
            let report = ReplayReport::from_outcome(outcome, failures);
            EvalRunItem {
                fixture_id: fixture.id.clone(),
                cell: None,
                report,
                trace_run_id: outcome.trace_run_id().map(str::to_string),
                sample_index: None,
            }
        })
        .collect()
}

/// Per-cell tunables for [`run_matrix_cells`]. Grouped to keep the
/// driver signature below clippy's `too_many_arguments` cap and to
/// signal that these knobs travel together — adding another retry /
/// concurrency parameter belongs here, not as a fresh fn-level arg.
pub(crate) struct MatrixOptions {
    pub samples: u32,
    pub max_concurrent: usize,
    pub agent_base: Option<awaken_contract::registry_spec::AgentSpec>,
    pub agent_overrides: Option<AgentSpecPatch>,
    pub judge: Option<JudgeContext>,
}

/// Matrix-mode driver — Live execution against real providers, one
/// `(fixture, cell, sample)` combination per item. Models are
/// pre-resolved before any provider call so a missing model fails fast
/// (404) instead of burning tokens on the cells that did resolve.
async fn run_matrix_cells(
    state: &AppState,
    fixtures: &[Fixture],
    cells: &[MatrixCell],
    options: MatrixOptions,
    trace_sink: Option<Arc<dyn MetricsSink>>,
) -> Result<Vec<EvalRunItem>, ApiError> {
    let MatrixOptions {
        samples,
        max_concurrent,
        agent_base,
        agent_overrides,
        judge,
    } = options;
    use awaken_contract::contract::executor::LlmExecutor;
    use awaken_contract::registry_spec::ModelBindingSpec;

    // Pre-resolve every model once — same executor reused across all
    // fixtures (and samples) of the same cell. Carry the binding spec
    // forward so we can compute cost_usd post-replay without a second
    // registry lookup.
    let mut resolved: Vec<(MatrixCell, Arc<dyn LlmExecutor>, String, ModelBindingSpec)> =
        Vec::with_capacity(cells.len());
    for cell in cells {
        let model_id = cell
            .model_id
            .as_deref()
            .expect("matrix expansion always sets model_id");
        let r = resolve_live_executor(state, model_id).await?;
        resolved.push((cell.clone(), r.executor, r.upstream_model, r.binding));
    }

    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent));
    let mut handles = Vec::with_capacity(fixtures.len() * resolved.len() * samples as usize);
    // Emit a sample_index only when samples > 1 so single-sample runs
    // keep the same on-disk shape as before the flakiness feature
    // landed (the field stays absent in JSON).
    let emit_sample_index = samples > 1;
    for fixture in fixtures {
        for (cell, executor, upstream_model, binding) in &resolved {
            for sample in 0..samples {
                let fixture = fixture.clone();
                let cell = cell.clone();
                let executor = executor.clone();
                let upstream_model = upstream_model.clone();
                let binding = binding.clone();
                let overrides = agent_overrides.clone();
                let base = agent_base.clone();
                let trace_sink = trace_sink.clone();
                let revise_for_task = revise_tuple_for(judge.as_ref(), &fixture.expect);
                let permit = semaphore.clone().acquire_owned().await.expect("semaphore");
                handles.push(tokio::spawn(async move {
                    let _permit = permit;
                    let mut builder =
                        RuntimeReplayer::new().with_live_executor(executor, upstream_model);
                    if let Some(b) = base {
                        builder = builder.with_agent_base(b);
                    }
                    let replayer =
                        apply_cell_decorators(builder, overrides, trace_sink, revise_for_task);
                    let outcomes = replay_all(&replayer, std::slice::from_ref(&fixture)).await;
                    let outcome = outcomes
                        .into_iter()
                        .next()
                        .expect("one fixture → one outcome");
                    (fixture, cell, sample, outcome, binding)
                }));
            }
        }
    }

    let mut items: Vec<EvalRunItem> = Vec::with_capacity(handles.len());
    for handle in handles {
        let (fixture, cell, sample, outcome, binding) = handle
            .await
            .map_err(|err| ApiError::Internal(format!("matrix cell task panicked: {err}")))?;
        let failures = score_outcome(&outcome, &fixture, judge.as_ref()).await?;
        let mut report = ReplayReport::from_outcome(&outcome, failures);
        report.cost_usd =
            binding.compute_cost_usd(report.total_input_tokens, report.total_output_tokens);
        items.push(EvalRunItem {
            fixture_id: fixture.id,
            cell: Some(cell),
            report,
            trace_run_id: outcome.trace_run_id().map(str::to_string),
            sample_index: if emit_sample_index {
                Some(sample)
            } else {
                None
            },
        });
    }
    Ok(items)
}
