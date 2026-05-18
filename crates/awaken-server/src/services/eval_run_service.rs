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
    EvalRunStore, EvalRunStoreError, EvalRunSummary, Fixture, MatrixCell, ReplayReport,
    RuntimeReplayer, diff_against_baseline, expand_cells, mint_run_id, replay_all, score,
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

/// Soft cap on TOTAL replay cells (fixtures × matrix size) per
/// synchronous run. Replays are in-process and synchronous — long
/// runs would hold the HTTP connection past nginx-default 60s. Above
/// this count, the endpoint returns 400 with a "split the dataset or
/// shrink the matrix" hint. Sized for typical regression suites:
/// 50 fixtures × 2 models = 100 cells.
const MAX_CELLS_PER_SYNC_RUN: usize = 100;

/// Per-cell concurrency cap in matrix runs. Bounds the burst put on
/// rate-limited upstream providers. Five matches the online endpoint's
/// cap and most paid-tier rate limits.
const MAX_CONCURRENT_MATRIX_CELLS: usize = 5;

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
    /// Optional `AgentSpecPatch` applied to every fixture's synthetic
    /// agent. Only takes effect in Live mode; ignored on Scripted runs
    /// (scripted agents are fixed). Reuses `ConfigRecord`'s
    /// `AgentSpecPatch` machinery so operators get the same
    /// `deny_unknown_fields` validation they get on `PATCH /v1/config/agents`.
    #[serde(default)]
    pub agent_overrides: Option<AgentSpecPatch>,
}

#[derive(Debug, Serialize)]
pub struct EvalRunResponse {
    pub run: EvalRun,
    /// Present only when [`StartRunRequest::baseline_run_id`] or the
    /// `?baseline=` query param resolved to a real prior run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffSummary>,
}

#[derive(Debug, Serialize)]
pub struct ListEvalRunsResponse {
    pub runs: Vec<EvalRunSummary>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ListRunsQuery {
    pub dataset_id: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GetRunQuery {
    /// When set, the response includes a `diff` against the named run.
    /// The baseline must exist or the request 404s (caller passed an
    /// invalid id — silent omission would mask the typo).
    pub baseline: Option<String>,
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
    let models = body.models.clone().unwrap_or_default();
    let cells = expand_cells(&models);
    let total_cells = fixtures.len() * cells.len();
    if total_cells > MAX_CELLS_PER_SYNC_RUN {
        return Err(ApiError::BadRequest(format!(
            "dataset {} × matrix expands to {total_cells} cells (max {MAX_CELLS_PER_SYNC_RUN} \
             for synchronous run); split the dataset or shrink the matrix",
            body.dataset_id,
        )));
    }

    let trace_sink: Option<Arc<dyn MetricsSink>> = state
        .trace_store()
        .map(|store| Arc::new(TraceStoreSink::new(store)) as Arc<dyn MetricsSink>);
    let items: Vec<EvalRunItem> = if body.models.is_some() {
        run_matrix_cells(
            &state,
            &fixtures,
            &cells,
            body.agent_overrides.clone(),
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

    Ok(Json(EvalRunResponse { run, diff }).into_response())
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
    Ok(Json(EvalRunResponse { run, diff }).into_response())
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
            }
        })
        .collect()
}

/// Matrix-mode driver — Live execution against real providers, one
/// `(fixture, cell)` combination per item. Models are pre-resolved
/// before any provider call so a missing model fails fast (404)
/// instead of burning tokens on the cells that did resolve.
async fn run_matrix_cells(
    state: &AppState,
    fixtures: &[Fixture],
    cells: &[MatrixCell],
    agent_overrides: Option<AgentSpecPatch>,
    trace_sink: Option<Arc<dyn MetricsSink>>,
) -> Result<Vec<EvalRunItem>, ApiError> {
    use awaken_contract::contract::executor::LlmExecutor;

    // Pre-resolve every model once — same executor reused across all
    // fixtures of the same cell.
    let mut resolved: Vec<(MatrixCell, Arc<dyn LlmExecutor>, String)> =
        Vec::with_capacity(cells.len());
    for cell in cells {
        let model_id = cell
            .model_id
            .as_deref()
            .expect("matrix expansion always sets model_id");
        let (executor, upstream_model) = resolve_live_executor(state, model_id).await?;
        resolved.push((cell.clone(), executor, upstream_model));
    }

    let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_MATRIX_CELLS));
    let mut handles = Vec::with_capacity(fixtures.len() * resolved.len());
    for fixture in fixtures {
        for (cell, executor, upstream_model) in &resolved {
            let fixture = fixture.clone();
            let cell = cell.clone();
            let executor = executor.clone();
            let upstream_model = upstream_model.clone();
            let overrides = agent_overrides.clone();
            let trace_sink = trace_sink.clone();
            let permit = semaphore.clone().acquire_owned().await.expect("semaphore");
            handles.push(tokio::spawn(async move {
                let _permit = permit;
                let mut replayer =
                    RuntimeReplayer::new().with_live_executor(executor, upstream_model);
                if let Some(p) = overrides {
                    replayer = replayer.with_agent_overrides(p);
                }
                if let Some(sink) = trace_sink {
                    replayer = replayer.with_tee_sink(sink);
                }
                let outcomes = replay_all(&replayer, std::slice::from_ref(&fixture)).await;
                let outcome = outcomes
                    .into_iter()
                    .next()
                    .expect("one fixture → one outcome");
                (fixture, cell, outcome)
            }));
        }
    }

    let mut items: Vec<EvalRunItem> = Vec::with_capacity(handles.len());
    for handle in handles {
        let (fixture, cell, outcome) = handle
            .await
            .map_err(|err| ApiError::Internal(format!("matrix cell task panicked: {err}")))?;
        let failures = score(&outcome, &fixture.expect);
        let report = ReplayReport::from_outcome(&outcome, failures);
        items.push(EvalRunItem {
            fixture_id: fixture.id,
            cell: Some(cell),
            report,
            trace_run_id: outcome.trace_run_id().map(str::to_string),
        });
    }
    Ok(items)
}
