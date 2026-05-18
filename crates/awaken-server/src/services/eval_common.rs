//! Helpers shared between `dataset_service` and `eval_run_service`.
//!
//! Both wrap the same `ConfigStore` + `TraceStore` plumbing and need
//! identical error mappings. Centralising the mappers prevents one of
//! them silently drifting (e.g. dataset returning a 409 for a revision
//! conflict while eval-run returning a 500 for the same condition).

use std::sync::Arc;

use awaken_contract::contract::config_store::ConfigStore;
use awaken_contract::contract::storage::StorageError;
use awaken_ext_observability::trace_store::TraceStoreError;

use crate::app::AppState;
use crate::error::ApiError;

/// Fetch the attached `ConfigStore` or surface a 503 — both eval
/// services depend on it being wired and otherwise return identical
/// "config store not configured" messages.
pub(crate) fn config_store_or_unavailable(
    state: &AppState,
) -> Result<Arc<dyn ConfigStore>, ApiError> {
    state
        .config_store
        .clone()
        .ok_or_else(|| ApiError::ServiceUnavailable("config store not configured".into()))
}

/// Translate `ConfigStore` errors into HTTP-shaped `ApiError`s.
///
/// `VersionConflict` becomes a 409 with explicit `expected`/`actual` so
/// the client can re-fetch and retry; `NotFound` and `AlreadyExists`
/// retain their natural shape. Other variants are server-side bugs and
/// fall through to 500.
pub(crate) fn map_storage_error(err: StorageError) -> ApiError {
    match err {
        StorageError::NotFound(msg) => ApiError::NotFound(msg),
        StorageError::AlreadyExists(msg) => ApiError::Conflict(msg),
        StorageError::VersionConflict { expected, actual } => ApiError::Conflict(format!(
            "revision conflict: expected {expected}, actual {actual}"
        )),
        StorageError::Validation(msg) => ApiError::BadRequest(msg),
        err => ApiError::Internal(err.to_string()),
    }
}

/// Translate `TraceStore` errors into HTTP-shaped `ApiError`s. Same
/// shape as `/v1/traces` route mappings so the curate endpoint behaves
/// identically when callers reference a missing or malformed run id.
pub(crate) fn map_trace_store_error(err: TraceStoreError) -> ApiError {
    match err {
        TraceStoreError::NotFound { run_id } => {
            ApiError::NotFound(format!("trace not found: {run_id}"))
        }
        TraceStoreError::InvalidRunId(id) => ApiError::BadRequest(format!("invalid run id: {id}")),
        err => ApiError::Internal(err.to_string()),
    }
}
