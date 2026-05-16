//! ADR-0030 D7: GET /v1/traces, GET /v1/traces/:run_id, POST /v1/traces/:run_id/pin.

use std::time::UNIX_EPOCH;

use awaken_ext_observability::trace_store::{
    ReferenceKind, RunSummary, TraceFilter, TraceStoreError,
};
use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::app::AppState;
use crate::error::ApiError;

// ── Wire type ──────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct RunSummaryWire {
    pub run_id: String,
    pub agent_id: String,
    /// Seconds since Unix epoch.
    pub started_at: u64,
    pub ended_at: Option<u64>,
    pub prompt_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experiment_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub judge_score: Option<f32>,
}

impl From<RunSummary> for RunSummaryWire {
    fn from(s: RunSummary) -> Self {
        let started_at = s
            .started_at
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let ended_at = s
            .ended_at
            .and_then(|t| t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).ok());
        Self {
            run_id: s.run_id,
            agent_id: s.agent_id,
            started_at,
            ended_at,
            prompt_ids: s.prompt_ids,
            experiment_id: s.experiment_id,
            variant_name: s.variant_name,
            final_status: s.final_status,
            judge_score: s.judge_score,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ListTracesResponse {
    pub runs: Vec<RunSummaryWire>,
}

// ── Query params ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ListTracesQuery {
    pub agent_id: Option<String>,
    pub prompt_id: Option<String>,
    pub experiment_id: Option<String>,
    pub variant_name: Option<String>,
    pub limit: Option<usize>,
    /// RFC 3339 timestamp; the handler parses this into a `SystemTime` and
    /// filters out runs whose `started_at` is older. Invalid values return
    /// HTTP 400 — never silently ignored.
    pub since: Option<String>,
}

// ── Error mapping ──────────────────────────────────────────────────────────

fn map_trace_store_error(err: TraceStoreError) -> ApiError {
    match err {
        TraceStoreError::NotFound { run_id } => {
            ApiError::NotFound(format!("trace not found: {run_id}"))
        }
        TraceStoreError::InvalidRunId(id) => ApiError::BadRequest(format!("invalid run id: {id}")),
        err => ApiError::Internal(err.to_string()),
    }
}

// ── Handlers ──────────────────────────────────────────────────────────────

/// `GET /v1/traces` — list runs matching optional filters.
//
// `skip_all` is intentional — `headers: HeaderMap` carries the
// `Authorization` bearer token, which would otherwise be Debug-printed
// into every tracing event under this span.
#[tracing::instrument(skip_all, fields(agent_id = ?params.agent_id))]
pub async fn list_traces(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Query(params): Query<ListTracesQuery>,
) -> Result<Response, ApiError> {
    crate::config_routes::ensure_admin_auth(&state, &headers)?;
    let store = state
        .trace_store()
        .ok_or_else(|| ApiError::ServiceUnavailable("trace store not configured".into()))?;

    // The `since` parse error preserves the original {error, detail}
    // wire shape; clients depend on the two-field split.
    let since = match params.since.as_deref() {
        None => None,
        Some(s) => match chrono::DateTime::parse_from_rfc3339(s) {
            Ok(dt) => Some(std::time::SystemTime::from(dt)),
            Err(err) => {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": "invalid `since` query parameter; expected RFC 3339 timestamp",
                        "detail": err.to_string(),
                    })),
                )
                    .into_response());
            }
        },
    };

    // Reject `limit=0` symmetrically with the event endpoint; `Some(0)`
    // would otherwise reach the store and silently return an empty list,
    // which is indistinguishable from "no runs matched". `None` falls
    // through to the store's default page size.
    if matches!(params.limit, Some(0)) {
        return Err(ApiError::BadRequest("`limit` must be >= 1".into()));
    }

    let filter = TraceFilter {
        agent_id: params.agent_id,
        prompt_id: params.prompt_id,
        experiment_id: params.experiment_id,
        variant_name: params.variant_name,
        since,
        limit: params.limit,
    };

    let summaries = store.list(&filter).map_err(map_trace_store_error)?;
    let runs: Vec<RunSummaryWire> = summaries.into_iter().map(RunSummaryWire::from).collect();
    Ok(Json(ListTracesResponse { runs }).into_response())
}

/// Per-page event cap for `GET /v1/traces/:run_id`.  Trades response
/// memory for round-trip count.  A run that has been allowed through the
/// sampling-buffer cap of `MAX_BUFFERED_EVENTS_PER_RUN` (10_000) returns
/// in at most 10 pages.
const DEFAULT_TRACE_EVENT_PAGE: usize = 1_000;

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GetTraceQuery {
    /// Zero-based event index to start from.  Defaults to 0.
    pub offset: Option<usize>,
    /// Maximum events to return; capped at `DEFAULT_TRACE_EVENT_PAGE`.
    pub limit: Option<usize>,
}

/// `GET /v1/traces/:run_id` — return an NDJSON page of events for a run.
//
// `skip_all` keeps the bearer header out of trace logs; `run_id` is fine
// to surface because it is path-bound and not a credential. The
// `x-trace-next-offset` / `x-trace-total-events` headers cap the **response**
// size so a client cannot pull more than `DEFAULT_TRACE_EVENT_PAGE` events
// in one round-trip. Server-side memory is **not** bounded by pagination:
// see the KNOWN LIMITATION note inside the handler — `store.read` still
// materialises the whole run before slicing. Storage-level pagination is
// tracked as a follow-up (`TraceStore::read_page`).
#[tracing::instrument(skip_all, fields(run_id = %run_id))]
pub async fn get_trace(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(run_id): Path<String>,
    Query(params): Query<GetTraceQuery>,
) -> Result<Response, ApiError> {
    crate::config_routes::ensure_admin_auth(&state, &headers)?;
    let store = state
        .trace_store()
        .ok_or_else(|| ApiError::ServiceUnavailable("trace store not configured".into()))?;

    let offset = params.offset.unwrap_or(0);
    // F17: clamp `limit` to a positive value. `limit=0` would freeze a
    // client in an infinite pagination loop because `x-trace-next-offset`
    // would keep returning the same offset without ever advancing past it.
    // Lower bound is 1, upper bound is `DEFAULT_TRACE_EVENT_PAGE`.
    let raw_limit = params.limit.unwrap_or(DEFAULT_TRACE_EVENT_PAGE);
    if raw_limit == 0 {
        return Err(ApiError::BadRequest("`limit` must be >= 1".into()));
    }
    let limit = raw_limit.min(DEFAULT_TRACE_EVENT_PAGE);

    // KNOWN LIMITATION: `store.read` materialises every event for the run
    // before pagination slices in memory. The per-run sampling buffer cap
    // (10k events) bounds this in the sampled path, but a misbehaving
    // direct writer could still produce a large vector here. A follow-up
    // adds `TraceStore::read_page(run_id, offset, limit)` so pagination
    // is also a storage-layer operation.
    let events = store.read(&run_id).map_err(map_trace_store_error)?;
    let total = events.len();
    let end = offset.saturating_add(limit).min(total);
    let page = events.get(offset..end).unwrap_or(&[]);
    // Serialise as NDJSON — one JSON line per event, terminated by '\n'.
    let mut buf = String::new();
    for event in page {
        match serde_json::to_string(event) {
            Ok(line) => {
                buf.push_str(&line);
                buf.push('\n');
            }
            Err(err) => {
                tracing::warn!(run_id = %run_id, error = %err, "failed to serialise trace event");
            }
        }
    }
    // An empty result from a known run returns 200 with an empty body.
    // `TraceStoreError::NotFound` is returned by `read` for a missing run.
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .header("x-trace-total-events", total.to_string());
    if end < total {
        builder = builder.header("x-trace-next-offset", end.to_string());
    }
    let resp = builder
        .body(Body::from(buf))
        .map_err(|e| ApiError::Internal(format!("response build failed: {e}")))?;
    Ok(resp)
}

/// `POST /v1/traces/:run_id/pin` — mark a run as operator-pinned so it is
/// exempt from the retention pruner.
//
// `skip_all` keeps the bearer header out of trace logs.
#[tracing::instrument(skip_all, fields(run_id = %run_id))]
pub async fn pin_trace(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Path(run_id): Path<String>,
) -> Result<Response, ApiError> {
    crate::config_routes::ensure_admin_auth(&state, &headers)?;
    let store = state
        .trace_store()
        .ok_or_else(|| ApiError::ServiceUnavailable("trace store not configured".into()))?;

    store
        .mark_referenced(&run_id, ReferenceKind::OperatorPin)
        .map_err(map_trace_store_error)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
