//! Axum router setup — unified route registration.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use awaken_contract::contract::message::Message;

use crate::app::AppState;
use crate::config_routes::config_routes;
use crate::http_run::wire_sse_relay;
use crate::http_sse::{sse_body_stream, sse_response};
use crate::mailbox::{ACTIVE_RUN_CONFLICT_MESSAGE, MailboxDispatchStatus, MailboxError};
use crate::protocols::a2a::http::a2a_routes;
use crate::protocols::ag_ui::http::ag_ui_routes;
use crate::protocols::ai_sdk_v6::http::ai_sdk_routes;
use crate::protocols::mcp::http::mcp_routes;
use crate::query::{self, MessageQueryParams};
use awaken_runtime::RunRequest;

/// API error type returned by route handlers.
#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    Conflict(String),
    NotFound(String),
    ThreadNotFound(String),
    RunNotFound(String),
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            ApiError::Conflict(msg) => (StatusCode::CONFLICT, msg),
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, msg),
            ApiError::ThreadNotFound(id) => {
                (StatusCode::NOT_FOUND, format!("thread not found: {id}"))
            }
            ApiError::RunNotFound(id) => (StatusCode::NOT_FOUND, format!("run not found: {id}")),
            ApiError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };
        (status, Json(json!({"error": message}))).into_response()
    }
}

pub(crate) fn map_mailbox_error(error: MailboxError) -> ApiError {
    match error {
        MailboxError::Validation(msg) if msg == ACTIVE_RUN_CONFLICT_MESSAGE => {
            ApiError::Conflict(msg)
        }
        MailboxError::Validation(msg) => ApiError::BadRequest(msg),
        MailboxError::Store(err) => ApiError::Internal(err.to_string()),
        MailboxError::Internal(msg) => ApiError::Internal(msg),
    }
}

/// Build the complete router with all routes and a concurrency limit layer.
///
/// `max_concurrent` controls the maximum number of in-flight requests.
/// Pass `0` to disable the limit (useful in tests).
pub fn build_router() -> Router<AppState> {
    Router::new()
        .merge(health_routes())
        .merge(thread_routes())
        .merge(run_routes())
        .merge(config_routes())
        .merge(ai_sdk_routes())
        .merge(ag_ui_routes())
        .merge(a2a_routes())
        .merge(mcp_routes())
        .route("/metrics", get(crate::metrics::metrics_handler))
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
    Query(params): Query<ListParams>,
) -> Result<Json<Value>, ApiError> {
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.clamp(1, 200);
    let ids = st
        .store
        .list_threads(offset, limit)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(
        json!({ "items": ids, "offset": offset, "limit": limit }),
    ))
}

#[tracing::instrument(skip(st))]
async fn list_thread_summaries(
    State(st): State<AppState>,
    Query(params): Query<ListParams>,
) -> Result<Json<Value>, ApiError> {
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.clamp(1, 200);
    let ids = st
        .store
        .list_threads(offset, limit)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let mut items = Vec::with_capacity(ids.len());
    for id in ids {
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
                "title": thread.metadata.title,
                "updated_at": thread.metadata.updated_at,
                "agent_id": latest_run.map(|run| run.agent_id),
            }));
        }
    }
    Ok(Json(
        json!({ "items": items, "offset": offset, "limit": limit }),
    ))
}

#[derive(Debug, Deserialize)]
struct CreateThreadPayload {
    #[serde(default)]
    title: Option<String>,
}

#[tracing::instrument(skip(st, payload))]
async fn create_thread(
    State(st): State<AppState>,
    Json(payload): Json<CreateThreadPayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let thread = crate::services::thread_service::create_thread(st.store.as_ref(), payload.title)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
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
) -> Result<StatusCode, ApiError> {
    // Verify thread exists
    st.store
        .load_thread(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or(ApiError::ThreadNotFound(id.clone()))?;

    st.store
        .delete_thread(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct PatchThreadPayload {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    custom: Option<std::collections::HashMap<String, Value>>,
}

#[tracing::instrument(skip(st, payload), fields(thread_id = %id))]
async fn patch_thread(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(payload): Json<PatchThreadPayload>,
) -> Result<Json<Value>, ApiError> {
    let mut thread = st
        .store
        .load_thread(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or(ApiError::ThreadNotFound(id.clone()))?;

    if let Some(title) = payload.title {
        thread.metadata.title = Some(title);
    }
    if let Some(custom) = payload.custom {
        for (key, value) in custom {
            thread.metadata.custom.insert(key, value);
        }
    }
    thread.metadata.updated_at = Some(awaken_contract::now_ms());

    st.store
        .save_thread(&thread)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let value = serde_json::to_value(thread).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(value))
}

#[tracing::instrument(skip(st), fields(thread_id = %id))]
async fn interrupt_thread(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let interrupted = st
        .mailbox
        .interrupt(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    if interrupted.active_job.is_some() || interrupted.superseded_count > 0 {
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "interrupt_requested",
                "thread_id": id,
                "superseded_jobs": interrupted.superseded_count,
            })),
        )
            .into_response());
    }

    Err(ApiError::ThreadNotFound(id))
}

#[tracing::instrument(skip(st), fields(thread_id = %id))]
async fn get_thread_messages(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<MessageQueryParams>,
) -> Result<Json<Value>, ApiError> {
    use awaken_contract::contract::message::Visibility;

    // Verify thread exists
    st.store
        .load_thread(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or(ApiError::ThreadNotFound(id.clone()))?;

    let messages = st
        .store
        .load_messages(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .unwrap_or_default();

    // Filter by visibility unless `?visibility=all` is specified
    let include_internal = params
        .visibility
        .as_deref()
        .is_some_and(|v| v.eq_ignore_ascii_case("all"));

    let messages: Vec<_> = if include_internal {
        messages
    } else {
        messages
            .into_iter()
            .filter(|m| m.visibility != Visibility::Internal)
            .collect()
    };

    let offset = params.offset_or_default();
    let limit = params.clamped_limit();
    let total = messages.len();
    let slice: Vec<_> = messages.into_iter().skip(offset).take(limit).collect();
    let has_more = offset + slice.len() < total;

    let value = serde_json::to_value(&slice).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(json!({
        "messages": value,
        "total": total,
        "has_more": has_more,
    })))
}

#[derive(Debug, Deserialize)]
struct PostThreadMessagesPayload {
    #[serde(rename = "agentId", alias = "agent_id", default)]
    agent_id: Option<String>,
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

    let mut request = RunRequest::new(id.clone(), messages);
    if let Some(aid) = payload.agent_id {
        request = request.with_agent_id(aid);
    }
    let result = st
        .mailbox
        .submit_background(request)
        .await
        .map_err(map_mailbox_error)?;

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
            "job_id": result.job_id,
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
    let limit = params.limit.clamp(1, 200);
    let jobs = st
        .mailbox
        .list_jobs(&id, None, limit, 0)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let value = serde_json::to_value(&jobs).map_err(|e| ApiError::Internal(e.to_string()))?;
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
    messages: Vec<RunMessage>,
}

#[tracing::instrument(skip(st, payload), fields(run_id = %id))]
async fn push_run_inputs(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(payload): Json<PushRunInputsPayload>,
) -> Result<Response, ApiError> {
    let run = crate::services::run_service::get_run(st.store.as_ref(), &id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or(ApiError::RunNotFound(id.clone()))?;

    let messages = convert_run_messages(payload.messages);
    if messages.is_empty() {
        return Err(ApiError::BadRequest(
            "at least one message is required".to_string(),
        ));
    }

    let request = RunRequest::new(run.thread_id, messages).with_agent_id(run.agent_id);
    let _ = st
        .mailbox
        .submit_background(request)
        .await
        .map_err(map_mailbox_error)?;

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
    let cancelled = st
        .mailbox
        .cancel(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    if cancelled {
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "cancel_requested",
                "run_id": id,
            })),
        )
            .into_response());
    }

    Err(ApiError::RunNotFound(id))
}

#[tracing::instrument(skip(st), fields(thread_id = %id))]
async fn cancel_thread(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let cancelled = st
        .mailbox
        .cancel(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    if cancelled {
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "cancel_requested",
                "thread_id": id,
            })),
        )
            .into_response());
    }

    Err(ApiError::ThreadNotFound(id))
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
        updated_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    };

    let sent = st
        .mailbox
        .send_decision(&id, payload.tool_call_id.clone(), resume);

    if sent {
        Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "decision_submitted",
                "run_id": id,
                "tool_call_id": payload.tool_call_id,
            })),
        )
            .into_response())
    } else {
        Err(ApiError::RunNotFound(id))
    }
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
        updated_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    };

    let sent = st
        .mailbox
        .send_decision(&id, payload.tool_call_id.clone(), resume);

    if sent {
        Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "decision_submitted",
                "thread_id": id,
                "tool_call_id": payload.tool_call_id,
            })),
        )
            .into_response())
    } else {
        Err(ApiError::ThreadNotFound(id))
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_error_bad_request_response() {
        let err = ApiError::BadRequest("missing field".into());
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn api_error_not_found_response() {
        let err = ApiError::NotFound("resource".into());
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn api_error_thread_not_found_response() {
        let err = ApiError::ThreadNotFound("t-123".into());
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn api_error_run_not_found_response() {
        let err = ApiError::RunNotFound("r-123".into());
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn api_error_internal_response() {
        let err = ApiError::Internal("db error".into());
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn convert_run_messages_works() {
        let msgs = vec![
            RunMessage {
                role: "user".into(),
                content: "hello".into(),
            },
            RunMessage {
                role: "unknown".into(),
                content: "x".into(),
            },
        ];
        let converted = convert_run_messages(msgs);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].text(), "hello");
    }

    #[test]
    fn list_params_defaults() {
        let params: ListParams = serde_json::from_str("{}").unwrap();
        assert_eq!(params.offset, None);
        assert_eq!(params.limit, 50);
    }

    #[test]
    fn decision_payload_deserialize() {
        let json = r#"{"toolCallId":"c1","action":"resume","payload":{"approved":true}}"#;
        let payload: DecisionPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.tool_call_id, "c1");
        assert_eq!(payload.action, "resume");
    }

    // ── CreateRunPayload deserialization ──

    #[test]
    fn create_run_payload_camel_case() {
        let json =
            r#"{"agentId":"a1","threadId":"t1","messages":[{"role":"user","content":"hi"}]}"#;
        let p: CreateRunPayload = serde_json::from_str(json).unwrap();
        assert_eq!(p.agent_id, "a1");
        assert_eq!(p.thread_id.as_deref(), Some("t1"));
        assert_eq!(p.messages.len(), 1);
        assert_eq!(p.messages[0].role, "user");
        assert_eq!(p.messages[0].content, "hi");
    }

    #[test]
    fn create_run_payload_snake_case_alias() {
        let json = r#"{"agent_id":"a2","thread_id":"t2","messages":[]}"#;
        let p: CreateRunPayload = serde_json::from_str(json).unwrap();
        assert_eq!(p.agent_id, "a2");
        assert_eq!(p.thread_id.as_deref(), Some("t2"));
        assert!(p.messages.is_empty());
    }

    #[test]
    fn create_run_payload_defaults() {
        let json = r#"{"agentId":"a3"}"#;
        let p: CreateRunPayload = serde_json::from_str(json).unwrap();
        assert_eq!(p.agent_id, "a3");
        assert_eq!(p.thread_id, None);
        assert!(p.messages.is_empty());
    }

    #[test]
    fn create_run_payload_missing_agent_id_fails() {
        let json = r#"{"messages":[]}"#;
        let result = serde_json::from_str::<CreateRunPayload>(json);
        assert!(result.is_err());
    }

    // ── PushRunInputsPayload deserialization ──

    #[test]
    fn push_run_inputs_payload_empty_default() {
        let json = r#"{}"#;
        let p: PushRunInputsPayload = serde_json::from_str(json).unwrap();
        assert!(p.messages.is_empty());
    }

    #[test]
    fn push_run_inputs_payload_with_messages() {
        let json =
            r#"{"messages":[{"role":"user","content":"msg1"},{"role":"user","content":"msg2"}]}"#;
        let p: PushRunInputsPayload = serde_json::from_str(json).unwrap();
        assert_eq!(p.messages.len(), 2);
        assert_eq!(p.messages[0].content, "msg1");
        assert_eq!(p.messages[1].content, "msg2");
    }

    // ── ListRunsParams deserialization ──

    #[test]
    fn list_runs_params_defaults() {
        let p: ListRunsParams = serde_json::from_str("{}").unwrap();
        assert_eq!(p.offset, None);
        assert_eq!(p.limit, 50);
        assert_eq!(p.status, None);
    }

    #[test]
    fn list_runs_params_with_status_filter() {
        let json = r#"{"offset":10,"limit":25,"status":"running"}"#;
        let p: ListRunsParams = serde_json::from_str(json).unwrap();
        assert_eq!(p.offset, Some(10));
        assert_eq!(p.limit, 25);
        assert_eq!(p.status.as_deref(), Some("running"));
    }

    // ── PatchThreadPayload deserialization ──

    #[test]
    fn patch_thread_payload_title_only() {
        let json = r#"{"title":"new title"}"#;
        let p: PatchThreadPayload = serde_json::from_str(json).unwrap();
        assert_eq!(p.title.as_deref(), Some("new title"));
        assert!(p.custom.is_none());
    }

    #[test]
    fn patch_thread_payload_custom_only() {
        let json = r#"{"custom":{"key":"value"}}"#;
        let p: PatchThreadPayload = serde_json::from_str(json).unwrap();
        assert!(p.title.is_none());
        let custom = p.custom.unwrap();
        assert_eq!(custom.get("key").unwrap(), "value");
    }

    #[test]
    fn patch_thread_payload_both() {
        let json = r#"{"title":"t","custom":{"k":"v"}}"#;
        let p: PatchThreadPayload = serde_json::from_str(json).unwrap();
        assert_eq!(p.title.as_deref(), Some("t"));
        assert!(p.custom.is_some());
    }

    #[test]
    fn patch_thread_payload_empty() {
        let json = r#"{}"#;
        let p: PatchThreadPayload = serde_json::from_str(json).unwrap();
        assert!(p.title.is_none());
        assert!(p.custom.is_none());
    }

    // ── DecisionPayload additional tests ──

    #[test]
    fn decision_payload_snake_case_alias() {
        let json = r#"{"tool_call_id":"c2","action":"cancel"}"#;
        let p: DecisionPayload = serde_json::from_str(json).unwrap();
        assert_eq!(p.tool_call_id, "c2");
        assert_eq!(p.action, "cancel");
        assert_eq!(p.payload, Value::Null);
    }

    #[test]
    fn decision_payload_missing_required_fails() {
        // missing action
        let json = r#"{"toolCallId":"c1"}"#;
        assert!(serde_json::from_str::<DecisionPayload>(json).is_err());

        // missing tool_call_id
        let json = r#"{"action":"resume"}"#;
        assert!(serde_json::from_str::<DecisionPayload>(json).is_err());
    }

    // ── convert_run_messages edge cases ──

    #[test]
    fn convert_run_messages_empty() {
        let converted = convert_run_messages(vec![]);
        assert!(converted.is_empty());
    }

    #[test]
    fn convert_run_messages_system_role() {
        let msgs = vec![RunMessage {
            role: "system".into(),
            content: "you are helpful".into(),
        }];
        let converted = convert_run_messages(msgs);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].text(), "you are helpful");
    }

    #[test]
    fn convert_run_messages_assistant_role() {
        let msgs = vec![RunMessage {
            role: "assistant".into(),
            content: "sure".into(),
        }];
        let converted = convert_run_messages(msgs);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].text(), "sure");
    }

    #[test]
    fn convert_run_messages_mixed_known_unknown() {
        let msgs = vec![
            RunMessage {
                role: "user".into(),
                content: "a".into(),
            },
            RunMessage {
                role: "assistant".into(),
                content: "b".into(),
            },
            RunMessage {
                role: "system".into(),
                content: "c".into(),
            },
            RunMessage {
                role: "function".into(),
                content: "d".into(),
            },
            RunMessage {
                role: "tool".into(),
                content: "e".into(),
            },
        ];
        let converted = convert_run_messages(msgs);
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[0].text(), "a");
        assert_eq!(converted[1].text(), "b");
        assert_eq!(converted[2].text(), "c");
    }

    // ── ApiError response body validation ──

    #[tokio::test]
    async fn api_error_thread_not_found_body() {
        let err = ApiError::ThreadNotFound("t-abc".into());
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["error"], "thread not found: t-abc");
    }

    #[tokio::test]
    async fn api_error_run_not_found_body() {
        let err = ApiError::RunNotFound("r-xyz".into());
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["error"], "run not found: r-xyz");
    }

    #[tokio::test]
    async fn api_error_internal_body() {
        let err = ApiError::Internal("db crashed".into());
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["error"], "db crashed");
    }

    #[tokio::test]
    async fn api_error_bad_request_body() {
        let err = ApiError::BadRequest("invalid input".into());
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["error"], "invalid input");
    }

    // ── CreateThreadPayload deserialization ──

    #[test]
    fn create_thread_payload_with_title() {
        let json = r#"{"title":"my thread"}"#;
        let p: CreateThreadPayload = serde_json::from_str(json).unwrap();
        assert_eq!(p.title.as_deref(), Some("my thread"));
    }

    #[test]
    fn create_thread_payload_empty() {
        let json = r#"{}"#;
        let p: CreateThreadPayload = serde_json::from_str(json).unwrap();
        assert!(p.title.is_none());
    }

    // ── MailboxPayload deserialization ──

    #[test]
    fn mailbox_payload_default() {
        let json = r#"{}"#;
        let p: MailboxPayload = serde_json::from_str(json).unwrap();
        assert_eq!(p.payload, Value::Null);
    }

    #[test]
    fn mailbox_payload_with_content() {
        let json = r#"{"payload":{"text":"hello"}}"#;
        let p: MailboxPayload = serde_json::from_str(json).unwrap();
        assert_eq!(p.payload["text"], "hello");
    }

    // ── PostThreadMessagesPayload deserialization ──

    #[test]
    fn post_thread_messages_payload_camel_case() {
        let json = r#"{"agentId":"agent-1","messages":[{"role":"user","content":"test"}]}"#;
        let p: PostThreadMessagesPayload = serde_json::from_str(json).unwrap();
        assert_eq!(p.agent_id.as_deref(), Some("agent-1"));
        assert_eq!(p.messages.len(), 1);
    }

    #[test]
    fn post_thread_messages_payload_snake_case_alias() {
        let json = r#"{"agent_id":"agent-2","messages":[]}"#;
        let p: PostThreadMessagesPayload = serde_json::from_str(json).unwrap();
        assert_eq!(p.agent_id.as_deref(), Some("agent-2"));
    }

    #[test]
    fn post_thread_messages_payload_defaults() {
        let json = r#"{}"#;
        let p: PostThreadMessagesPayload = serde_json::from_str(json).unwrap();
        assert!(p.agent_id.is_none());
        assert!(p.messages.is_empty());
    }

    // ── RunMessage deserialization ──

    #[test]
    fn run_message_deserialize() {
        let json = r#"{"role":"user","content":"hello world"}"#;
        let m: RunMessage = serde_json::from_str(json).unwrap();
        assert_eq!(m.role, "user");
        assert_eq!(m.content, "hello world");
    }

    #[test]
    fn run_message_missing_field_fails() {
        let json = r#"{"role":"user"}"#;
        assert!(serde_json::from_str::<RunMessage>(json).is_err());
    }

    // ── ListParams with explicit values ──

    #[test]
    fn list_params_explicit_values() {
        let json = r#"{"offset":5,"limit":100}"#;
        let p: ListParams = serde_json::from_str(json).unwrap();
        assert_eq!(p.offset, Some(5));
        assert_eq!(p.limit, 100);
    }

    // ── Health check integration tests ──────────────────────────────

    mod health_integration {
        use super::*;
        use crate::app::{AppState, ServerConfig};
        use crate::mailbox::{Mailbox, MailboxConfig};
        use awaken_runtime::AgentRuntime;
        use awaken_stores::{InMemoryMailboxStore, InMemoryStore};
        use http_body_util::BodyExt;
        use std::sync::Arc;
        use tower::ServiceExt;

        struct StubResolver;
        impl awaken_runtime::AgentResolver for StubResolver {
            fn resolve(
                &self,
                agent_id: &str,
            ) -> Result<awaken_runtime::ResolvedAgent, awaken_runtime::RuntimeError> {
                Err(awaken_runtime::RuntimeError::AgentNotFound {
                    agent_id: agent_id.to_string(),
                })
            }
        }

        fn make_app_state() -> AppState {
            let runtime = Arc::new(AgentRuntime::new(Arc::new(StubResolver)));
            let mailbox_store = Arc::new(InMemoryMailboxStore::new());
            let mailbox = Arc::new(Mailbox::new(
                runtime.clone(),
                mailbox_store,
                "test".to_string(),
                MailboxConfig::default(),
            ));
            let store = Arc::new(InMemoryStore::new());
            AppState::new(
                runtime,
                mailbox,
                store.clone(),
                Arc::new(StubResolver),
                ServerConfig::default(),
            )
        }

        #[tokio::test]
        async fn health_live_returns_200() {
            let state = make_app_state();
            let app = build_router().with_state(state);

            let req = axum::http::Request::builder()
                .uri("/health/live")
                .body(axum::body::Body::empty())
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }

        #[tokio::test]
        async fn health_ready_returns_healthy_with_working_store() {
            let state = make_app_state();
            let app = build_router().with_state(state);

            let req = axum::http::Request::builder()
                .uri("/health")
                .body(axum::body::Body::empty())
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);

            let body = resp.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["status"], "healthy");
            assert_eq!(json["components"]["store"], "ok");
            assert_eq!(json["components"]["runtime"], "ok");
        }

        #[tokio::test]
        async fn health_ready_returns_unhealthy_with_failing_store() {
            use awaken_contract::contract::message::Message;
            use awaken_contract::contract::storage::{
                RunPage, RunQuery, RunRecord, RunStore, StorageError, ThreadRunStore, ThreadStore,
            };
            use awaken_contract::thread::{Thread, ThreadMetadata};

            /// Store that always returns errors.
            struct FailingStore;

            #[async_trait::async_trait]
            impl ThreadStore for FailingStore {
                async fn load_thread(&self, _id: &str) -> Result<Option<Thread>, StorageError> {
                    Err(StorageError::Io("simulated failure".into()))
                }
                async fn save_thread(&self, _t: &Thread) -> Result<(), StorageError> {
                    Err(StorageError::Io("simulated failure".into()))
                }
                async fn delete_thread(&self, _id: &str) -> Result<(), StorageError> {
                    Err(StorageError::Io("simulated failure".into()))
                }
                async fn list_threads(
                    &self,
                    _offset: usize,
                    _limit: usize,
                ) -> Result<Vec<String>, StorageError> {
                    Err(StorageError::Io("simulated failure".into()))
                }
                async fn load_messages(
                    &self,
                    _id: &str,
                ) -> Result<Option<Vec<Message>>, StorageError> {
                    Err(StorageError::Io("simulated failure".into()))
                }
                async fn save_messages(
                    &self,
                    _id: &str,
                    _msgs: &[Message],
                ) -> Result<(), StorageError> {
                    Err(StorageError::Io("simulated failure".into()))
                }
                async fn delete_messages(&self, _id: &str) -> Result<(), StorageError> {
                    Err(StorageError::Io("simulated failure".into()))
                }
                async fn update_thread_metadata(
                    &self,
                    _id: &str,
                    _meta: ThreadMetadata,
                ) -> Result<(), StorageError> {
                    Err(StorageError::Io("simulated failure".into()))
                }
            }

            #[async_trait::async_trait]
            impl RunStore for FailingStore {
                async fn create_run(&self, _r: &RunRecord) -> Result<(), StorageError> {
                    Err(StorageError::Io("simulated failure".into()))
                }
                async fn load_run(&self, _id: &str) -> Result<Option<RunRecord>, StorageError> {
                    Err(StorageError::Io("simulated failure".into()))
                }
                async fn latest_run(&self, _id: &str) -> Result<Option<RunRecord>, StorageError> {
                    Err(StorageError::Io("simulated failure".into()))
                }
                async fn list_runs(&self, _q: &RunQuery) -> Result<RunPage, StorageError> {
                    Err(StorageError::Io("simulated failure".into()))
                }
            }

            #[async_trait::async_trait]
            impl ThreadRunStore for FailingStore {
                async fn checkpoint(
                    &self,
                    _thread_id: &str,
                    _messages: &[Message],
                    _run: &RunRecord,
                ) -> Result<(), StorageError> {
                    Err(StorageError::Io("simulated failure".into()))
                }
            }

            let runtime = Arc::new(AgentRuntime::new(Arc::new(StubResolver)));
            let mailbox_store = Arc::new(InMemoryMailboxStore::new());
            let mailbox = Arc::new(Mailbox::new(
                runtime.clone(),
                mailbox_store,
                "test".to_string(),
                MailboxConfig::default(),
            ));
            let state = AppState::new(
                runtime,
                mailbox,
                Arc::new(FailingStore),
                Arc::new(StubResolver),
                ServerConfig::default(),
            );
            let app = build_router().with_state(state);

            let req = axum::http::Request::builder()
                .uri("/health")
                .body(axum::body::Body::empty())
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

            let body = resp.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["status"], "unhealthy");
            assert_eq!(json["components"]["store"], "error");
        }
    }
}
