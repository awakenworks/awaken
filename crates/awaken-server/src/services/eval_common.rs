//! Helpers shared between `dataset_service` and `eval_run_service`.
//!
//! Both wrap the same `ConfigStore` + `TraceStore` plumbing and need
//! identical error mappings. Centralising the mappers prevents one of
//! them silently drifting (e.g. dataset returning a 409 for a revision
//! conflict while eval-run returning a 500 for the same condition).

use std::sync::Arc;

use awaken_ext_observability::trace_store::TraceStoreError;
use awaken_server_contract::contract::config_store::ConfigStore;
use awaken_server_contract::contract::executor::LlmExecutor;
use awaken_server_contract::contract::storage::StorageError;
use awaken_server_contract::registry_spec::{AgentSpec, ModelSpec, ProviderSpec};

use crate::app::EvalRoutesState;
use crate::error::ApiError;
use crate::services::config_runtime::build_genai_provider_executor_with_broker;

/// Fetch the config store required by `EvalRoutesState`. Route composition only
/// mounts eval handlers once this module state exists, so handlers do not carry
/// a second "config store not configured" branch.
pub(crate) fn eval_config_store(state: &EvalRoutesState) -> Arc<dyn ConfigStore> {
    state.config.config_store.clone()
}

/// Resolve a `model_id` against the registry to a live executor + its
/// upstream model name. Used by online eval and the matrix path of the
/// dataset run endpoint when fixtures execute against real providers.
///
/// Composition:
///   1. Read `models/{model_id}` → `ModelSpec`
///   2. Read `providers/{provider_id}` → `ProviderSpec`
///   3. `build_genai_provider_executor_with_broker(spec, broker)`
///
/// `NotFound` on either lookup becomes `404` with a message identifying
/// which side missed.
pub(crate) async fn resolve_live_executor(
    state: &EvalRoutesState,
    model_id: &str,
) -> Result<ResolvedLiveExecutor, ApiError> {
    let store = eval_config_store(state);

    let model_value = store
        .get("models", model_id)
        .await
        .map_err(map_storage_error)?
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "model not found: models/{model_id} (register via /v1/config/models)"
            ))
        })?;
    // ConfigStore may store either a bare-spec or the ConfigRecord
    // envelope; awaken_server_contract::config_record::ConfigRecord::from_value
    // handles both shapes transparently.
    let model_record =
        awaken_server_contract::config_record::ConfigRecord::<ModelSpec>::from_value(model_value)
            .map_err(|err| ApiError::Internal(format!("decoding model spec: {err}")))?;
    let spec = model_record.spec;

    let provider_value = store
        .get("providers", &spec.provider_id)
        .await
        .map_err(map_storage_error)?
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "provider not found: providers/{} (referenced by model {model_id})",
                spec.provider_id
            ))
        })?;
    let provider_record =
        awaken_server_contract::config_record::ConfigRecord::<ProviderSpec>::from_value(
            provider_value,
        )
        .map_err(|err| ApiError::Internal(format!("decoding provider: {err}")))?;
    let provider = provider_record.spec;

    let executor =
        build_genai_provider_executor_with_broker(&provider, state.run.credential_broker.clone())
            .map_err(|err| ApiError::Internal(format!("building provider executor: {err}")))?;
    Ok(ResolvedLiveExecutor {
        upstream_model: spec.upstream_model.clone(),
        spec,
        executor,
    })
}

/// Result of [`resolve_live_executor`]. Carries the executor *plus* the
/// resolved [`ModelSpec`] so callers can read pricing (and other future
/// model metadata) without a second registry lookup.
pub(crate) struct ResolvedLiveExecutor {
    pub executor: Arc<dyn LlmExecutor>,
    pub upstream_model: String,
    pub spec: ModelSpec,
}

/// Resolve `agent_id` against the registry to its persisted
/// [`AgentSpec`]. Used by the eval services so a `POST /v1/eval/runs`
/// (or `/v1/eval/online`) with `agent_id` set replays the agent's real
/// `system_prompt` / `allowed_tools` / sampling params — not the
/// synthetic stub the replayer falls back to without a base.
///
/// `NotFound` becomes a 404 identifying the missing record so the
/// caller can correct the id rather than seeing an opaque 500.
pub(crate) async fn resolve_agent_spec(
    state: &EvalRoutesState,
    agent_id: &str,
) -> Result<AgentSpec, ApiError> {
    let store = eval_config_store(state);
    let raw = store
        .get("agents", agent_id)
        .await
        .map_err(map_storage_error)?
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "agent not found: agents/{agent_id} (register via /v1/config/agents)"
            ))
        })?;
    let record = awaken_server_contract::config_record::ConfigRecord::<AgentSpec>::from_value(raw)
        .map_err(|err| ApiError::Internal(format!("decoding agent: {err}")))?;
    Ok(record.spec)
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
