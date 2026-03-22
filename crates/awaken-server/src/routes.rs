//! Axum router setup — unified route registration.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
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
        .route("/v1/threads/{id}", get(get_thread))
        .route("/v1/threads/{id}/messages", get(get_thread_messages))
        .route(
            "/v1/threads/{id}/mailbox",
            post(push_mailbox).get(peek_mailbox),
        )
}

fn run_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/runs", post(start_run))
        .route("/v1/runs/{id}", get(get_run))
        .route("/v1/runs/{id}/cancel", post(cancel_run))
        .route("/v1/runs/{id}/decision", post(submit_decision))
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
        .thread_store
        .list_threads(offset, limit)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(
        json!({ "items": ids, "offset": offset, "limit": limit }),
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
    let thread =
        crate::services::thread_service::create_thread(st.thread_store.as_ref(), payload.title)
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
        .thread_store
        .load_thread(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or(ApiError::ThreadNotFound(id))?;
    let value = serde_json::to_value(thread).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(value))
}

#[derive(Debug, Deserialize)]
struct MessageQueryParams {
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default = "default_limit")]
    limit: usize,
}

async fn get_thread_messages(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<MessageQueryParams>,
) -> Result<Json<Value>, ApiError> {
    let thread = st
        .thread_store
        .load_thread(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or(ApiError::ThreadNotFound(id))?;

    let messages = thread.messages;
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
    msgs.into_iter()
        .filter_map(|m| match m.role.as_str() {
            "user" => Some(Message::user(m.content)),
            "assistant" => Some(Message::assistant(m.content)),
            "system" => Some(Message::system(m.content)),
            _ => None,
        })
        .collect()
}

async fn start_run(
    State(st): State<AppState>,
    Json(payload): Json<CreateRunPayload>,
) -> Result<Response, ApiError> {
    let agent_id = payload.agent_id.trim().to_string();
    if agent_id.is_empty() {
        return Err(ApiError::BadRequest("agent_id cannot be empty".to_string()));
    }

    let thread_id = payload
        .thread_id
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());

    let messages = convert_run_messages(payload.messages);

    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    let encoder = awaken_contract::contract::transport::Identity::default();
    let sse_rx = wire_sse_relay(event_rx, encoder, st.config.sse_buffer_size);

    let runtime = st.runtime.clone();
    tokio::spawn(async move {
        let sink = crate::transport::channel_sink::ChannelEventSink::new(event_tx);
        let request = awaken_runtime::RunRequest::new(thread_id, messages).with_agent_id(agent_id);
        if let Err(e) = runtime.run(request, &sink).await {
            tracing::warn!(error = %e, "run failed");
        }
    });

    Ok(sse_response(sse_body_stream(sse_rx)))
}

async fn get_run(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let record = crate::services::run_service::get_run(st.run_store.as_ref(), &id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or(ApiError::RunNotFound(id))?;
    let value = serde_json::to_value(record).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(value))
}

async fn cancel_run(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    // Try to cancel via runtime's active runs (by thread_id = run_id heuristic)
    // In practice, the caller should use thread_id, but we accept run_id as a best-effort lookup.
    if st.runtime.cancel_by_thread(&id) {
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
    #[allow(dead_code)]
    payload: Value,
}

async fn submit_decision(
    State(_st): State<AppState>,
    Path(id): Path<String>,
    Json(payload): Json<DecisionPayload>,
) -> Result<Response, ApiError> {
    let _action = match payload.action.as_str() {
        "resume" | "cancel" => payload.action.clone(),
        other => {
            return Err(ApiError::BadRequest(format!(
                "invalid action: {other}, expected 'resume' or 'cancel'"
            )));
        }
    };

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
