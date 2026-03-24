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
use crate::http_run::wire_sse_relay;
use crate::http_sse::{sse_body_stream, sse_response};
use crate::protocols::a2a::http::a2a_routes;
use crate::protocols::ag_ui::http::ag_ui_routes;
use crate::protocols::ai_sdk_v6::http::ai_sdk_routes;
use crate::run_dispatcher::RunSpec;

/// API error type returned by route handlers.
#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    NotFound(String),
    ThreadNotFound(String),
    RunNotFound(String),
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
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

/// Build the complete router with all routes.
pub fn build_router() -> Router<AppState> {
    Router::new()
        .merge(health_routes())
        .merge(thread_routes())
        .merge(run_routes())
        .merge(ai_sdk_routes())
        .merge(ag_ui_routes())
        .merge(a2a_routes())
}

fn health_routes() -> Router<AppState> {
    Router::new().route("/health", get(health))
}

fn thread_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/threads", get(list_threads).post(create_thread))
        .route("/v1/threads/summaries", get(list_thread_summaries))
        .route(
            "/v1/threads/:id",
            get(get_thread).delete(delete_thread).patch(patch_thread),
        )
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

async fn health() -> impl IntoResponse {
    StatusCode::OK
}

// ── Threads ──

fn default_limit() -> usize {
    50
}

#[derive(Debug, Deserialize)]
struct ListParams {
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default = "default_limit")]
    limit: usize,
}

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
    thread.metadata.updated_at = Some(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    );

    st.store
        .save_thread(&thread)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let value = serde_json::to_value(thread).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(value))
}

async fn interrupt_thread(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    if st.dispatcher.cancel_run(&id) {
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "interrupt_requested",
                "thread_id": id,
            })),
        )
            .into_response());
    }

    Err(ApiError::ThreadNotFound(id))
}

#[derive(Debug, Deserialize)]
struct MessageQueryParams {
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default = "default_limit")]
    limit: usize,
    /// Pass `visibility=all` to include internal messages; otherwise they are filtered out.
    #[serde(default)]
    visibility: Option<String>,
}

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

    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.clamp(1, 200);
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

    let (status, _rx) = st
        .dispatcher
        .dispatch_with_status(RunSpec {
            thread_id: id.clone(),
            agent_id: payload.agent_id,
            messages,
        })
        .await;

    let body = match status {
        crate::run_dispatcher::DispatchStatus::Running => json!({
            "status": "running",
            "thread_id": id,
        }),
        crate::run_dispatcher::DispatchStatus::Queued { pending_ahead } => json!({
            "status": "queued",
            "thread_id": id,
            "pending_ahead": pending_ahead,
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

async fn push_mailbox(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<MailboxPayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let entry = crate::services::mailbox_service::push_message(
        st.mailbox_store.as_ref(),
        &id,
        body.payload,
    )
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?;

    let value = serde_json::to_value(&entry).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok((StatusCode::CREATED, Json(value)))
}

async fn peek_mailbox(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<ListParams>,
) -> Result<Json<Value>, ApiError> {
    let limit = params.limit.clamp(1, 200);
    let entries =
        crate::services::mailbox_service::peek_messages(st.mailbox_store.as_ref(), &id, limit)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;

    let value = serde_json::to_value(&entries).map_err(|e| ApiError::Internal(e.to_string()))?;
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

async fn start_run(
    State(st): State<AppState>,
    Json(payload): Json<CreateRunPayload>,
) -> Result<Response, ApiError> {
    let agent_id = payload.agent_id.trim().to_string();
    if agent_id.is_empty() {
        return Err(ApiError::BadRequest("agent_id cannot be empty".to_string()));
    }

    let messages = convert_run_messages(payload.messages);
    let (thread_id, messages) =
        crate::run_dispatcher::prepare_run_inputs(payload.thread_id, messages)?;

    let spec = RunSpec {
        thread_id,
        agent_id: Some(agent_id),
        messages,
    };
    let event_rx = st.dispatcher.dispatch(spec).await;
    let encoder = awaken_contract::contract::transport::Identity::default();
    let sse_rx = wire_sse_relay(event_rx, encoder, st.config.sse_buffer_size);

    Ok(sse_response(sse_body_stream(sse_rx)))
}

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

    let spec = RunSpec {
        thread_id: run.thread_id,
        agent_id: Some(run.agent_id),
        messages,
    };
    let _ = st.dispatcher.dispatch(spec).await;

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "inputs_accepted",
            "run_id": id,
        })),
    )
        .into_response())
}

async fn cancel_run(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    // Dual-index lookup: tries run_id first, then thread_id.
    if st.dispatcher.cancel_run(&id) {
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

#[derive(Debug, Deserialize)]
struct DecisionPayload {
    #[serde(rename = "toolCallId", alias = "tool_call_id")]
    tool_call_id: String,
    action: String,
    #[serde(default)]
    payload: Value,
}

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
        .dispatcher
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

// ── Thread Runs ──

#[derive(Debug, Deserialize)]
struct ListRunsParams {
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    status: Option<String>,
}

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
}
