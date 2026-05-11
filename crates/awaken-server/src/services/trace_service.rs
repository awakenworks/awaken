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

// ── Error wrapper ──────────────────────────────────────────────────────────

/// Converts `TraceStoreError` to an HTTP response.
pub struct TraceAppError(pub TraceStoreError);

impl IntoResponse for TraceAppError {
    fn into_response(self) -> Response {
        let (status, msg) = match self.0 {
            TraceStoreError::NotFound { run_id } => {
                (StatusCode::NOT_FOUND, format!("trace not found: {run_id}"))
            }
            TraceStoreError::InvalidRunId(id) => {
                (StatusCode::BAD_REQUEST, format!("invalid run id: {id}"))
            }
            err => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
        };
        (status, Json(json!({ "error": msg }))).into_response()
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
) -> Response {
    if let Err(err) = crate::config_routes::ensure_admin_auth(&state, &headers) {
        return err.into_response();
    }
    let Some(store) = state.trace_store() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "trace store not configured" })),
        )
            .into_response();
    };

    let since = match params.since.as_deref() {
        None => None,
        Some(s) => match chrono::DateTime::parse_from_rfc3339(s) {
            Ok(dt) => Some(std::time::SystemTime::from(dt)),
            Err(err) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": "invalid `since` query parameter; expected RFC 3339 timestamp",
                        "detail": err.to_string(),
                    })),
                )
                    .into_response();
            }
        },
    };

    // Reject `limit=0` symmetrically with the event endpoint; `Some(0)`
    // would otherwise reach the store and silently return an empty list,
    // which is indistinguishable from "no runs matched". `None` falls
    // through to the store's default page size.
    if matches!(params.limit, Some(0)) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "`limit` must be >= 1" })),
        )
            .into_response();
    }

    let filter = TraceFilter {
        agent_id: params.agent_id,
        prompt_id: params.prompt_id,
        experiment_id: params.experiment_id,
        variant_name: params.variant_name,
        since,
        limit: params.limit,
    };

    match store.list(&filter) {
        Ok(summaries) => {
            let runs: Vec<RunSummaryWire> =
                summaries.into_iter().map(RunSummaryWire::from).collect();
            Json(ListTracesResponse { runs }).into_response()
        }
        Err(err) => TraceAppError(err).into_response(),
    }
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
) -> Response {
    if let Err(err) = crate::config_routes::ensure_admin_auth(&state, &headers) {
        return err.into_response();
    }
    let Some(store) = state.trace_store() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "trace store not configured" })),
        )
            .into_response();
    };

    let offset = params.offset.unwrap_or(0);
    // F17: clamp `limit` to a positive value. `limit=0` would freeze a
    // client in an infinite pagination loop because `x-trace-next-offset`
    // would keep returning the same offset without ever advancing past it.
    // Lower bound is 1, upper bound is `DEFAULT_TRACE_EVENT_PAGE`.
    let raw_limit = params.limit.unwrap_or(DEFAULT_TRACE_EVENT_PAGE);
    if raw_limit == 0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "`limit` must be >= 1",
            })),
        )
            .into_response();
    }
    let limit = raw_limit.min(DEFAULT_TRACE_EVENT_PAGE);

    // KNOWN LIMITATION: `store.read` materialises every event for the run
    // before pagination slices in memory. The per-run sampling buffer cap
    // (10k events) bounds this in the sampled path, but a misbehaving
    // direct writer could still produce a large vector here. A follow-up
    // adds `TraceStore::read_page(run_id, offset, limit)` so pagination
    // is also a storage-layer operation.
    match store.read(&run_id) {
        Ok(events) => {
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
            builder
                .body(Body::from(buf))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(err) => TraceAppError(err).into_response(),
    }
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
) -> Response {
    if let Err(err) = crate::config_routes::ensure_admin_auth(&state, &headers) {
        return err.into_response();
    }
    let Some(store) = state.trace_store() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "trace store not configured" })),
        )
            .into_response();
    };

    match store.mark_referenced(&run_id, ReferenceKind::OperatorPin) {
        Ok(()) => (StatusCode::NO_CONTENT).into_response(),
        Err(err) => TraceAppError(err).into_response(),
    }
}
