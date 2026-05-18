//! `/v1/eval/datasets` CRUD + `POST /v1/eval/datasets/:id/items { from_run_id }`
//! (ADR-0032 D6).
//!
//! Datasets are [`DatasetSpec`] records stored in the same
//! [`ConfigStore`] that holds `AgentSpec` / `ToolSpec`. The handlers wrap
//! every record in [`ConfigRecord<DatasetSpec>`] so revision-aware writes
//! ([`ConfigStore::put_if_revision`]) protect against concurrent admin
//! edits. The `items` endpoint reads a [`TraceStore`] run and curates a
//! [`Fixture`] from it via [`trace_to_provider_script`] (ADR-0032 D5),
//! appending the result to the dataset's fixture list.

use awaken_contract::config_record::{ConfigRecord, RecordMeta, validate_config_record};
use awaken_contract::contract::config_store::extract_meta_revision;
use awaken_eval::{
    DATASETS_NAMESPACE, DatasetSpec, Expectation, Fixture, MockResponse, trace_to_provider_script,
};
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::error::ApiError;
use crate::services::eval_common::{
    config_store_or_unavailable, map_storage_error, map_trace_store_error,
};

// `DATASETS_NAMESPACE` re-exported from `awaken_eval::dataset` is the
// single source of truth — see the const's docstring there.

// ── Wire types ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct DatasetSummaryWire {
    pub id: String,
    pub description: String,
    pub fixture_count: usize,
    pub revision: u64,
}

#[derive(Debug, Serialize)]
pub struct ListDatasetsResponse {
    pub datasets: Vec<DatasetSummaryWire>,
}

/// Body for `POST /v1/eval/datasets/:id/items { from_run_id, user_input }`.
///
/// `from_run_id` identifies a run in the [`TraceStore`]. `user_input` is
/// optional: when omitted, the server falls back to the user message
/// recovered from the trace's captured `request_messages` (requires the
/// originating run to have had `ContentCapture::Enabled`). Explicit
/// `user_input` always wins so operators can rephrase prompts.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurateItemsRequest {
    pub from_run_id: String,
    #[serde(default)]
    pub user_input: Option<String>,
    /// Optional fixture id. Defaults to the run id so the curated
    /// fixture's provenance is unambiguous in the dataset.
    #[serde(default)]
    pub fixture_id: Option<String>,
    /// Optional description for the curated fixture; defaults to a
    /// "curated from trace …" string.
    #[serde(default)]
    pub description: Option<String>,
    /// Mirrors `Fixture::allow_unused_provider_script`. Default `false`.
    #[serde(default)]
    pub allow_unused_provider_script: bool,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    100
}

// Error mappers and store accessors live in `services::eval_common` so
// dataset_service and eval_run_service cannot drift.

// ── Handlers ──────────────────────────────────────────────────────────────

/// `GET /v1/eval/datasets` — list dataset summaries.
#[tracing::instrument(skip_all)]
pub async fn list_datasets(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ListParams>,
) -> Result<Response, ApiError> {
    crate::config_routes::ensure_admin_auth(&state, &headers)?;
    let store = config_store_or_unavailable(&state)?;
    let raw = store
        .list(DATASETS_NAMESPACE, params.offset, params.limit)
        .await
        .map_err(map_storage_error)?;
    let mut datasets = Vec::with_capacity(raw.len());
    for (id, value) in raw {
        // A malformed record blocks the whole list — better to surface
        // it than to silently drop a dataset and let the operator wonder
        // where it went.
        let record: ConfigRecord<DatasetSpec> =
            validate_config_record(value).map_err(|err| ApiError::Internal(err.to_string()))?;
        datasets.push(DatasetSummaryWire {
            id,
            description: record.spec.description,
            fixture_count: record.spec.fixtures.len(),
            revision: record.meta.revision,
        });
    }
    Ok(Json(ListDatasetsResponse { datasets }).into_response())
}

/// `POST /v1/eval/datasets` — create a dataset. Body is a [`DatasetSpec`]
/// JSON. The dataset id is taken from the body's `"id"` field, with a
/// fallback to a `?id=` query param to keep the wire shape consistent
/// with the rest of the config CRUD surface.
#[tracing::instrument(skip_all, fields(id = ?id_param.id))]
pub async fn create_dataset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(id_param): Query<IdParam>,
    Json(body): Json<CreateDatasetRequest>,
) -> Result<Response, ApiError> {
    crate::config_routes::ensure_admin_auth(&state, &headers)?;
    let store = config_store_or_unavailable(&state)?;
    let id = id_param
        .id
        .or(body.id.clone())
        .ok_or_else(|| ApiError::BadRequest("dataset id is required (in body or ?id=)".into()))?;
    let record = ConfigRecord {
        spec: body.spec,
        meta: RecordMeta::new_user(),
    };
    let value = record
        .to_value()
        .map_err(|err| ApiError::Internal(err.to_string()))?;
    // put_if_absent so re-POSTing the same id surfaces a Conflict instead
    // of silently clobbering an existing dataset.
    store
        .put_if_absent(DATASETS_NAMESPACE, &id, &value)
        .await
        .map_err(map_storage_error)?;
    Ok((StatusCode::CREATED, Json(record)).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateDatasetRequest {
    /// Optional id. When omitted, falls back to `?id=` query param.
    #[serde(default)]
    pub id: Option<String>,
    pub spec: DatasetSpec,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct IdParam {
    #[serde(default)]
    pub id: Option<String>,
}

/// `GET /v1/eval/datasets/:id` — fetch one dataset record.
#[tracing::instrument(skip_all, fields(id = %id))]
pub async fn get_dataset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    crate::config_routes::ensure_admin_auth(&state, &headers)?;
    let store = config_store_or_unavailable(&state)?;
    let value = store
        .get(DATASETS_NAMESPACE, &id)
        .await
        .map_err(map_storage_error)?
        .ok_or_else(|| ApiError::NotFound(format!("dataset not found: {id}")))?;
    let record: ConfigRecord<DatasetSpec> =
        validate_config_record(value).map_err(|err| ApiError::Internal(err.to_string()))?;
    Ok(Json(record).into_response())
}

/// `PUT /v1/eval/datasets/:id` — replace the dataset. Body carries the
/// expected revision (read first, then write) so concurrent edits collide
/// as `409 Conflict` instead of last-write-wins.
#[tracing::instrument(skip_all, fields(id = %id))]
pub async fn put_dataset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<PutDatasetRequest>,
) -> Result<Response, ApiError> {
    crate::config_routes::ensure_admin_auth(&state, &headers)?;
    let store = config_store_or_unavailable(&state)?;
    let mut meta = match store
        .get(DATASETS_NAMESPACE, &id)
        .await
        .map_err(map_storage_error)?
    {
        Some(existing) => {
            let existing_revision = extract_meta_revision(&existing).unwrap_or(0);
            if existing_revision != body.expected_revision {
                return Err(ApiError::Conflict(format!(
                    "revision conflict: expected {}, actual {existing_revision}",
                    body.expected_revision
                )));
            }
            let prior: ConfigRecord<DatasetSpec> = validate_config_record(existing)
                .map_err(|err| ApiError::Internal(err.to_string()))?;
            prior.meta
        }
        None if body.expected_revision == 0 => RecordMeta::new_user(),
        None => {
            return Err(ApiError::NotFound(format!("dataset not found: {id}")));
        }
    };
    let now = awaken_contract::time::now_ms();
    meta.updated_at = now;
    meta.revision = meta.revision.saturating_add(1);
    let record = ConfigRecord {
        spec: body.spec,
        meta,
    };
    let value = record
        .to_value()
        .map_err(|err| ApiError::Internal(err.to_string()))?;
    store
        .put_if_revision(DATASETS_NAMESPACE, &id, &value, body.expected_revision)
        .await
        .map_err(map_storage_error)?;
    Ok(Json(record).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PutDatasetRequest {
    pub expected_revision: u64,
    pub spec: DatasetSpec,
}

/// `DELETE /v1/eval/datasets/:id` — remove the dataset. Idempotent.
#[tracing::instrument(skip_all, fields(id = %id))]
pub async fn delete_dataset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    crate::config_routes::ensure_admin_auth(&state, &headers)?;
    let store = config_store_or_unavailable(&state)?;
    store
        .delete(DATASETS_NAMESPACE, &id)
        .await
        .map_err(map_storage_error)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `POST /v1/eval/datasets/:id/items` — curate a fixture from a trace run
/// and append it to the dataset (ADR-0032 D5+D6c).
///
/// Read-modify-write under the dataset's current revision. A concurrent
/// edit between `get` and `put_if_revision` surfaces as `409 Conflict`.
#[tracing::instrument(skip_all, fields(id = %id, run_id = %body.from_run_id))]
pub async fn curate_items(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<CurateItemsRequest>,
) -> Result<Response, ApiError> {
    crate::config_routes::ensure_admin_auth(&state, &headers)?;
    let store = config_store_or_unavailable(&state)?;
    let trace_store = state
        .trace_store()
        .ok_or_else(|| ApiError::ServiceUnavailable("trace store not configured".into()))?;

    let events = trace_store
        .read(&body.from_run_id)
        .map_err(map_trace_store_error)?;
    let conversion = trace_to_provider_script(&events)
        .map_err(|err| ApiError::BadRequest(format!("curating trace: {err}")))?;

    let existing_value = store
        .get(DATASETS_NAMESPACE, &id)
        .await
        .map_err(map_storage_error)?
        .ok_or_else(|| ApiError::NotFound(format!("dataset not found: {id}")))?;
    let existing_revision = extract_meta_revision(&existing_value).unwrap_or(0);
    let mut record: ConfigRecord<DatasetSpec> = validate_config_record(existing_value)
        .map_err(|err| ApiError::Internal(err.to_string()))?;

    let fixture_id = body.fixture_id.unwrap_or_else(|| body.from_run_id.clone());
    if record.spec.fixtures.iter().any(|f| f.id == fixture_id) {
        return Err(ApiError::Conflict(format!(
            "dataset already has fixture {fixture_id}"
        )));
    }
    let user_input = body
        .user_input
        .or(conversion.user_input.clone())
        .ok_or_else(|| {
            ApiError::BadRequest(
                "user_input is required: trace did not capture request_messages — \
                 enable ContentCapture::Enabled on the run, or supply user_input in the body"
                    .into(),
            )
        })?;
    let fixture = Fixture {
        id: fixture_id.clone(),
        description: Some(
            body.description
                .unwrap_or_else(|| format!("Curated from trace {}", body.from_run_id)),
        ),
        user_input,
        provider_script: conversion.provider_script,
        source_run_id: Some(body.from_run_id),
        source_model_id: conversion.source_model_id,
        allow_unused_provider_script: body.allow_unused_provider_script,
        mock_response: MockResponse::default(),
        expect: Expectation::default(),
    };
    record.spec.fixtures.push(fixture);

    let now = awaken_contract::time::now_ms();
    record.meta.updated_at = now;
    record.meta.revision = record.meta.revision.saturating_add(1);
    let value = record
        .to_value()
        .map_err(|err| ApiError::Internal(err.to_string()))?;
    store
        .put_if_revision(DATASETS_NAMESPACE, &id, &value, existing_revision)
        .await
        .map_err(map_storage_error)?;

    Ok((StatusCode::CREATED, Json(record)).into_response())
}
