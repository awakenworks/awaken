//! Axum router setup — unified route registration.
use crate::services::trace_service::{get_trace, list_traces, pin_trace};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::{RunRoutesState, ServerState, TraceRoutesState};
use crate::http_run::wire_sse_relay;
use crate::http_sse::{sse_body_stream, sse_response};
use crate::mailbox::{ACTIVE_RUN_CONFLICT_MESSAGE, MailboxDispatchStatus, MailboxError};
use crate::query::{self, MessageQueryParams, ThreadQueryParams};
use crate::services::run_control_service::{
    InputMode, InterruptMode, RunControlError, RunControlService,
};
use awaken_contract::contract::message::Message;
use awaken_contract::contract::storage::{ChildThreadDeleteStrategy, StorageError};
use awaken_runtime::RunActivation;

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

use crate::route_modules::{AdminRunModule, RouteModule, SystemRoutes};

/// Build the complete router for the given state.
pub fn build_router(state: &ServerState) -> Router {
    crate::metrics::install_recorder();
    let admin_config = state.admin_api_config();

    let mut router = Router::new();
    router = state.run_routes_state().mount(router);
    router = state.protocol_routes_state().mount(router);
    router = SystemRoutes(state.system_routes_state()).mount(router);
    router = state.event_module().mount(router);
    if admin_config.expose_config_routes {
        router = AdminRunModule(state.admin_run_routes_state()).mount(router);
        router = state.config_routes_state().mount(router);
    }
    if admin_config.expose_eval_routes {
        router = state.eval_routes_state().mount(router);
    }
    if admin_config.expose_trace_routes {
        router = state.trace_routes_state().mount(router);
    }

    router
        .route("/metrics", get(crate::metrics::metrics_handler))
        .layer(middleware::from_fn(crate::metrics::http_metrics_middleware))
}

pub(crate) fn trace_routes() -> Router<TraceRoutesState> {
    Router::new()
        .route("/v1/traces", get(list_traces))
        .route("/v1/traces/:run_id", get(get_trace))
        .route("/v1/traces/:run_id/pin", post(pin_trace))
}

pub(crate) fn health_routes() -> Router<RunRoutesState> {
    Router::new()
        .route("/health", get(health_ready))
        .route("/health/live", get(health_live))
}

pub(crate) fn thread_routes() -> Router<RunRoutesState> {
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

pub(crate) fn run_routes() -> Router<RunRoutesState> {
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

// ── Health ──

/// Liveness probe — always returns 200.  Use for k8s `livenessProbe`.
#[tracing::instrument]
async fn health_live() -> impl IntoResponse {
    StatusCode::OK
}

/// Readiness probe — checks that critical dependencies are reachable.
/// Returns 200 with `"status":"healthy"` when everything is fine, or 503
/// with `"status":"unhealthy"` when a component check fails.
///
/// Individual component checks are capped at 5 seconds to avoid blocking
/// the probe.
#[tracing::instrument(skip(st))]
async fn health_ready(State(st): State<RunRoutesState>) -> impl IntoResponse {
    const CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    // -- Store check: attempt a lightweight list operation.
    let store_status =
        match tokio::time::timeout(CHECK_TIMEOUT, st.run.store().list_threads(0, 1)).await {
            Ok(Ok(_)) => "ok",
            Ok(Err(_)) => "error",
            Err(_) => "timeout",
        };

    // -- Runtime check: the runtime is healthy if it exists (it is
    //    always present once ServerState is constructed).
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
    State(st): State<RunRoutesState>,
    Query(params): Query<ThreadQueryParams>,
) -> Result<Json<Value>, ApiError> {
    let query = params.storage_query().map_err(ApiError::BadRequest)?;
    let page = st
        .run
        .store()
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
    State(st): State<RunRoutesState>,
    Query(params): Query<ThreadQueryParams>,
) -> Result<Json<Value>, ApiError> {
    let query = params.storage_query().map_err(ApiError::BadRequest)?;
    let page = st
        .run
        .store()
        .list_threads_query(&query)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let mut items = Vec::with_capacity(page.items.len());
    for id in page.items {
        let latest_run = st
            .run
            .store()
            .latest_run(&id)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        if let Some(thread) = st
            .run
            .store()
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
    State(st): State<RunRoutesState>,
    Json(payload): Json<CreateThreadPayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let thread = crate::services::thread_events::create_thread(
        &st,
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
    State(st): State<RunRoutesState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let thread = st
        .run
        .store()
        .load_thread(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or(ApiError::ThreadNotFound(id))?;
    let value = serde_json::to_value(thread).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(value))
}

#[tracing::instrument(skip(st), fields(thread_id = %id))]
async fn delete_thread(
    State(st): State<RunRoutesState>,
    Path(id): Path<String>,
    Query(params): Query<DeleteThreadParams>,
) -> Result<StatusCode, ApiError> {
    crate::services::thread_events::delete_thread(
        &st,
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
    State(st): State<RunRoutesState>,
    Path(id): Path<String>,
    Json(payload): Json<PatchThreadPayload>,
) -> Result<Json<Value>, ApiError> {
    let thread = crate::services::thread_events::update_thread(
        &st,
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
    State(st): State<RunRoutesState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let interrupted = RunControlService::new(st.run.clone())
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
    State(st): State<RunRoutesState>,
    Path(id): Path<String>,
    Query(params): Query<MessageQueryParams>,
) -> Result<Json<Value>, ApiError> {
    // Verify thread exists
    st.run
        .store()
        .load_thread(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or(ApiError::ThreadNotFound(id.clone()))?;

    let query = params.storage_query().map_err(ApiError::BadRequest)?;
    let page = st
        .run
        .store()
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
    State(st): State<RunRoutesState>,
    Path(id): Path<String>,
    Json(payload): Json<PostThreadMessagesPayload>,
) -> Result<Response, ApiError> {
    // Require existing thread for thread-centric API semantics.
    st.run
        .store()
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

    let service = RunControlService::new(st.run.clone());
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
    State(st): State<RunRoutesState>,
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
        .run
        .mailbox
        .submit_background(RunActivation::new(id, messages))
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
    State(st): State<RunRoutesState>,
    Path(id): Path<String>,
    Query(params): Query<ListParams>,
) -> Result<Json<Value>, ApiError> {
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.clamp(1, 200);
    let dispatches = st
        .run
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
    State(st): State<RunRoutesState>,
    Json(payload): Json<CreateRunPayload>,
) -> Result<Response, ApiError> {
    let agent_id = payload.agent_id.trim().to_string();
    if agent_id.is_empty() {
        return Err(ApiError::BadRequest("agent_id cannot be empty".to_string()));
    }

    let messages = convert_run_messages(payload.messages);
    let (thread_id, messages) = crate::request::prepare_run_inputs(payload.thread_id, messages)?;

    let request = RunActivation::new(thread_id, messages).with_agent_id(agent_id);
    let (_result, event_rx) = st
        .run
        .mailbox
        .submit(request)
        .await
        .map_err(map_mailbox_error)?;
    let encoder = awaken_contract::contract::transport::Identity::default();
    let sse_rx = wire_sse_relay(event_rx, encoder, st.sse_buffer_size, None);

    Ok(sse_response(sse_body_stream(sse_rx)))
}

#[tracing::instrument(skip(st), fields(run_id = %id))]
async fn get_run(
    State(st): State<RunRoutesState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let record = crate::services::run_service::get_run(st.run.store().as_ref(), &id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or(ApiError::RunNotFound(id))?;
    let value = serde_json::to_value(record).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(value))
}

#[tracing::instrument(skip(st))]
async fn list_runs(
    State(st): State<RunRoutesState>,
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
    let page = crate::services::run_service::list_runs(st.run.store().as_ref(), &query)
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
    State(st): State<RunRoutesState>,
    Path(id): Path<String>,
    Json(payload): Json<PushRunInputsPayload>,
) -> Result<Response, ApiError> {
    let messages = convert_run_messages(payload.messages);
    if messages.is_empty() {
        return Err(ApiError::BadRequest(
            "at least one message is required".to_string(),
        ));
    }

    let service = RunControlService::new(st.run.clone());
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
    State(st): State<RunRoutesState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    RunControlService::new(st.run.clone())
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
    State(st): State<RunRoutesState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    RunControlService::new(st.run.clone())
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
    State(st): State<RunRoutesState>,
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

    RunControlService::new(st.run.clone())
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
    State(st): State<RunRoutesState>,
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

    RunControlService::new(st.run.clone())
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
    State(st): State<RunRoutesState>,
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
    let page = crate::services::run_service::list_runs(st.run.store().as_ref(), &query)
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
    State(st): State<RunRoutesState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let record = crate::services::run_service::latest_run(st.run.store().as_ref(), &id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or(ApiError::RunNotFound(format!("no runs for thread {id}")))?;
    let value = serde_json::to_value(record).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(value))
}

#[tracing::instrument(skip(st), fields(thread_id = %id))]
async fn active_thread_run(
    State(st): State<RunRoutesState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let active = RunControlService::new(st.run.clone())
        .get_active_run(&id)
        .await
        .map_err(map_run_control_error)?;
    Ok(Json(json!({ "active_run": active })))
}

#[cfg(test)]
#[path = "routes_test.rs"]
mod tests;
