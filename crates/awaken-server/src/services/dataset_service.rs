//! `/v1/eval/datasets` CRUD + `POST /v1/eval/datasets/:id/items { from_run_id }`
//! (ADR-0032 D6).
//!
//! Datasets are [`DatasetSpec`] records stored in the same
//! [`ConfigStore`] that holds `AgentSpec` / `ToolSpec`. The handlers wrap
//! every record in [`ConfigRecord<DatasetSpec>`] so revision-aware writes
//! ([`ConfigStore::put_if_revision`]) protect against concurrent admin
//! edits. The `items` endpoint reads a [`TraceStore`] run and curates a
//! [`Fixture`] from recovered trace metadata and, when requested/possible,
//! a `provider_script` snapshot (ADR-0032 D5), appending the result to
//! the dataset's fixture list.

use awaken_contract::config_record::{ConfigRecord, RecordMeta, validate_config_record};
use awaken_contract::contract::config_store::extract_meta_revision;
use awaken_eval::fixture::DialogueTurn;
use awaken_eval::{
    CurateError, DATASETS_NAMESPACE, DatasetSpec, Expectation, Fixture, MockResponse,
    trace_fixture_source, trace_to_provider_script,
};
use awaken_ext_observability::trace_store::{ReferenceKind, TraceStore};
use awaken_runtime::engine::ProviderScriptEvent;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::app::AppState;
use crate::error::ApiError;
use crate::services::dataset_wire::{
    AppendFixtureRequest, CreateDatasetRequest, CurateItemsRequest, DatasetSummaryWire, IdParam,
    ImportDialogueRequest, ImportDialogueResponse, ImportTracesRequest, ImportTracesResponse,
    ListDatasetsResponse, ListParams, ProviderScriptMode, PutDatasetRequest,
};
use crate::services::eval_common::{
    config_store_or_unavailable, map_storage_error, map_trace_store_error,
};

// `DATASETS_NAMESPACE` re-exported from `awaken_eval::dataset` is the
// single source of truth — see the const's docstring there.

fn require_non_empty_expected(expect: &Expectation) -> Result<(), ApiError> {
    if expect.is_empty() {
        return Err(ApiError::BadRequest(
            "expected must contain at least one expectation criterion".into(),
        ));
    }
    Ok(())
}

fn mark_dataset_trace_reference(
    trace_store: &dyn TraceStore,
    run_id: &str,
) -> Result<(), ApiError> {
    trace_store
        .mark_referenced(run_id, ReferenceKind::Dataset)
        .map_err(map_trace_store_error)
}

struct CuratedTracePayload {
    user_input: Option<String>,
    source_model_id: Option<String>,
    provider_script: Vec<ProviderScriptEvent>,
    provider_script_error: Option<String>,
}

fn curate_trace_payload(
    events: &[awaken_ext_observability::MetricsEvent],
    mode: ProviderScriptMode,
) -> Result<CuratedTracePayload, CurateError> {
    let source = trace_fixture_source(events)?;
    match mode {
        ProviderScriptMode::Require => {
            let conversion = trace_to_provider_script(events)?;
            Ok(CuratedTracePayload {
                user_input: conversion.user_input,
                source_model_id: conversion.source_model_id,
                provider_script: conversion.provider_script,
                provider_script_error: None,
            })
        }
        ProviderScriptMode::Optional => match trace_to_provider_script(events) {
            Ok(conversion) => Ok(CuratedTracePayload {
                user_input: conversion.user_input,
                source_model_id: conversion.source_model_id,
                provider_script: conversion.provider_script,
                provider_script_error: None,
            }),
            Err(err) => Ok(CuratedTracePayload {
                user_input: source.user_input,
                source_model_id: source.source_model_id,
                provider_script: Vec::new(),
                provider_script_error: Some(err.to_string()),
            }),
        },
        ProviderScriptMode::Skip => Ok(CuratedTracePayload {
            user_input: source.user_input,
            source_model_id: source.source_model_id,
            provider_script: Vec::new(),
            provider_script_error: Some("provider_script conversion skipped by request".into()),
        }),
    }
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
    body.spec
        .validate_unique_fixture_ids()
        .map_err(ApiError::BadRequest)?;
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
    body.spec
        .validate_unique_fixture_ids()
        .map_err(ApiError::BadRequest)?;
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

/// `POST /v1/eval/datasets/:id/fixtures` — atomically append a single
/// fixture to the dataset. Race-safe alternative to PUT for the "iterate
/// fixture by fixture" workflow: PUT requires re-sending the whole list
/// and silently drops appends made by concurrent admins between GET and
/// PUT.
///
/// Rejects with 409 when `expected_revision` doesn't match (operator
/// should re-GET, decide whether to retry). Rejects with 409 when the
/// fixture id already exists in the dataset (use PUT to mutate).
#[tracing::instrument(skip_all, fields(id = %id, fixture_id = %body.fixture.id))]
pub async fn append_fixture(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<AppendFixtureRequest>,
) -> Result<Response, ApiError> {
    crate::config_routes::ensure_admin_auth(&state, &headers)?;
    let store = config_store_or_unavailable(&state)?;
    let existing_value = store
        .get(DATASETS_NAMESPACE, &id)
        .await
        .map_err(map_storage_error)?
        .ok_or_else(|| ApiError::NotFound(format!("dataset not found: {id}")))?;
    let existing_revision = extract_meta_revision(&existing_value).unwrap_or(0);
    if existing_revision != body.expected_revision {
        return Err(ApiError::Conflict(format!(
            "revision conflict: expected {}, actual {existing_revision}",
            body.expected_revision
        )));
    }
    let mut record: ConfigRecord<DatasetSpec> = validate_config_record(existing_value)
        .map_err(|err| ApiError::Internal(err.to_string()))?;
    if record.spec.fixtures.iter().any(|f| f.id == body.fixture.id) {
        return Err(ApiError::Conflict(format!(
            "dataset already has fixture {} (use PUT to replace the whole spec)",
            body.fixture.id
        )));
    }
    record.spec.fixtures.push(body.fixture);
    let now = awaken_contract::time::now_ms();
    record.meta.updated_at = now;
    record.meta.revision = record.meta.revision.saturating_add(1);
    let value = record
        .to_value()
        .map_err(|err| ApiError::Internal(err.to_string()))?;
    store
        .put_if_revision(DATASETS_NAMESPACE, &id, &value, body.expected_revision)
        .await
        .map_err(map_storage_error)?;
    Ok((StatusCode::CREATED, Json(record)).into_response())
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
    require_non_empty_expected(&body.expect)?;

    let events = trace_store
        .read(&body.from_run_id)
        .map_err(map_trace_store_error)?;
    let payload = curate_trace_payload(&events, body.provider_script_mode)
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
        .or(payload.user_input.clone())
        .ok_or_else(|| {
            ApiError::BadRequest(
                "user_input is required: trace did not capture request_messages — \
                 enable ContentCapture::Enabled on the run, or supply user_input in the body"
                    .into(),
            )
        })?;
    let source_run_id = body.from_run_id.clone();
    let fixture = Fixture {
        id: fixture_id.clone(),
        description: Some(
            body.description
                .unwrap_or_else(|| format!("Curated from trace {source_run_id}")),
        ),
        user_input,
        provider_script: payload.provider_script,
        provider_script_error: payload.provider_script_error,
        source_run_id: Some(source_run_id.clone()),
        source_model_id: payload.source_model_id,
        allow_unused_provider_script: body.allow_unused_provider_script,
        mock_response: MockResponse::default(),
        expect: body.expect,
        continued_turns: vec![],
    };
    record.spec.fixtures.push(fixture);

    let now = awaken_contract::time::now_ms();
    record.meta.updated_at = now;
    record.meta.revision = record.meta.revision.saturating_add(1);
    let value = record
        .to_value()
        .map_err(|err| ApiError::Internal(err.to_string()))?;
    // Mark before the CAS write: if the dataset write succeeds, the
    // source trace is already pinned against retention. A concurrent
    // CAS failure may leave a harmless over-retention sentinel, but the
    // opposite ordering can persist a fixture whose source trace is
    // immediately pruneable if mark_referenced fails.
    mark_dataset_trace_reference(trace_store.as_ref(), &source_run_id)?;
    store
        .put_if_revision(DATASETS_NAMESPACE, &id, &value, existing_revision)
        .await
        .map_err(map_storage_error)?;

    Ok((StatusCode::CREATED, Json(record)).into_response())
}

// ── Bulk import from prod traces ─────────────────────────────────────────

// `default_import_traces_max` lives on `ServerConfig::eval_limits`
// — see the read inside `import_traces` below.

/// `POST /v1/eval/datasets/:id/import-traces` — sample prod traces and
/// promote them to fixtures in one shot.
///
/// Closes the loop between production observability and the regression
/// dataset: an operator filters traces by agent / time, the server runs
/// [`trace_to_provider_script`] on each, and appends the resulting
/// fixtures under CAS. Traces whose `run_id` matches an existing fixture
/// id are skipped (no clobber).
#[tracing::instrument(skip_all, fields(id = %id, agent_id = ?body.agent_id))]
pub async fn import_traces(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<ImportTracesRequest>,
) -> Result<Response, ApiError> {
    crate::config_routes::ensure_admin_auth(&state, &headers)?;
    let store = config_store_or_unavailable(&state)?;
    let trace_store = state
        .trace_store()
        .ok_or_else(|| ApiError::ServiceUnavailable("trace store not configured".into()))?;
    require_non_empty_expected(&body.expect)?;

    // Load + CAS-check the dataset.
    let existing_value = store
        .get(DATASETS_NAMESPACE, &id)
        .await
        .map_err(map_storage_error)?
        .ok_or_else(|| ApiError::NotFound(format!("dataset not found: {id}")))?;
    let existing_revision = extract_meta_revision(&existing_value).unwrap_or(0);
    if existing_revision != body.expected_revision {
        return Err(ApiError::Conflict(format!(
            "revision conflict: expected {}, actual {}",
            body.expected_revision, existing_revision,
        )));
    }
    let mut record: ConfigRecord<DatasetSpec> = validate_config_record(existing_value)
        .map_err(|err| ApiError::Internal(err.to_string()))?;

    // Build the trace filter. since_secs → SystemTime for the
    // observability layer's existing TraceFilter.
    let since = body
        .since_secs
        .map(|s| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s));
    let max_count = body
        .max_count
        .unwrap_or(state.config.eval_limits.default_import_traces_max);
    let filter = awaken_ext_observability::trace_store::TraceFilter {
        agent_id: body.agent_id.clone(),
        since,
        limit: Some(max_count),
        ..Default::default()
    };
    let summaries = trace_store.list(&filter).map_err(map_trace_store_error)?;

    // Mutable seen-set updated AFTER each successful push so duplicate
    // run_ids inside `summaries` itself (e.g. from a pagination retry
    // on a flaky trace backend) cannot produce duplicate fixtures
    // within a single import call. Static existing_ids snapshotted from
    // the dataset state catches duplicates against prior writes.
    let mut seen_ids: std::collections::HashSet<String> =
        record.spec.fixtures.iter().map(|f| f.id.clone()).collect();
    let mut imported_run_ids: Vec<String> = Vec::new();
    let mut imported = 0usize;
    let mut skipped = 0usize;
    for summary in summaries {
        if seen_ids.contains(&summary.run_id) {
            skipped += 1;
            continue;
        }
        let events = trace_store
            .read(&summary.run_id)
            .map_err(map_trace_store_error)?;
        let payload = match curate_trace_payload(&events, body.provider_script_mode) {
            Ok(c) => c,
            Err(err) if body.skip_uncuratable => {
                tracing::warn!(run_id = %summary.run_id, %err, "skipping uncuratable trace");
                skipped += 1;
                continue;
            }
            Err(err) => {
                return Err(ApiError::BadRequest(format!(
                    "curating trace {}: {err}",
                    summary.run_id
                )));
            }
        };
        let user_input = match payload.user_input.clone() {
            Some(u) => u,
            None if body.skip_uncuratable => {
                skipped += 1;
                continue;
            }
            None => {
                return Err(ApiError::BadRequest(format!(
                    "trace {} did not capture request_messages — \
                     enable ContentCapture::Enabled or set skip_uncuratable=true",
                    summary.run_id
                )));
            }
        };
        record.spec.fixtures.push(Fixture {
            id: summary.run_id.clone(),
            description: Some(format!("Imported from trace {}", summary.run_id)),
            user_input,
            provider_script: payload.provider_script,
            provider_script_error: payload.provider_script_error,
            source_run_id: Some(summary.run_id.clone()),
            source_model_id: payload.source_model_id,
            allow_unused_provider_script: false,
            mock_response: MockResponse::default(),
            expect: body.expect.clone(),
            continued_turns: vec![],
        });
        seen_ids.insert(summary.run_id.clone());
        imported_run_ids.push(summary.run_id);
        imported += 1;
    }
    // Final belt-and-braces invariant check: the loop guards above
    // should make this impossible, but if a future refactor regresses
    // the seen-set logic, fail loud here rather than persist a corrupt
    // dataset that breaks the diff layer downstream.
    record
        .spec
        .validate_unique_fixture_ids()
        .map_err(ApiError::Internal)?;

    if imported == 0 {
        return Ok(Json(ImportTracesResponse {
            imported_count: 0,
            skipped_count: skipped,
            dataset_revision: existing_revision,
        })
        .into_response());
    }

    record.meta.updated_at = awaken_contract::time::now_ms();
    record.meta.revision = record.meta.revision.saturating_add(1);
    let value = record
        .to_value()
        .map_err(|err| ApiError::Internal(err.to_string()))?;
    for run_id in &imported_run_ids {
        mark_dataset_trace_reference(trace_store.as_ref(), run_id)?;
    }
    store
        .put_if_revision(DATASETS_NAMESPACE, &id, &value, existing_revision)
        .await
        .map_err(map_storage_error)?;

    Ok(Json(ImportTracesResponse {
        imported_count: imported,
        skipped_count: skipped,
        dataset_revision: record.meta.revision,
    })
    .into_response())
}

// ── Dialogue importer (POST /v1/eval/datasets/:id/import-dialogue) ──────

/// `POST /v1/eval/datasets/:id/import-dialogue` — assemble one multi-turn
/// dialogue fixture from N successive trace runs (same conversation
/// thread). Each run must be curatable (have `request_messages`
/// captured on its first inference) — partial traces 400 out.
#[tracing::instrument(skip_all, fields(id = %id, run_count = body.run_ids.len()))]
pub async fn import_dialogue(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<ImportDialogueRequest>,
) -> Result<Response, ApiError> {
    crate::config_routes::ensure_admin_auth(&state, &headers)?;
    if body.run_ids.is_empty() {
        return Err(ApiError::BadRequest("run_ids must be non-empty".into()));
    }
    require_non_empty_expected(&body.expect)?;
    let store = config_store_or_unavailable(&state)?;
    let trace_store = state
        .trace_store()
        .ok_or_else(|| ApiError::ServiceUnavailable("trace store not configured".into()))?;

    let existing_value = store
        .get(DATASETS_NAMESPACE, &id)
        .await
        .map_err(map_storage_error)?
        .ok_or_else(|| ApiError::NotFound(format!("dataset not found: {id}")))?;
    let existing_revision = extract_meta_revision(&existing_value).unwrap_or(0);
    if existing_revision != body.expected_revision {
        return Err(ApiError::Conflict(format!(
            "revision conflict: expected {}, actual {}",
            body.expected_revision, existing_revision,
        )));
    }
    let mut record: ConfigRecord<DatasetSpec> = validate_config_record(existing_value)
        .map_err(|err| ApiError::Internal(err.to_string()))?;

    let fixture_id = body
        .fixture_id
        .clone()
        .unwrap_or_else(|| body.run_ids[0].clone());
    if record.spec.fixtures.iter().any(|f| f.id == fixture_id) {
        return Err(ApiError::Conflict(format!(
            "dataset already has fixture {fixture_id}"
        )));
    }

    // Curate each run in order. First → turn 0; rest → continued_turns.
    let mut turn_inputs: Vec<(String, Vec<ProviderScriptEvent>, Option<String>)> =
        Vec::with_capacity(body.run_ids.len());
    let mut source_model_id: Option<String> = None;
    let mut conversation_thread_id: Option<String> = None;
    for run_id in &body.run_ids {
        let events = trace_store.read(run_id).map_err(map_trace_store_error)?;
        // Stitching assumes a single conversation thread per dialogue:
        // pin thread_id from the first run, refuse to stitch any later
        // run whose first span carries a different one. Otherwise the
        // "dialogue" would actually be unrelated traces concatenated.
        let run_thread_id = events
            .iter()
            .find_map(|e| match e {
                awaken_ext_observability::MetricsEvent::Inference(s) => {
                    Some(s.context.thread_id.clone())
                }
                _ => None,
            })
            .ok_or_else(|| {
                ApiError::BadRequest(format!(
                    "trace {run_id} has no inference span; cannot determine conversation thread"
                ))
            })?;
        match &conversation_thread_id {
            None => conversation_thread_id = Some(run_thread_id),
            Some(first) if first != &run_thread_id => {
                return Err(ApiError::BadRequest(format!(
                    "run {run_id} thread_id={run_thread_id} differs from \
                     dialogue thread_id={first}; runs must come from one conversation"
                )));
            }
            _ => {}
        }
        let payload = curate_trace_payload(&events, body.provider_script_mode)
            .map_err(|err| ApiError::BadRequest(format!("curating trace {run_id}: {err}")))?;
        let user_input = payload.user_input.clone().ok_or_else(|| {
            ApiError::BadRequest(format!(
                "trace {run_id} did not capture request_messages — \
                 enable ContentCapture::Enabled on the originating run"
            ))
        })?;
        // Pin source model from the first run; refuse to stitch later
        // runs whose model differs — the agent isn't usually swapped
        // mid-conversation and the resulting fixture's pin would be
        // misleading.
        match (&source_model_id, &payload.source_model_id) {
            (None, m) => source_model_id = m.clone(),
            (Some(first), Some(later)) if first != later => {
                return Err(ApiError::BadRequest(format!(
                    "run {run_id} source_model_id={later} differs from dialogue \
                     source_model_id={first}; model changed mid-conversation"
                )));
            }
            _ => {}
        }
        turn_inputs.push((
            user_input,
            payload.provider_script,
            payload.provider_script_error,
        ));
    }

    let mut iter = turn_inputs.into_iter();
    let (first_input, first_script, first_script_error) =
        iter.next().expect("at least one run_id (validated above)");
    let continued_turns: Vec<DialogueTurn> = iter
        .map(
            |(user_input, provider_script, provider_script_error)| DialogueTurn {
                user_input,
                provider_script,
                provider_script_error,
            },
        )
        .collect();

    let fixture = Fixture {
        id: fixture_id.clone(),
        description: Some(
            body.description
                .clone()
                .unwrap_or_else(|| format!("Stitched dialogue from {} runs", body.run_ids.len())),
        ),
        user_input: first_input,
        provider_script: first_script,
        provider_script_error: first_script_error,
        source_run_id: Some(body.run_ids[0].clone()),
        source_model_id,
        allow_unused_provider_script: false,
        mock_response: MockResponse::default(),
        expect: body.expect,
        continued_turns,
    };
    record.spec.fixtures.push(fixture);

    record.meta.updated_at = awaken_contract::time::now_ms();
    record.meta.revision = record.meta.revision.saturating_add(1);
    let value = record
        .to_value()
        .map_err(|err| ApiError::Internal(err.to_string()))?;
    for run_id in &body.run_ids {
        mark_dataset_trace_reference(trace_store.as_ref(), run_id)?;
    }
    store
        .put_if_revision(DATASETS_NAMESPACE, &id, &value, existing_revision)
        .await
        .map_err(map_storage_error)?;

    Ok(Json(ImportDialogueResponse {
        fixture_id,
        dataset_revision: record.meta.revision,
    })
    .into_response())
}
