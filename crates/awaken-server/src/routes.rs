//! Axum router setup — unified route registration.
use crate::eval_router::eval_routes;
use crate::services::trace_service::{get_trace, list_traces, pin_trace};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{ChildThreadDeleteStrategy, StorageError};
use awaken_ext_observability::runtime_stats::parse_window_str;

use crate::app::AppState;
use crate::config_routes::config_routes;
use crate::http_run::wire_sse_relay;
use crate::http_sse::{sse_body_stream, sse_response};
use crate::mailbox::{ACTIVE_RUN_CONFLICT_MESSAGE, MailboxDispatchStatus, MailboxError};
use crate::protocols::a2a::a2a_routes;
use crate::protocols::ag_ui::http::ag_ui_routes;
use crate::protocols::ai_sdk_v6::http::ai_sdk_routes;
use crate::protocols::mcp::http::mcp_routes;
use crate::query::{self, MessageQueryParams, ThreadQueryParams};
use crate::services::run_control_service::{
    InputMode, InterruptMode, RunControlError, RunControlService,
};
use awaken_runtime::RunRequest;

pub use crate::error::ApiError;

pub(crate) fn map_mailbox_error(error: MailboxError) -> ApiError {
    match error {
        MailboxError::Validation(msg) if msg == ACTIVE_RUN_CONFLICT_MESSAGE => {
            ApiError::Conflict(msg)
        }
        MailboxError::Validation(msg) => ApiError::BadRequest(msg),
        MailboxError::Store(StorageError::Validation(msg)) => ApiError::BadRequest(msg),
        MailboxError::Store(
            err @ StorageError::AlreadyExists(_) | err @ StorageError::VersionConflict { .. },
        ) => ApiError::Conflict(err.to_string()),
        MailboxError::Store(err) => ApiError::Internal(err.to_string()),
        MailboxError::Internal(msg) => ApiError::Internal(msg),
    }
}

fn map_thread_storage_error(thread_id: Option<&str>, error: StorageError) -> ApiError {
    match error {
        StorageError::Validation(message) => ApiError::BadRequest(message),
        err @ StorageError::AlreadyExists(_) | err @ StorageError::VersionConflict { .. } => {
            ApiError::Conflict(err.to_string())
        }
        StorageError::NotFound(id) if thread_id == Some(id.as_str()) => {
            ApiError::ThreadNotFound(id)
        }
        StorageError::NotFound(id) => ApiError::NotFound(format!("not found: {id}")),
        err => ApiError::Internal(err.to_string()),
    }
}

fn map_run_control_error(error: RunControlError) -> ApiError {
    match error {
        RunControlError::ThreadNotFound(id) => ApiError::ThreadNotFound(id),
        RunControlError::RunNotFound(id) => ApiError::RunNotFound(id),
        RunControlError::DecisionTargetNotFound(id) => ApiError::RunNotFound(id),
        RunControlError::Store(error) => ApiError::Internal(error.to_string()),
        RunControlError::Mailbox(error) => map_mailbox_error(error),
    }
}

/// Build the complete router for the given state.
pub fn build_router(state: &AppState) -> Router<AppState> {
    crate::metrics::install_recorder();

    let admin_config = state.admin_api_config();

    let mut router = Router::new()
        .merge(health_routes())
        .merge(thread_routes())
        .merge(run_routes())
        .merge(ai_sdk_routes())
        .merge(ag_ui_routes())
        .merge(a2a_routes())
        .merge(mcp_routes());

    if admin_config.expose_config_routes {
        router = router.merge(admin_routes());
        router = router.merge(eval_routes());
    }

    if admin_config.expose_trace_routes {
        router = router.merge(trace_routes());
    }

    router
        .route("/metrics", get(crate::metrics::metrics_handler))
        .layer(middleware::from_fn(crate::metrics::http_metrics_middleware))
}

fn trace_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/traces", get(list_traces))
        .route("/v1/traces/:run_id", get(get_trace))
        .route("/v1/traces/:run_id/pin", post(pin_trace))
}

fn health_routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health_ready))
        .route("/health/live", get(health_live))
}

fn thread_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/threads", get(list_threads).post(create_thread))
        .route("/v1/threads/summaries", get(list_thread_summaries))
        .route(
            "/v1/threads/:id",
            get(get_thread).delete(delete_thread).patch(patch_thread),
        )
        .route("/v1/threads/:id/cancel", post(cancel_thread))
        .route("/v1/threads/:id/decision", post(submit_thread_decision))
        .route("/v1/threads/:id/interrupt", post(interrupt_thread))
        .route("/v1/threads/:id/metadata", patch(patch_thread))
        .route(
            "/v1/threads/:id/messages",
            get(get_thread_messages).post(post_thread_messages),
        )
        .route(
            "/v1/threads/:id/mailbox",
            post(push_mailbox).get(peek_mailbox),
        )
}

fn run_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/runs", get(list_runs).post(start_run))
        .route("/v1/runs/:id", get(get_run))
        .route("/v1/runs/:id/inputs", post(push_run_inputs))
        .route("/v1/runs/:id/cancel", post(cancel_run))
        .route("/v1/runs/:id/decision", post(submit_decision))
        .route("/v1/threads/:id/runs", get(list_thread_runs))
        .route("/v1/threads/:id/runs/active", get(active_thread_run))
        .route("/v1/threads/:id/runs/latest", get(latest_thread_run))
}

fn admin_routes() -> Router<AppState> {
    // permission-preview is always registered so the handler returns 503
    // when the `permission` feature is absent (a 404 would be ambiguous
    // with "agent not found"). Matches `runtime-stats` / trace conventions.
    let permission_preview = get(get_agent_permission_preview);
    config_routes()
        .route("/v1/system/info", get(system_info))
        .route("/v1/agents/:id/runtime-stats", get(get_agent_runtime_stats))
        .route("/v1/agents/runtime-stats", get(list_agents_runtime_stats))
        .route("/v1/agents/:id/permission-preview", permission_preview)
}

#[tracing::instrument(skip(state))]
async fn get_agent_permission_preview(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(err) = crate::config_routes::ensure_admin_auth(&state, &headers) {
        return err.into_response();
    }
    #[cfg(feature = "permission")]
    {
        let _ = &id;
        match crate::services::permission_preview::preview_agent_permissions(&state, &id).await {
            Ok(preview) => Json(preview).into_response(),
            Err(err) => map_permission_preview_error(err).into_response(),
        }
    }
    #[cfg(not(feature = "permission"))]
    {
        let _ = (state, id);
        ApiError::ServiceUnavailable(
            "permission feature not compiled into this server build".to_string(),
        )
        .into_response()
    }
}

#[cfg(feature = "permission")]
fn map_permission_preview_error(
    err: crate::services::permission_preview::PermissionPreviewError,
) -> ApiError {
    use crate::services::permission_preview::PermissionPreviewError as PE;
    match err {
        PE::AgentNotFound(id) => ApiError::NotFound(format!("agent not found: {id}")),
        PE::InvalidSpec(msg) | PE::InvalidPermissionConfig { reason: msg, .. } => {
            ApiError::BadRequest(msg)
        }
        PE::RegistryUnavailable => ApiError::Internal("runtime registry unavailable".into()),
        PE::Config(err) => ApiError::Internal(err.to_string()),
    }
}

// ── Agent runtime-stats endpoints ───────────────────────────────────

/// Query params for `GET /v1/agents/:id/runtime-stats`.
#[derive(Debug, Deserialize, Default)]
struct RuntimeStatsQuery {
    /// Optional time window, e.g. `1h`, `24h`, `7d`, `3600s`, `90`.
    window: Option<String>,
}

/// `GET /v1/agents/:id/runtime-stats` — return the agent's rolling-window
/// snapshot, or 404 when the agent has not been seen by the registry, or
/// 503 when the registry is not configured on this server.
///
/// Accepts an optional `?window=` query parameter (e.g. `1h`, `24h`, `7d`)
/// to restrict the snapshot to a shorter sub-window of the registry's full
/// history.  An invalid format returns 400.
#[tracing::instrument(skip(state))]
async fn get_agent_runtime_stats(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<RuntimeStatsQuery>,
) -> Response {
    if let Err(err) = crate::config_routes::ensure_admin_auth(&state, &headers) {
        return err.into_response();
    }
    let Some(registry) = state.runtime_stats() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "runtime_stats registry not configured" })),
        )
            .into_response();
    };

    let window = match params.window.as_deref() {
        None => None,
        Some(s) => match parse_window_str(s) {
            Ok(d) => Some(d),
            Err(msg) => {
                return (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response();
            }
        },
    };

    match registry.snapshot_for_window(&id, window) {
        Some(snapshot) => Json(snapshot).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("agent not found in runtime stats: {id}") })),
        )
            .into_response(),
    }
}

/// `GET /v1/agents/runtime-stats` — return one snapshot per known agent,
/// sorted by `agent_id`. Returns `{"agents":[...]}` (or 503 when the
/// registry is missing).
#[tracing::instrument(skip(state))]
async fn list_agents_runtime_stats(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(err) = crate::config_routes::ensure_admin_auth(&state, &headers) {
        return err.into_response();
    }
    let Some(registry) = state.runtime_stats() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "runtime_stats registry not configured" })),
        )
            .into_response();
    };
    let snapshots: Vec<_> = registry
        .known_agents()
        .into_iter()
        .filter_map(|id| registry.snapshot_for(&id))
        .collect();
    Json(json!({ "agents": snapshots })).into_response()
}

// ── Health ──

/// Liveness probe — always returns 200.  Use for k8s `livenessProbe`.
#[tracing::instrument]
async fn health_live() -> impl IntoResponse {
    StatusCode::OK
}

/// `GET /v1/system/info` — flat snapshot of server identity for the admin
/// console System card. Returns the crate version, seconds since process
/// start, and which optional subsystems are wired in.
///
/// Concrete store backend names are intentionally NOT exposed: the
/// `&dyn Trait` lookup only yields the trait name, not the impl, so any
/// string would mislead. Embedders that need to expose the concrete backend
/// can decorate `AppState` with their own field and a separate route.
#[tracing::instrument(skip(state))]
async fn system_info(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(err) = crate::config_routes::ensure_admin_auth(&state, &headers) {
        return err.into_response();
    }
    let uptime_secs = state.started_at().elapsed().as_secs();
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": uptime_secs,
        "config_store_enabled": state.config_store.is_some(),
        "audit_log_enabled": state.audit_log().is_some(),
        "runtime_stats_enabled": state.runtime_stats().is_some(),
    }))
    .into_response()
}

/// Readiness probe — checks that critical dependencies are reachable.
/// Returns 200 with `"status":"healthy"` when everything is fine, or 503
/// with `"status":"unhealthy"` when a component check fails.
///
/// Individual component checks are capped at 5 seconds to avoid blocking
/// the probe.
#[tracing::instrument(skip(st))]
async fn health_ready(State(st): State<AppState>) -> impl IntoResponse {
    const CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    // -- Store check: attempt a lightweight list operation.
    let store_status = match tokio::time::timeout(CHECK_TIMEOUT, st.store.list_threads(0, 1)).await
    {
        Ok(Ok(_)) => "ok",
        Ok(Err(_)) => "error",
        Err(_) => "timeout",
    };

    // -- Runtime check: the runtime is healthy if it exists (it is
    //    always present once AppState is constructed).
    let runtime_status = "ok";

    let all_ok = store_status == "ok" && runtime_status == "ok";
    let overall = if all_ok { "healthy" } else { "unhealthy" };
    let status_code = if all_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (
        status_code,
        Json(json!({
            "status": overall,
            "components": {
                "store": store_status,
                "runtime": runtime_status,
            }
        })),
    )
}

// ── Threads ──

#[derive(Debug, Deserialize)]
struct ListParams {
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default = "query::default_limit")]
    limit: usize,
}

#[tracing::instrument(skip(st))]
async fn list_threads(
    State(st): State<AppState>,
    Query(params): Query<ThreadQueryParams>,
) -> Result<Json<Value>, ApiError> {
    let query = params.storage_query().map_err(ApiError::BadRequest)?;
    let page = st
        .store
        .list_threads_query(&query)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({
        "items": page.items,
        "offset": query.offset,
        "limit": query.limit,
        "total": page.total,
        "has_more": page.has_more,
        "next_cursor": page.next_cursor,
    })))
}

#[tracing::instrument(skip(st))]
async fn list_thread_summaries(
    State(st): State<AppState>,
    Query(params): Query<ThreadQueryParams>,
) -> Result<Json<Value>, ApiError> {
    let query = params.storage_query().map_err(ApiError::BadRequest)?;
    let page = st
        .store
        .list_threads_query(&query)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let mut items = Vec::with_capacity(page.items.len());
    for id in page.items {
        let latest_run = st
            .store
            .latest_run(&id)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        if let Some(thread) = st
            .store
            .load_thread(&id)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?
        {
            items.push(json!({
                "id": thread.id,
                "resource_id": thread.resource_id,
                "parent_thread_id": thread.parent_thread_id,
                "title": thread.metadata.title,
                "updated_at": thread.metadata.updated_at,
                "agent_id": latest_run.map(|run| run.agent_id),
            }));
        }
    }
    Ok(Json(json!({
        "items": items,
        "offset": query.offset,
        "limit": query.limit,
        "total": page.total,
        "has_more": page.has_more,
        "next_cursor": page.next_cursor,
    })))
}

#[derive(Debug, Deserialize)]
struct CreateThreadPayload {
    #[serde(default)]
    title: Option<String>,
    #[serde(default, alias = "resourceId")]
    resource_id: Option<String>,
    #[serde(default, alias = "parentThreadId")]
    parent_thread_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeleteThreadParams {
    #[serde(
        default = "default_child_thread_delete_strategy",
        alias = "childStrategy"
    )]
    child_strategy: ChildThreadDeleteStrategy,
}

fn default_child_thread_delete_strategy() -> ChildThreadDeleteStrategy {
    ChildThreadDeleteStrategy::Detach
}

#[derive(Debug, Clone, Default)]
enum OptionalField<T> {
    #[default]
    Unset,
    Null,
    Value(T),
}

impl<T> OptionalField<T> {
    fn into_optional_update(self) -> Option<Option<T>> {
        match self {
            Self::Unset => None,
            Self::Null => Some(None),
            Self::Value(value) => Some(Some(value)),
        }
    }
}

impl<'de, T> Deserialize<'de> for OptionalField<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Option::<T>::deserialize(deserializer).map(|value| match value {
            Some(value) => Self::Value(value),
            None => Self::Null,
        })
    }
}

#[tracing::instrument(skip(st, payload))]
async fn create_thread(
    State(st): State<AppState>,
    Json(payload): Json<CreateThreadPayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let thread = crate::services::thread_service::create_thread_with_options(
        st.store.as_ref(),
        crate::services::thread_service::CreateThreadOptions {
            title: payload.title,
            resource_id: payload.resource_id,
            parent_thread_id: payload.parent_thread_id,
        },
    )
    .await
    .map_err(|error| map_thread_storage_error(None, error))?;
    let value = serde_json::to_value(&thread).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok((StatusCode::CREATED, Json(value)))
}

#[tracing::instrument(skip(st), fields(thread_id = %id))]
async fn get_thread(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let thread = st
        .store
        .load_thread(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or(ApiError::ThreadNotFound(id))?;
    let value = serde_json::to_value(thread).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(value))
}

#[tracing::instrument(skip(st), fields(thread_id = %id))]
async fn delete_thread(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<DeleteThreadParams>,
) -> Result<StatusCode, ApiError> {
    crate::services::thread_service::delete_thread(
        st.store.as_ref(),
        &id,
        crate::services::thread_service::DeleteThreadOptions {
            child_strategy: params.child_strategy,
        },
    )
    .await
    .map_err(|error| map_thread_storage_error(Some(id.as_str()), error))?;

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct PatchThreadPayload {
    #[serde(default)]
    title: Option<String>,
    #[serde(default, alias = "resourceId")]
    resource_id: OptionalField<String>,
    #[serde(default, alias = "parentThreadId")]
    parent_thread_id: OptionalField<String>,
    #[serde(default)]
    custom: Option<std::collections::HashMap<String, Value>>,
}

#[tracing::instrument(skip(st, payload), fields(thread_id = %id))]
async fn patch_thread(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(payload): Json<PatchThreadPayload>,
) -> Result<Json<Value>, ApiError> {
    let thread = crate::services::thread_service::update_thread(
        st.store.as_ref(),
        &id,
        crate::services::thread_service::UpdateThreadOptions {
            title: payload.title,
            resource_id: payload.resource_id.into_optional_update(),
            parent_thread_id: payload.parent_thread_id.into_optional_update(),
            custom: payload.custom,
        },
    )
    .await
    .map_err(|error| map_thread_storage_error(Some(id.as_str()), error))?;

    let value = serde_json::to_value(thread).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(value))
}

#[tracing::instrument(skip(st), fields(thread_id = %id))]
async fn interrupt_thread(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let interrupted = RunControlService::new(st)
        .interrupt_thread(&id, InterruptMode::Graceful)
        .await
        .map_err(map_run_control_error)?;

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "interrupt_requested",
            "thread_id": id,
            "superseded_dispatches": interrupted.superseded_count,
        })),
    )
        .into_response())
}

#[tracing::instrument(skip(st), fields(thread_id = %id))]
async fn get_thread_messages(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<MessageQueryParams>,
) -> Result<Json<Value>, ApiError> {
    // Verify thread exists
    st.store
        .load_thread(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or(ApiError::ThreadNotFound(id.clone()))?;

    let query = params.storage_query().map_err(ApiError::BadRequest)?;
    let page = st
        .store
        .list_message_records(&id, &query)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let messages: Vec<Message> = page
        .records
        .into_iter()
        .map(|record| record.message)
        .collect();

    let value = serde_json::to_value(&messages).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({
        "messages": value,
        "total": page.total,
        "has_more": page.has_more,
        "next_cursor": page.next_cursor,
    })))
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum PushInputMode {
    #[default]
    Queue,
    #[serde(alias = "steer")]
    LiveThenQueue,
    InterruptThenQueue,
    ResumeOpenRun,
}

impl PushInputMode {
    fn input_mode(self) -> Option<InputMode> {
        match self {
            PushInputMode::Queue => Some(InputMode::Queue),
            PushInputMode::InterruptThenQueue => Some(InputMode::InterruptThenQueue),
            PushInputMode::ResumeOpenRun => Some(InputMode::ResumeOpenRun),
            PushInputMode::LiveThenQueue => None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct PostThreadMessagesPayload {
    #[serde(rename = "agentId", alias = "agent_id", default)]
    agent_id: Option<String>,
    #[serde(default)]
    mode: PushInputMode,
    #[serde(default)]
    messages: Vec<RunMessage>,
}

#[tracing::instrument(skip(st, payload), fields(thread_id = %id))]
async fn post_thread_messages(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(payload): Json<PostThreadMessagesPayload>,
) -> Result<Response, ApiError> {
    // Require existing thread for thread-centric API semantics.
    st.store
        .load_thread(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or(ApiError::ThreadNotFound(id.clone()))?;

    let messages = convert_run_messages(payload.messages);
    if messages.is_empty() {
        return Err(ApiError::BadRequest(
            "at least one message is required".to_string(),
        ));
    }

    let service = RunControlService::new(st);
    let result = match payload.mode.input_mode() {
        Some(mode) => {
            service
                .inject_user_input(&id, payload.agent_id, messages, mode)
                .await
        }
        None => {
            service
                .inject_user_input_live_then_queue(&id, payload.agent_id, messages)
                .await
        }
    }
    .map_err(map_run_control_error)?;

    let body = match result.status {
        MailboxDispatchStatus::Running => json!({
            "status": "running",
            "thread_id": id,
        }),
        MailboxDispatchStatus::Queued => json!({
            "status": "queued",
            "thread_id": id,
        }),
    };

    Ok((StatusCode::ACCEPTED, Json(body)).into_response())
}

// ── Mailbox ──

#[derive(Debug, Deserialize)]
struct MailboxPayload {
    #[serde(default)]
    payload: Value,
}

#[tracing::instrument(skip(st, body), fields(thread_id = %id))]
async fn push_mailbox(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<MailboxPayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    // Convert the opaque payload into a user message for the mailbox.
    let text = body
        .payload
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let messages = if text.is_empty() {
        vec![awaken_contract::contract::message::Message::user(
            body.payload.to_string(),
        )]
    } else {
        vec![awaken_contract::contract::message::Message::user(text)]
    };

    let result = st
        .mailbox
        .submit_background(RunRequest::new(id, messages))
        .await
        .map_err(map_mailbox_error)?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "dispatch_id": result.dispatch_id,
            "run_id": result.run_id,
            "thread_id": result.thread_id,
        })),
    ))
}

#[tracing::instrument(skip(st), fields(thread_id = %id))]
async fn peek_mailbox(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<ListParams>,
) -> Result<Json<Value>, ApiError> {
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.clamp(1, 200);
    let dispatches = st
        .mailbox
        .list_dispatches(&id, None, limit, offset)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let value = serde_json::to_value(&dispatches).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({ "items": value })))
}

// ── Runs ──

#[derive(Debug, Deserialize)]
struct CreateRunPayload {
    #[serde(rename = "agentId", alias = "agent_id")]
    agent_id: String,
    #[serde(rename = "threadId", alias = "thread_id", default)]
    thread_id: Option<String>,
    #[serde(default)]
    messages: Vec<RunMessage>,
}

#[derive(Debug, Deserialize)]
struct RunMessage {
    role: String,
    content: String,
}

fn convert_run_messages(msgs: Vec<RunMessage>) -> Vec<Message> {
    crate::message_convert::convert_role_content_pairs(
        msgs.into_iter().map(|m| (m.role, m.content)),
    )
}

#[tracing::instrument(skip(st, payload))]
async fn start_run(
    State(st): State<AppState>,
    Json(payload): Json<CreateRunPayload>,
) -> Result<Response, ApiError> {
    let agent_id = payload.agent_id.trim().to_string();
    if agent_id.is_empty() {
        return Err(ApiError::BadRequest("agent_id cannot be empty".to_string()));
    }

    let messages = convert_run_messages(payload.messages);
    let (thread_id, messages) = crate::request::prepare_run_inputs(payload.thread_id, messages)?;

    let request = RunRequest::new(thread_id, messages).with_agent_id(agent_id);
    let (_result, event_rx) = st
        .mailbox
        .submit(request)
        .await
        .map_err(map_mailbox_error)?;
    let encoder = awaken_contract::contract::transport::Identity::default();
    let sse_rx = wire_sse_relay(event_rx, encoder, st.config.sse_buffer_size, None);

    Ok(sse_response(sse_body_stream(sse_rx)))
}

#[tracing::instrument(skip(st), fields(run_id = %id))]
async fn get_run(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let record = crate::services::run_service::get_run(st.store.as_ref(), &id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or(ApiError::RunNotFound(id))?;
    let value = serde_json::to_value(record).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(value))
}

#[tracing::instrument(skip(st))]
async fn list_runs(
    State(st): State<AppState>,
    Query(params): Query<ListRunsParams>,
) -> Result<Json<Value>, ApiError> {
    use awaken_contract::contract::lifecycle::RunStatus;
    use awaken_contract::contract::storage::RunQuery;

    let status = params
        .status
        .as_deref()
        .map(|s| match s {
            "created" => Ok(RunStatus::Created),
            "running" => Ok(RunStatus::Running),
            "waiting" => Ok(RunStatus::Waiting),
            "done" => Ok(RunStatus::Done),
            other => Err(ApiError::BadRequest(format!(
                "invalid status filter: {other}"
            ))),
        })
        .transpose()?;

    let query = RunQuery {
        offset: params.offset.unwrap_or(0),
        limit: params.limit.clamp(1, 200),
        thread_id: None,
        status,
    };
    let page = crate::services::run_service::list_runs(st.store.as_ref(), &query)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let value = serde_json::to_value(&page.items).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({
        "items": value,
        "total": page.total,
        "has_more": page.has_more,
    })))
}

#[derive(Debug, Deserialize)]
struct PushRunInputsPayload {
    #[serde(default)]
    mode: PushInputMode,
    #[serde(default)]
    messages: Vec<RunMessage>,
}

#[tracing::instrument(skip(st, payload), fields(run_id = %id))]
async fn push_run_inputs(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(payload): Json<PushRunInputsPayload>,
) -> Result<Response, ApiError> {
    let messages = convert_run_messages(payload.messages);
    if messages.is_empty() {
        return Err(ApiError::BadRequest(
            "at least one message is required".to_string(),
        ));
    }

    let service = RunControlService::new(st);
    let result = match payload.mode.input_mode() {
        Some(mode) => service.inject_run_input(&id, messages, mode).await,
        None => {
            service
                .inject_run_input_live_then_queue(&id, messages)
                .await
        }
    };
    let _ = result.map_err(map_run_control_error)?;

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "inputs_accepted",
            "run_id": id,
        })),
    )
        .into_response())
}

#[tracing::instrument(skip(st), fields(run_id = %id))]
async fn cancel_run(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    RunControlService::new(st)
        .cancel_run(&id)
        .await
        .map_err(map_run_control_error)?;

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "cancel_requested",
            "run_id": id,
        })),
    )
        .into_response())
}

#[tracing::instrument(skip(st), fields(thread_id = %id))]
async fn cancel_thread(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    RunControlService::new(st)
        .cancel_run(&id)
        .await
        .map_err(|error| match error {
            RunControlError::RunNotFound(_) => ApiError::ThreadNotFound(id.clone()),
            other => map_run_control_error(other),
        })?;

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "cancel_requested",
            "thread_id": id,
        })),
    )
        .into_response())
}

#[derive(Debug, Deserialize)]
struct DecisionPayload {
    #[serde(rename = "toolCallId", alias = "tool_call_id")]
    tool_call_id: String,
    action: String,
    #[serde(default)]
    payload: Value,
}

#[tracing::instrument(skip(st, payload), fields(run_id = %id))]
async fn submit_decision(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(payload): Json<DecisionPayload>,
) -> Result<Response, ApiError> {
    use awaken_contract::contract::suspension::{ResumeDecisionAction, ToolCallResume};

    let action = match payload.action.as_str() {
        "resume" => ResumeDecisionAction::Resume,
        "cancel" => ResumeDecisionAction::Cancel,
        other => {
            return Err(ApiError::BadRequest(format!(
                "invalid action: {other}, expected 'resume' or 'cancel'"
            )));
        }
    };

    let resume = ToolCallResume {
        decision_id: uuid::Uuid::now_v7().to_string(),
        action,
        result: payload.payload.clone(),
        reason: None,
        updated_at: crate::time::now_millis(),
    };

    RunControlService::new(st)
        .decide(&id, payload.tool_call_id.clone(), resume)
        .await
        .map_err(map_run_control_error)?;

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "decision_submitted",
            "run_id": id,
            "tool_call_id": payload.tool_call_id,
        })),
    )
        .into_response())
}

#[tracing::instrument(skip(st, payload), fields(thread_id = %id))]
async fn submit_thread_decision(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(payload): Json<DecisionPayload>,
) -> Result<Response, ApiError> {
    use awaken_contract::contract::suspension::{ResumeDecisionAction, ToolCallResume};

    let action = match payload.action.as_str() {
        "resume" => ResumeDecisionAction::Resume,
        "cancel" => ResumeDecisionAction::Cancel,
        other => {
            return Err(ApiError::BadRequest(format!(
                "invalid action: {other}, expected 'resume' or 'cancel'"
            )));
        }
    };

    let resume = ToolCallResume {
        decision_id: uuid::Uuid::now_v7().to_string(),
        action,
        result: payload.payload.clone(),
        reason: None,
        updated_at: crate::time::now_millis(),
    };

    RunControlService::new(st)
        .decide(&id, payload.tool_call_id.clone(), resume)
        .await
        .map_err(|error| match error {
            RunControlError::DecisionTargetNotFound(_) => ApiError::ThreadNotFound(id.clone()),
            other => map_run_control_error(other),
        })?;

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "decision_submitted",
            "thread_id": id,
            "tool_call_id": payload.tool_call_id,
        })),
    )
        .into_response())
}

// ── Thread Runs ──

#[derive(Debug, Deserialize)]
struct ListRunsParams {
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default = "query::default_limit")]
    limit: usize,
    #[serde(default)]
    status: Option<String>,
}

#[tracing::instrument(skip(st), fields(thread_id = %id))]
async fn list_thread_runs(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<ListRunsParams>,
) -> Result<Json<Value>, ApiError> {
    use awaken_contract::contract::lifecycle::RunStatus;
    use awaken_contract::contract::storage::RunQuery;

    let status = params
        .status
        .as_deref()
        .map(|s| match s {
            "created" => Ok(RunStatus::Created),
            "running" => Ok(RunStatus::Running),
            "waiting" => Ok(RunStatus::Waiting),
            "done" => Ok(RunStatus::Done),
            other => Err(ApiError::BadRequest(format!(
                "invalid status filter: {other}"
            ))),
        })
        .transpose()?;

    let query = RunQuery {
        offset: params.offset.unwrap_or(0),
        limit: params.limit.clamp(1, 200),
        thread_id: Some(id),
        status,
    };
    let page = crate::services::run_service::list_runs(st.store.as_ref(), &query)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let value = serde_json::to_value(&page.items).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({
        "items": value,
        "total": page.total,
        "has_more": page.has_more,
    })))
}

#[tracing::instrument(skip(st), fields(thread_id = %id))]
async fn latest_thread_run(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let record = crate::services::run_service::latest_run(st.store.as_ref(), &id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or(ApiError::RunNotFound(format!("no runs for thread {id}")))?;
    let value = serde_json::to_value(record).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(value))
}

#[tracing::instrument(skip(st), fields(thread_id = %id))]
async fn active_thread_run(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let active = RunControlService::new(st)
        .get_active_run(&id)
        .await
        .map_err(map_run_control_error)?;
    Ok(Json(json!({ "active_run": active })))
}

#[cfg(test)]
#[path = "routes_test.rs"]
mod tests;
