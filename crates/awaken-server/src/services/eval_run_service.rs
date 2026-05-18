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

use awaken_contract::config_record::validate_config_record;
use awaken_contract::contract::config_store::extract_meta_revision;
use awaken_eval::{
    DATASETS_NAMESPACE, DatasetSpec, DiffSummary, EvalRun, EvalRunFilter, EvalRunItem,
    EvalRunStore, EvalRunStoreError, EvalRunSummary, Fixture, ReplayReport, RuntimeReplayer,
    diff_against_baseline, mint_run_id, replay_all, score,
};
use awaken_ext_observability::trace_store::TraceStoreSink;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::error::ApiError;
use crate::services::eval_common::{config_store_or_unavailable, map_storage_error};

// `DATASETS_NAMESPACE` re-exported from `awaken_eval::dataset`.

/// Soft cap on dataset size for synchronous replay. Replays are
/// in-process and synchronous — a long-running dataset would hold the
/// HTTP connection open past most ingress timeouts (nginx default 60s).
/// Above this size, the endpoint returns 400 with a "split the dataset"
/// hint instead of silently blocking. Sized for typical regression
/// suites (5-50 fixtures, each ~1s of replay).
const MAX_FIXTURES_PER_SYNC_RUN: usize = 100;

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
    if fixtures.len() > MAX_FIXTURES_PER_SYNC_RUN {
        return Err(ApiError::BadRequest(format!(
            "dataset {} has {} fixtures (max {} for synchronous replay); \
             split into smaller datasets",
            body.dataset_id,
            fixtures.len(),
            MAX_FIXTURES_PER_SYNC_RUN,
        )));
    }

    // Tee replay spans into the server's TraceStore when one is
    // attached so the admin UI can pivot from an EvalRunItem to the
    // full per-run trace. When no TraceStore is wired, fall back to
    // the in-memory aggregation only and leave `trace_run_id` empty.
    let mut replayer = RuntimeReplayer::new();
    if let Some(trace_store) = state.trace_store() {
        replayer = replayer.with_tee_sink(Arc::new(TraceStoreSink::new(trace_store)));
    }
    let outcomes = replay_all(&replayer, &fixtures).await;
    let items: Vec<EvalRunItem> = outcomes
        .iter()
        .zip(fixtures.iter())
        .map(|(outcome, fixture)| {
            let failures = score(outcome, &fixture.expect);
            let report = ReplayReport::from_outcome(outcome, failures);
            EvalRunItem {
                fixture_id: fixture.id.clone(),
                report,
                trace_run_id: outcome.trace_run_id().map(str::to_string),
            }
        })
        .collect();

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
    let baseline_reports: Vec<ReplayReport> =
        baseline.items.into_iter().map(|i| i.report).collect();
    let new_reports: Vec<ReplayReport> = new_run.items.iter().map(|i| i.report.clone()).collect();
    Ok(diff_against_baseline(&baseline_reports, &new_reports))
}
