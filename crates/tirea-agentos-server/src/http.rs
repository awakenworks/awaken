use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::Value;
use tirea_agentos::contracts::storage::{
    MailboxEntryOrigin, MailboxEntryStatus, MailboxQuery, ThreadListPage, ThreadListQuery,
};
use tirea_agentos::contracts::thread::{Message, Visibility};
use tirea_agentos::contracts::{RunRequest, ToolCallDecision};
use tirea_agentos::runtime::AgentOsRunError;
use tirea_contract::storage::{RunOrigin, RunPage, RunQuery, RunRecord, RunStatus};
use tirea_contract::{AgentEvent, Identity};

use crate::service::{
    check_run_liveness, load_run_record, normalize_optional_id, parse_message_query,
    require_agent_state_store, start_background_run, start_http_run,
    try_cancel_active_or_queued_run_by_id, try_forward_decisions_to_active_run_by_id, ApiError,
    EnqueueOptions, MessageQueryParams, RunLookup,
};
use crate::transport::http_run::{wire_http_sse_relay, HttpSseRelayConfig};
use crate::transport::http_sse::{sse_body_stream, sse_response};

pub use crate::service::AppState;

const HEALTH_PATH: &str = "/health";
const THREADS_PATH: &str = "/v1/threads";
const THREAD_SUMMARIES_PATH: &str = "/v1/threads/summaries";
const THREAD_PATH: &str = "/v1/threads/:id";
const THREAD_INTERRUPT_PATH: &str = "/v1/threads/:id/interrupt";
const THREAD_METADATA_PATH: &str = "/v1/threads/:id/metadata";
const THREAD_MESSAGES_PATH: &str = "/v1/threads/:id/messages";
const THREAD_MAILBOX_PATH: &str = "/v1/threads/:id/mailbox";
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
        // Register /summaries before /:id to avoid `:id` capturing "summaries"
        .route(THREAD_SUMMARIES_PATH, get(get_thread_summaries))
        .route(THREAD_PATH, get(get_thread).delete(delete_thread))
        .route(THREAD_INTERRUPT_PATH, post(interrupt_thread))
        .route(THREAD_METADATA_PATH, patch(patch_thread_metadata))
        .route(THREAD_MESSAGES_PATH, get(get_thread_messages))
        .route(THREAD_MAILBOX_PATH, get(list_thread_mailbox))
}

/// Build run projection query routes (opt-in, not included in the default public router).
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
) -> Result<Json<Value>, ApiError> {
    let Some(thread) = st
        .read_store
        .load_thread(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
    else {
        return Err(ApiError::ThreadNotFound(id));
    };
    let value = serde_json::to_value(thread).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(sanitize_public_thread_value(value)))
}

async fn get_thread_messages(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<MessageQueryParams>,
) -> Result<Json<Value>, ApiError> {
    let query = parse_message_query(&params);
    let page = st
        .read_store
        .load_messages(&id, &query)
        .await
        .map_err(|e| match e {
            tirea_agentos::contracts::storage::ThreadStoreError::NotFound(_) => {
                ApiError::ThreadNotFound(id)
            }
            other => ApiError::Internal(other.to_string()),
        })?;
    let value = serde_json::to_value(page).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(sanitize_public_message_page_value(value)))
}

fn sanitize_public_thread_value(mut value: Value) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.remove("patches");
        if let Some(state) = object.get_mut("state") {
            strip_public_run_state(state);
        }
        if let Some(messages) = object.get_mut("messages").and_then(Value::as_array_mut) {
            for message in messages {
                strip_public_message_run_metadata(message);
            }
        }
    }
    value
}

fn sanitize_public_message_page_value(mut value: Value) -> Value {
    if let Some(messages) = value.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            strip_public_message_run_metadata(message);
        }
    }
    value
}

fn sanitize_public_mailbox_page_value(mut value: Value, visibility: Option<Visibility>) -> Value {
    if let Some(items) = value.get_mut("items").and_then(Value::as_array_mut) {
        for entry in items {
            strip_public_mailbox_payload_messages(entry, visibility);
        }
    }
    value
}

fn strip_public_run_state(state: &mut Value) {
    if let Some(run_state) = state.get_mut("__run").and_then(Value::as_object_mut) {
        run_state.remove("id");
    }
}

fn strip_public_message_run_metadata(message: &mut Value) {
    if let Some(object) = message.as_object_mut() {
        let mut remove_metadata = false;
        if let Some(metadata) = object.get_mut("metadata").and_then(Value::as_object_mut) {
            metadata.remove("run_id");
            remove_metadata = metadata.is_empty();
        }
        if remove_metadata {
            object.remove("metadata");
        }
    }
}

fn strip_public_mailbox_payload_messages(entry: &mut Value, visibility: Option<Visibility>) {
    let Some(payload) = entry.get_mut("payload").and_then(Value::as_object_mut) else {
        return;
    };
    let Some(messages) = payload.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };

    messages.retain(|message| match visibility {
        Some(Visibility::All) => mailbox_message_visibility(message) != Visibility::Internal,
        Some(Visibility::Internal) => mailbox_message_visibility(message) == Visibility::Internal,
        None => true,
    });

    for message in messages {
        strip_public_message_run_metadata(message);
    }
}

fn mailbox_message_visibility(message: &Value) -> Visibility {
    match message.get("visibility").and_then(Value::as_str) {
        Some("internal") => Visibility::Internal,
        _ => Visibility::All,
    }
}

// ---------------------------------------------------------------------------
// Thread CRUD + summaries
// ---------------------------------------------------------------------------

async fn delete_thread(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let store = require_agent_state_store(&st.os)?;
    store
        .delete(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct PatchMetadataPayload {
    title: Option<String>,
}

async fn patch_thread_metadata(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Json(payload): Json<PatchMetadataPayload>,
) -> Result<Json<Value>, ApiError> {
    let store = require_agent_state_store(&st.os)?;
    let mut thread = store
        .load_thread(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or(ApiError::ThreadNotFound(id))?;

    if let Some(title) = payload.title {
        thread
            .metadata
            .extra
            .insert("title".to_string(), Value::String(title));
    }

    store
        .save(&thread)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok(Json(
        serde_json::to_value(&thread.metadata).unwrap_or_default(),
    ))
}

async fn interrupt_thread(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    let result = st
        .mailbox_service
        .control(&id, crate::service::ControlSignal::Interrupt)
        .await?;

    let superseded_entry_ids: Vec<String> = result
        .superseded_entries
        .iter()
        .map(|entry| entry.entry_id.clone())
        .collect();
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "interrupt_requested",
            "thread_id": id,
            "generation": result.generation.unwrap_or(0),
            "cancelled_run_id": result.cancelled_run_id,
            "superseded_pending_count": result.superseded_entries.len(),
            "superseded_pending_entry_ids": superseded_entry_ids,
        })),
    )
        .into_response())
}

fn default_mailbox_limit() -> usize {
    50
}

#[derive(Debug, Deserialize)]
struct MailboxListParams {
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default = "default_mailbox_limit")]
    limit: usize,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    origin: Option<String>,
    #[serde(default)]
    visibility: Option<String>,
}

fn parse_mailbox_status(raw: &str) -> Option<MailboxEntryStatus> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "queued" => Some(MailboxEntryStatus::Queued),
        "claimed" => Some(MailboxEntryStatus::Claimed),
        "accepted" => Some(MailboxEntryStatus::Accepted),
        "superseded" => Some(MailboxEntryStatus::Superseded),
        "cancelled" => Some(MailboxEntryStatus::Cancelled),
        "dead_letter" | "deadletter" => Some(MailboxEntryStatus::DeadLetter),
        _ => None,
    }
}

fn parse_mailbox_origin(raw: &str) -> Option<Option<MailboxEntryOrigin>> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "external" => Some(Some(MailboxEntryOrigin::External)),
        "internal" => Some(Some(MailboxEntryOrigin::Internal)),
        "none" => Some(None),
        _ => None,
    }
}

fn parse_mailbox_visibility(raw: Option<&str>) -> Option<Visibility> {
    match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("internal") => Some(Visibility::Internal),
        Some("none") => None,
        _ => Some(Visibility::All),
    }
}

async fn list_thread_mailbox(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<MailboxListParams>,
) -> Result<Json<Value>, ApiError> {
    let origin = parse_mailbox_origin(params.origin.as_deref().unwrap_or("external"))
        .unwrap_or(Some(MailboxEntryOrigin::External));
    let visibility = parse_mailbox_visibility(params.visibility.as_deref());
    let query = MailboxQuery {
        mailbox_id: Some(id),
        origin,
        status: params.status.as_deref().and_then(parse_mailbox_status),
        offset: params.offset.unwrap_or(0),
        limit: params.limit.clamp(1, 200),
    };
    let page = st
        .mailbox_store()
        .list_mailbox_entries(&query)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let value = serde_json::to_value(page).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Json(sanitize_public_mailbox_page_value(value, visibility)))
}

#[derive(Debug, Serialize)]
struct ThreadSummary {
    id: String,
    title: Option<String>,
    updated_at: Option<u64>,
    created_at: Option<u64>,
    message_count: usize,
}

async fn get_thread_summaries(
    State(st): State<AppState>,
) -> Result<Json<Vec<ThreadSummary>>, ApiError> {
    let query = ThreadListQuery {
        offset: 0,
        limit: 200,
        resource_id: None,
        parent_thread_id: None,
    };
    let page = st
        .read_store
        .list_paginated(&query)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let mut summaries = Vec::with_capacity(page.items.len());
    for id in &page.items {
        if let Some(head) = st
            .read_store
            .load(id)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?
        {
            // Skip sub-agent threads (they have a parent_thread_id)
            if head.thread.parent_thread_id.is_some() {
                continue;
            }
            let title = head
                .thread
                .metadata
                .extra
                .get("title")
                .and_then(|v| v.as_str())
                .map(String::from);
            summaries.push(ThreadSummary {
                id: id.clone(),
                title,
                updated_at: head.thread.metadata.updated_at,
                created_at: head.thread.metadata.created_at,
                message_count: head.thread.messages.len(),
            });
        }
    }

    // Sort by updated_at descending (None treated as 0 → sorts last)
    summaries.sort_by(|a, b| b.updated_at.unwrap_or(0).cmp(&a.updated_at.unwrap_or(0)));

    Ok(Json(summaries))
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
    #[serde(rename = "terminationCode", alias = "termination_code", default)]
    termination_code: Option<String>,
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

fn parse_run_status(raw: &str) -> Option<RunStatus> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "running" => Some(RunStatus::Running),
        "waiting" => Some(RunStatus::Waiting),
        "done" => Some(RunStatus::Done),
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

fn normalize_termination_code(value: Option<String>) -> Option<String> {
    value.and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_ascii_lowercase())
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
                origin: RunOrigin::default(),
                state: self.state,
                messages: self.messages,
                initial_decisions: self.initial_decisions,
                source_mailbox_entry_id: None,
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
    let prepared = start_http_run(&st.os, resolved, run_request, &agent_id).await?;

    let starter = prepared
        .starter
        .lock()
        .unwrap()
        .take()
        .expect("starter already consumed");
    let ingress_rx = prepared
        .ingress_rx
        .lock()
        .unwrap()
        .take()
        .expect("ingress_rx already consumed");
    let encoder = Identity::<AgentEvent>::default();
    let sse_rx = wire_http_sse_relay(
        starter,
        encoder,
        ingress_rx,
        HttpSseRelayConfig {
            thread_id: prepared.thread_id,
            fanout: None,
            resumable_downstream: false,
            protocol_label: "run-api",
            on_relay_done: move |_sse_tx| async move {},
            error_formatter: |msg| {
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
        let forwarded = try_forward_decisions_to_active_run_by_id(
            &st.os,
            st.read_store.as_ref(),
            &id,
            decisions,
        )
        .await?;

        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "decision_forwarded",
                "run_id": id,
                "thread_id": forwarded.thread_id,
            })),
        )
            .into_response());
    }

    let Some(parent_run) = load_run_record(st.read_store.as_ref(), &id).await? else {
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
        origin: RunOrigin::default(),
        state: payload.state,
        messages: payload.messages,
        initial_decisions: decisions,
        source_mailbox_entry_id: None,
    };

    let (thread_id, _run_id, _entry_id) = start_background_run(
        &st.mailbox_service,
        &agent_id,
        run_request,
        EnqueueOptions::default(),
    )
    .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "continuation_started",
            "parent_run_id": id,
            "thread_id": thread_id,
        })),
    )
        .into_response())
}

async fn cancel_run(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    if try_cancel_active_or_queued_run_by_id(&st.os, st.mailbox_store(), &id)
        .await?
        .is_some()
    {
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "cancel_requested",
                "run_id": id,
            })),
        )
            .into_response());
    }

    Err(
        match check_run_liveness(st.read_store.as_ref(), &id).await? {
            RunLookup::ExistsButInactive => ApiError::BadRequest("run is not active".to_string()),
            RunLookup::NotFound => ApiError::RunNotFound(id),
        },
    )
}

async fn get_run(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<RunRecord>, ApiError> {
    let Some(record) = st
        .read_store
        .load_run(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
    else {
        return Err(ApiError::RunNotFound(id));
    };
    Ok(Json(record))
}

async fn list_runs(
    State(st): State<AppState>,
    Query(params): Query<RunListParams>,
) -> Result<Json<RunPage>, ApiError> {
    let query = RunQuery {
        offset: params.offset.unwrap_or(0),
        limit: params.limit.clamp(1, 200),
        thread_id: params.thread_id,
        parent_run_id: params.parent_run_id,
        status: params.status.as_deref().and_then(parse_run_status),
        termination_code: normalize_termination_code(params.termination_code),
        origin: params.origin.as_deref().and_then(parse_run_origin),
        created_at_from: params.created_at_from,
        created_at_to: params.created_at_to,
        updated_at_from: params.updated_at_from,
        updated_at_to: params.updated_at_to,
    };
    let page = st
        .read_store
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
        assert_eq!(parse_run_status("running"), Some(RunStatus::Running));
        assert_eq!(parse_run_status("waiting"), Some(RunStatus::Waiting));
        assert_eq!(parse_run_status("done"), Some(RunStatus::Done));
        assert_eq!(parse_run_status("unknown"), None);
        assert_eq!(
            normalize_termination_code(Some(" Cancelled ".to_string())),
            Some("cancelled".to_string())
        );

        assert_eq!(parse_run_origin("a2a"), Some(RunOrigin::A2a));
        assert_eq!(parse_run_origin("ag-ui"), Some(RunOrigin::AgUi));
        assert_eq!(parse_run_origin("x"), None);
    }

    #[test]
    fn parse_mailbox_filters_default_to_external_public_view() {
        assert_eq!(
            parse_mailbox_origin("external"),
            Some(Some(MailboxEntryOrigin::External))
        );
        assert_eq!(
            parse_mailbox_origin("internal"),
            Some(Some(MailboxEntryOrigin::Internal))
        );
        assert_eq!(parse_mailbox_origin("none"), Some(None));
        assert_eq!(parse_mailbox_origin("x"), None);

        assert_eq!(parse_mailbox_visibility(None), Some(Visibility::All));
        assert_eq!(
            parse_mailbox_visibility(Some("internal")),
            Some(Visibility::Internal)
        );
        assert_eq!(parse_mailbox_visibility(Some("none")), None);
    }
}
