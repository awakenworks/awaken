use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use tirea_agentos::contracts::storage::{MessagePage, ThreadListPage, ThreadListQuery};
use tirea_agentos::contracts::thread::Message;
use tirea_agentos::contracts::thread::Thread;
use tirea_agentos::contracts::{RunRequest, ToolCallDecision};
use tirea_agentos::orchestrator::AgentOsRunError;
use tirea_contract::storage::{RunOrigin, RunPage, RunQuery, RunRecord, RunRecordStatus};
use tirea_contract::{AgentEvent, Identity};

use crate::run_service::{global_run_service, wrap_with_run_tracking};
use crate::service::{
    check_run_liveness, load_run_record, parse_message_query, prepare_http_run, remove_active_run,
    resolve_thread_id_from_run, start_background_run, try_cancel_active_run_by_id,
    try_forward_decisions_to_active_run_by_id, ApiError, MessageQueryParams, RunLookup,
};
use crate::transport::http_run::wire_http_sse_relay;
use crate::transport::http_sse::{sse_body_stream, sse_response};

pub use crate::service::AppState;

const HEALTH_PATH: &str = "/health";
const THREADS_PATH: &str = "/v1/threads";
const THREAD_PATH: &str = "/v1/threads/:id";
const THREAD_MESSAGES_PATH: &str = "/v1/threads/:id/messages";
const RUNS_PATH: &str = "/v1/runs";
const RUN_PATH: &str = "/v1/runs/:id";
const RUN_INPUTS_PATH: &str = "/v1/runs/:id/inputs";
const RUN_CANCEL_PATH: &str = "/v1/runs/:id/cancel";

/// Build health routes.
pub fn health_routes() -> Router<AppState> {
    Router::new().route(HEALTH_PATH, get(health))
}

/// Build canonical thread query routes.
pub fn thread_routes() -> Router<AppState> {
    Router::new()
        .route(THREADS_PATH, get(list_threads))
        .route(THREAD_PATH, get(get_thread))
        .route(THREAD_MESSAGES_PATH, get(get_thread_messages))
}

/// Build run projection query routes.
pub fn run_routes() -> Router<AppState> {
    Router::new()
        .route(RUNS_PATH, get(list_runs).post(start_run))
        .route(RUN_PATH, get(get_run))
        .route(RUN_INPUTS_PATH, post(push_run_inputs))
        .route(RUN_CANCEL_PATH, post(cancel_run))
}

async fn health() -> impl IntoResponse {
    StatusCode::OK
}

fn default_thread_limit() -> usize {
    50
}

#[derive(Debug, Deserialize)]
struct ThreadListParams {
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default = "default_thread_limit")]
    limit: usize,
    #[serde(default)]
    parent_thread_id: Option<String>,
}

async fn list_threads(
    State(st): State<AppState>,
    Query(params): Query<ThreadListParams>,
) -> Result<Json<ThreadListPage>, ApiError> {
    let query = ThreadListQuery {
        offset: params.offset.unwrap_or(0),
        limit: params.limit.clamp(1, 200),
        resource_id: None,
        parent_thread_id: params.parent_thread_id,
    };
    st.read_store
        .list_paginated(&query)
        .await
        .map(Json)
        .map_err(|e| ApiError::Internal(e.to_string()))
}

async fn get_thread(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Thread>, ApiError> {
    let Some(thread) = st
        .read_store
        .load_thread(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
    else {
        return Err(ApiError::ThreadNotFound(id));
    };
    Ok(Json(thread))
}

async fn get_thread_messages(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<MessageQueryParams>,
) -> Result<Json<MessagePage>, ApiError> {
    let query = parse_message_query(&params);
    st.read_store
        .load_messages(&id, &query)
        .await
        .map(Json)
        .map_err(|e| match e {
            tirea_agentos::contracts::storage::ThreadStoreError::NotFound(_) => {
                ApiError::ThreadNotFound(id)
            }
            other => ApiError::Internal(other.to_string()),
        })
}

#[derive(Debug, Deserialize)]
struct RunListParams {
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default = "default_thread_limit")]
    limit: usize,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    parent_run_id: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    origin: Option<String>,
    #[serde(default)]
    created_at_from: Option<u64>,
    #[serde(default)]
    created_at_to: Option<u64>,
    #[serde(default)]
    updated_at_from: Option<u64>,
    #[serde(default)]
    updated_at_to: Option<u64>,
}

fn parse_run_status(raw: &str) -> Option<RunRecordStatus> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "submitted" => Some(RunRecordStatus::Submitted),
        "working" => Some(RunRecordStatus::Working),
        "input_required" => Some(RunRecordStatus::InputRequired),
        "auth_required" => Some(RunRecordStatus::AuthRequired),
        "completed" => Some(RunRecordStatus::Completed),
        "failed" => Some(RunRecordStatus::Failed),
        "canceled" | "cancelled" => Some(RunRecordStatus::Canceled),
        "rejected" => Some(RunRecordStatus::Rejected),
        _ => None,
    }
}

fn parse_run_origin(raw: &str) -> Option<RunOrigin> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "user" => Some(RunOrigin::User),
        "subagent" => Some(RunOrigin::Subagent),
        "ag_ui" | "ag-ui" | "agui" => Some(RunOrigin::AgUi),
        "ai_sdk" | "ai-sdk" | "aisdk" => Some(RunOrigin::AiSdk),
        "a2a" => Some(RunOrigin::A2a),
        "internal" => Some(RunOrigin::Internal),
        _ => None,
    }
}

fn normalize_optional_id(value: Option<String>) -> Option<String> {
    value.and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

#[derive(Debug, Deserialize)]
struct CreateRunPayload {
    #[serde(rename = "agentId", alias = "agent_id")]
    agent_id: String,
    #[serde(
        rename = "threadId",
        alias = "thread_id",
        alias = "context_id",
        alias = "contextId",
        default
    )]
    thread_id: Option<String>,
    #[serde(
        rename = "runId",
        alias = "run_id",
        alias = "task_id",
        alias = "taskId",
        default
    )]
    run_id: Option<String>,
    #[serde(rename = "parentRunId", alias = "parent_run_id", default)]
    parent_run_id: Option<String>,
    #[serde(
        rename = "parentThreadId",
        alias = "parent_thread_id",
        alias = "parentContextId",
        alias = "parent_context_id",
        default
    )]
    parent_thread_id: Option<String>,
    #[serde(rename = "resourceId", alias = "resource_id", default)]
    resource_id: Option<String>,
    #[serde(default)]
    state: Option<Value>,
    #[serde(default)]
    messages: Vec<Message>,
    #[serde(
        rename = "initialDecisions",
        alias = "initial_decisions",
        alias = "decisions",
        default
    )]
    initial_decisions: Vec<ToolCallDecision>,
}

impl CreateRunPayload {
    fn into_run_request(self) -> Result<(String, RunRequest), ApiError> {
        let agent_id = self.agent_id.trim().to_string();
        if agent_id.is_empty() {
            return Err(ApiError::BadRequest("agent_id cannot be empty".to_string()));
        }

        Ok((
            agent_id.clone(),
            RunRequest {
                agent_id,
                thread_id: normalize_optional_id(self.thread_id),
                run_id: normalize_optional_id(self.run_id),
                parent_run_id: normalize_optional_id(self.parent_run_id),
                parent_thread_id: normalize_optional_id(self.parent_thread_id),
                resource_id: normalize_optional_id(self.resource_id),
                state: self.state,
                messages: self.messages,
                initial_decisions: self.initial_decisions,
            },
        ))
    }
}

#[derive(Debug, Deserialize)]
struct RunInputPayload {
    #[serde(rename = "agentId", alias = "agent_id", default)]
    agent_id: Option<String>,
    #[serde(default)]
    messages: Vec<Message>,
    #[serde(rename = "state", default)]
    state: Option<Value>,
    #[serde(rename = "resourceId", alias = "resource_id", default)]
    resource_id: Option<String>,
    #[serde(rename = "runId", alias = "run_id", default)]
    run_id: Option<String>,
    #[serde(
        rename = "decisions",
        alias = "initialDecisions",
        alias = "initial_decisions",
        default
    )]
    decisions: Vec<ToolCallDecision>,
}

async fn start_run(
    State(st): State<AppState>,
    Json(payload): Json<CreateRunPayload>,
) -> Result<Response, ApiError> {
    let (agent_id, run_request) = payload.into_run_request()?;

    let resolved = st.os.resolve(&agent_id).map_err(AgentOsRunError::from)?;
    let prepared = prepare_http_run(&st.os, resolved, run_request, "run_api", &agent_id).await?;
    let active_key = prepared.active_key.clone();

    let encoder = wrap_with_run_tracking(
        Identity::<AgentEvent>::default(),
        prepared.run_id.clone(),
        prepared.thread_id.clone(),
        "run_api",
    );
    let sse_rx = wire_http_sse_relay(
        prepared.starter,
        encoder,
        prepared.ingress_rx,
        prepared.thread_id,
        None,
        false,
        "run-api",
        move |_sse_tx| async move {
            remove_active_run(&active_key).await;
        },
        |msg| {
            let error = json!({
                "type": "error",
                "message": msg,
                "code": "RELAY_ERROR",
            });
            let payload = serde_json::to_string(&error).unwrap_or_else(|_| {
                "{\"type\":\"error\",\"message\":\"relay error\",\"code\":\"RELAY_ERROR\"}"
                    .to_string()
            });
            Bytes::from(format!("data: {payload}\n\n"))
        },
    );

    Ok(sse_response(sse_body_stream(sse_rx)))
}

async fn push_run_inputs(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(mut payload): Json<RunInputPayload>,
) -> Result<Response, ApiError> {
    payload.resource_id = normalize_optional_id(payload.resource_id);
    payload.run_id = normalize_optional_id(payload.run_id);
    let decisions = payload.decisions;

    if payload.messages.is_empty() && decisions.is_empty() {
        return Err(ApiError::BadRequest(
            "messages and decisions cannot both be empty".to_string(),
        ));
    }

    if payload.messages.is_empty() {
        if try_forward_decisions_to_active_run_by_id(&id, decisions).await {
            let thread_id = resolve_thread_id_from_run(&id).await?;
            return Ok((
                StatusCode::ACCEPTED,
                Json(json!({
                    "status": "decision_forwarded",
                    "run_id": id,
                    "thread_id": thread_id,
                })),
            )
                .into_response());
        }

        return Err(match check_run_liveness(&id).await? {
            RunLookup::ExistsButInactive => {
                ApiError::BadRequest("run is not active".to_string())
            }
            RunLookup::NotFound => ApiError::RunNotFound(id),
        });
    }

    let Some(parent_run) = load_run_record(&id).await? else {
        return Err(ApiError::RunNotFound(id));
    };
    let agent_id = payload
        .agent_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ApiError::BadRequest("agent_id is required when messages are provided".to_string())
        })?
        .to_string();
    let run_request = RunRequest {
        agent_id: agent_id.clone(),
        thread_id: Some(parent_run.thread_id.clone()),
        run_id: payload.run_id,
        parent_run_id: Some(id.clone()),
        parent_thread_id: parent_run.parent_thread_id,
        resource_id: payload.resource_id,
        state: payload.state,
        messages: payload.messages,
        initial_decisions: decisions,
    };

    let (thread_id, run_id) =
        start_background_run(&st.os, &agent_id, run_request, "run_api").await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "continuation_started",
            "parent_run_id": id,
            "thread_id": thread_id,
            "run_id": run_id,
        })),
    )
        .into_response())
}

async fn cancel_run(Path(id): Path<String>) -> Result<Response, ApiError> {
    if try_cancel_active_run_by_id(&id).await {
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "cancel_requested",
                "run_id": id,
            })),
        )
            .into_response());
    }

    Err(match check_run_liveness(&id).await? {
        RunLookup::ExistsButInactive => ApiError::BadRequest("run is not active".to_string()),
        RunLookup::NotFound => ApiError::RunNotFound(id),
    })
}

async fn get_run(Path(id): Path<String>) -> Result<Json<RunRecord>, ApiError> {
    let Some(service) = global_run_service() else {
        return Err(ApiError::Internal(
            "run service not initialized".to_string(),
        ));
    };
    let Some(record) = service
        .get_run(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
    else {
        return Err(ApiError::RunNotFound(id));
    };
    Ok(Json(record))
}

async fn list_runs(Query(params): Query<RunListParams>) -> Result<Json<RunPage>, ApiError> {
    let Some(service) = global_run_service() else {
        return Err(ApiError::Internal(
            "run service not initialized".to_string(),
        ));
    };
    let query = RunQuery {
        offset: params.offset.unwrap_or(0),
        limit: params.limit.clamp(1, 200),
        thread_id: params.thread_id,
        parent_run_id: params.parent_run_id,
        status: params.status.as_deref().and_then(parse_run_status),
        origin: params.origin.as_deref().and_then(parse_run_origin),
        created_at_from: params.created_at_from,
        created_at_to: params.created_at_to,
        updated_at_from: params.updated_at_from,
        updated_at_to: params.updated_at_to,
    };
    let page = service
        .list_runs(&query)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(page))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_run_filters() {
        assert_eq!(
            parse_run_status("completed"),
            Some(RunRecordStatus::Completed)
        );
        assert_eq!(
            parse_run_status("cancelled"),
            Some(RunRecordStatus::Canceled)
        );
        assert_eq!(parse_run_status("unknown"), None);

        assert_eq!(parse_run_origin("a2a"), Some(RunOrigin::A2a));
        assert_eq!(parse_run_origin("ag-ui"), Some(RunOrigin::AgUi));
        assert_eq!(parse_run_origin("x"), None);
    }
}
