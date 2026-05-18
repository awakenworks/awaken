//! Helpers shared between `dataset_service` and `eval_run_service`.
//!
//! Both wrap the same `ConfigStore` + `TraceStore` plumbing and need
//! identical error mappings. Centralising the mappers prevents one of
//! them silently drifting (e.g. dataset returning a 409 for a revision
//! conflict while eval-run returning a 500 for the same condition).

use std::sync::Arc;

use awaken_contract::contract::config_store::ConfigStore;
use awaken_contract::contract::executor::LlmExecutor;
use awaken_contract::contract::storage::StorageError;
use awaken_contract::registry_spec::{ModelBindingSpec, ProviderSpec};
use awaken_ext_observability::trace_store::TraceStoreError;

use crate::app::AppState;
use crate::error::ApiError;
use crate::services::config_runtime::build_genai_provider_executor_with_broker;

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

/// Resolve a `model_id` against the registry to a live executor + its
/// upstream model name. Used by online eval and the matrix path of the
/// dataset run endpoint when fixtures execute against real providers.
///
/// Composition:
///   1. Read `model_bindings/{model_id}` → `ModelBindingSpec`
///   2. Read `providers/{provider_id}` → `ProviderSpec`
///   3. `build_genai_provider_executor_with_broker(spec, broker)`
///
/// `NotFound` on either lookup becomes `404` with a message identifying
/// which side missed.
pub(crate) async fn resolve_live_executor(
    state: &AppState,
    model_id: &str,
) -> Result<ResolvedLiveExecutor, ApiError> {
    let store = config_store_or_unavailable(state)?;

    let binding_value = store
        .get("models", model_id)
        .await
        .map_err(map_storage_error)?
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "model binding not found: models/{model_id} (register via /v1/config/models)"
            ))
        })?;
    // ConfigStore may store either a bare-spec or the ConfigRecord
    // envelope; awaken_contract::config_record::ConfigRecord::from_value
    // handles both shapes transparently.
    let binding_record =
        awaken_contract::config_record::ConfigRecord::<ModelBindingSpec>::from_value(binding_value)
            .map_err(|err| ApiError::Internal(format!("decoding model binding: {err}")))?;
    let binding = binding_record.spec;

    let provider_value = store
        .get("providers", &binding.provider_id)
        .await
        .map_err(map_storage_error)?
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "provider not found: providers/{} (referenced by model {model_id})",
                binding.provider_id
            ))
        })?;
    let provider_record =
        awaken_contract::config_record::ConfigRecord::<ProviderSpec>::from_value(provider_value)
            .map_err(|err| ApiError::Internal(format!("decoding provider: {err}")))?;
    let provider = provider_record.spec;

    let executor = build_genai_provider_executor_with_broker(&provider, state.credential_broker())
        .map_err(|err| ApiError::Internal(format!("building provider executor: {err}")))?;
    Ok(ResolvedLiveExecutor {
        upstream_model: binding.upstream_model.clone(),
        binding,
        executor,
    })
}

/// Result of [`resolve_live_executor`]. Carries the executor *plus* the
/// resolved [`ModelBindingSpec`] so callers can read pricing (and other
/// future binding metadata) without a second registry lookup.
pub(crate) struct ResolvedLiveExecutor {
    pub executor: Arc<dyn LlmExecutor>,
    pub upstream_model: String,
    pub binding: ModelBindingSpec,
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
