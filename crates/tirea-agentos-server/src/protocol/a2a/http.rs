use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, ETAG, IF_NONE_MATCH};
use axum::http::StatusCode;
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tirea_agentos::contracts::thread::Message;
use tirea_agentos::contracts::{RunRequest, ToolCallDecision};
use tirea_agentos::orchestrator::AgentOsRunError;
use tirea_contract::{AgentEvent, Identity};

use crate::run_service::{global_run_service, wrap_with_run_tracking};
use crate::service::{
    prepare_http_run, remove_active_run, try_cancel_active_run_by_id,
    try_forward_decisions_to_active_run_by_id, ApiError, AppState,
};
use crate::transport::http_run::wire_http_sse_relay;

const WELL_KNOWN_AGENT_CARD_PATH: &str = "/.well-known/agent-card.json";
const WELL_KNOWN_CACHE_CONTROL: &str = "public, max-age=30, must-revalidate";
const AGENTS_PATH: &str = "/agents";
const AGENT_CARD_PATH: &str = "/agents/:agent_id/agent-card";
const MESSAGE_SEND_PATH: &str = "/agents/:agent_id/:action";
const TASK_PATH: &str = "/agents/:agent_id/tasks/:task_action";

/// Build top-level well-known A2A discovery route.
pub fn well_known_routes() -> Router<AppState> {
    Router::new().route(WELL_KNOWN_AGENT_CARD_PATH, get(well_known_agent_card))
}

/// Build A2A-compatible HTTP routes.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(AGENTS_PATH, get(list_agents))
        .route(AGENT_CARD_PATH, get(get_agent_card))
        .route(MESSAGE_SEND_PATH, post(message_send))
        .route(TASK_PATH, get(get_task).post(cancel_task))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct A2aGatewayCard {
    name: String,
    description: String,
    version: String,
    url: String,
    default_input_modes: Vec<String>,
    default_output_modes: Vec<String>,
    capabilities: Value,
}

fn build_gateway_card(agent_ids: &[String]) -> A2aGatewayCard {
    let (name, description, url) = if let [single_agent_id] = agent_ids {
        (
            format!("tirea-agent-{single_agent_id}"),
            format!("A2A discovery card for Tirea agent '{single_agent_id}'"),
            format!("/v1/a2a/agents/{single_agent_id}/message:send"),
        )
    } else {
        (
            "tirea-a2a-gateway".to_string(),
            format!(
                "A2A discovery card for Tirea multi-agent gateway ({} agents)",
                agent_ids.len()
            ),
            "/v1/a2a/agents".to_string(),
        )
    };

    A2aGatewayCard {
        name,
        description,
        version: "1.0".to_string(),
        url,
        default_input_modes: vec!["application/json".to_string()],
        default_output_modes: vec!["application/json".to_string()],
        capabilities: json!({
            "taskManagement": true,
            "streaming": true,
            "agentDiscovery": true,
            "agentCount": agent_ids.len(),
            "agents": agent_ids
        }),
    }
}

fn fnv1a64(data: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn build_well_known_etag(agent_ids: &[String]) -> String {
    let canonical = format!("v1|{}", agent_ids.join("\u{001f}"));
    format!("W/\"a2a-agents-{:016x}\"", fnv1a64(canonical.as_bytes()))
}

fn if_none_match_matches(headers: &HeaderMap, etag: &str) -> bool {
    let Some(raw) = headers.get(IF_NONE_MATCH) else {
        return false;
    };
    let Ok(raw) = raw.to_str() else {
        return false;
    };
    raw.split(',')
        .map(str::trim)
        .any(|candidate| candidate == "*" || candidate == etag)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct A2aAgentEntry {
    agent_id: String,
    agent_card_url: String,
    message_send_url: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct A2aAgentCard {
    name: String,
    description: String,
    version: String,
    url: String,
    default_input_modes: Vec<String>,
    default_output_modes: Vec<String>,
    capabilities: Value,
}

fn build_agent_card(agent_id: &str) -> A2aAgentCard {
    A2aAgentCard {
        name: format!("tirea-agent-{agent_id}"),
        description: format!("A2A card for Tirea agent '{agent_id}'"),
        version: "1.0".to_string(),
        url: format!("/v1/a2a/agents/{agent_id}/message:send"),
        default_input_modes: vec!["application/json".to_string()],
        default_output_modes: vec!["application/json".to_string()],
        capabilities: json!({
            "taskManagement": true,
            "streaming": true
        }),
    }
}

async fn well_known_agent_card(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let agent_ids = st.os.agent_ids();
    let etag = build_well_known_etag(&agent_ids);

    if if_none_match_matches(&headers, &etag) {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        response.headers_mut().insert(
            CACHE_CONTROL,
            HeaderValue::from_static(WELL_KNOWN_CACHE_CONTROL),
        );
        if let Ok(value) = HeaderValue::from_str(&etag) {
            response.headers_mut().insert(ETAG, value);
        }
        return response;
    }

    let mut response = Json(build_gateway_card(&agent_ids)).into_response();
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static(WELL_KNOWN_CACHE_CONTROL),
    );
    if let Ok(value) = HeaderValue::from_str(&etag) {
        response.headers_mut().insert(ETAG, value);
    }
    response
}

async fn list_agents(State(st): State<AppState>) -> Json<Vec<A2aAgentEntry>> {
    let entries = st
        .os
        .agent_ids()
        .into_iter()
        .map(|agent_id| A2aAgentEntry {
            agent_card_url: format!("/v1/a2a/agents/{agent_id}/agent-card"),
            message_send_url: format!("/v1/a2a/agents/{agent_id}/message:send"),
            agent_id,
        })
        .collect();
    Json(entries)
}

async fn get_agent_card(
    State(st): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<A2aAgentCard>, ApiError> {
    st.os
        .validate_agent(&agent_id)
        .map_err(|_| ApiError::AgentNotFound(agent_id.clone()))?;
    Ok(Json(build_agent_card(&agent_id)))
}

#[derive(Debug, Deserialize)]
struct A2aMessage {
    #[serde(default)]
    role: Option<String>,
    content: String,
}

#[derive(Debug, Deserialize)]
struct A2aMessageSendPayload {
    #[serde(rename = "contextId", alias = "context_id", default)]
    context_id: Option<String>,
    #[serde(rename = "taskId", alias = "task_id", default)]
    task_id: Option<String>,
    #[serde(default)]
    message: Option<A2aMessage>,
    #[serde(default)]
    input: Option<String>,
    #[serde(default)]
    decisions: Vec<ToolCallDecision>,
}

impl A2aMessageSendPayload {
    fn normalize_id(value: Option<String>) -> Option<String> {
        value.and_then(|raw| {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
    }

    fn to_messages(&self) -> Vec<Message> {
        let mut out = Vec::new();
        if let Some(input) = self.input.as_deref() {
            let trimmed = input.trim();
            if !trimmed.is_empty() {
                out.push(Message::user(trimmed));
            }
        }
        if let Some(message) = self.message.as_ref() {
            let content = message.content.trim();
            if !content.is_empty() {
                let mapped = match message
                    .role
                    .as_deref()
                    .map(str::trim)
                    .map(str::to_ascii_lowercase)
                {
                    Some(role) if role == "assistant" => Message::assistant(content),
                    Some(role) if role == "system" => Message::system(content),
                    _ => Message::user(content),
                };
                out.push(mapped);
            }
        }
        out
    }
}

async fn run_exists(run_id: &str) -> Result<bool, ApiError> {
    let Some(service) = global_run_service() else {
        return Err(ApiError::Internal(
            "run service not initialized".to_string(),
        ));
    };
    Ok(service
        .get_run(run_id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .is_some())
}

async fn resolve_thread_id_from_run(run_id: &str) -> Result<Option<String>, ApiError> {
    let Some(service) = global_run_service() else {
        return Err(ApiError::Internal(
            "run service not initialized".to_string(),
        ));
    };
    service
        .resolve_thread_id(run_id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))
}

async fn start_background_run(
    st: &AppState,
    agent_id: &str,
    run_request: RunRequest,
) -> Result<(String, String), ApiError> {
    let resolved = st.os.resolve(agent_id).map_err(AgentOsRunError::from)?;
    let prepared = prepare_http_run(&st.os, resolved, run_request, "a2a", agent_id).await?;
    let run_id = prepared.run_id.clone();
    let thread_id = prepared.thread_id.clone();
    let active_key = prepared.active_key.clone();
    let thread_for_session = thread_id.clone();

    let encoder = wrap_with_run_tracking(
        Identity::<AgentEvent>::default(),
        run_id.clone(),
        thread_id.clone(),
        "a2a",
    );
    let mut sse_rx = wire_http_sse_relay(
        prepared.starter,
        encoder,
        prepared.ingress_rx,
        thread_for_session,
        None,
        false,
        "a2a",
        move |_sse_tx| async move {
            remove_active_run(&active_key).await;
        },
        |msg| {
            let payload = serde_json::to_string(&json!({
                "type": "error",
                "message": msg,
                "code": "RELAY_ERROR",
            }))
            .unwrap_or_else(|_| {
                "{\"type\":\"error\",\"message\":\"relay error\",\"code\":\"RELAY_ERROR\"}"
                    .to_string()
            });
            Bytes::from(format!("data: {payload}\n\n"))
        },
    );
    tokio::spawn(async move { while sse_rx.recv().await.is_some() {} });

    Ok((thread_id, run_id))
}

async fn message_send(
    State(st): State<AppState>,
    Path((agent_id, action)): Path<(String, String)>,
    Json(payload): Json<A2aMessageSendPayload>,
) -> Result<Response, ApiError> {
    if action != "message:send" {
        return Err(ApiError::BadRequest(
            "unsupported A2A action; expected 'message:send'".to_string(),
        ));
    }

    st.os
        .validate_agent(&agent_id)
        .map_err(|_| ApiError::AgentNotFound(agent_id.clone()))?;

    let task_id = A2aMessageSendPayload::normalize_id(payload.task_id.clone());
    let context_id = A2aMessageSendPayload::normalize_id(payload.context_id.clone());
    let messages = payload.to_messages();
    let decisions = payload.decisions;

    if task_id.is_none() && context_id.is_none() && messages.is_empty() && decisions.is_empty() {
        return Err(ApiError::BadRequest(
            "message send payload must include input/message/decisions/context/task".to_string(),
        ));
    }

    if messages.is_empty() && !decisions.is_empty() {
        let Some(task_id) = task_id.as_deref() else {
            return Err(ApiError::BadRequest(
                "task_id is required when forwarding decisions only".to_string(),
            ));
        };

        if try_forward_decisions_to_active_run_by_id(task_id, decisions).await {
            let thread_id = resolve_thread_id_from_run(task_id).await?;
            return Ok((
                StatusCode::ACCEPTED,
                Json(json!({
                    "contextId": thread_id,
                    "taskId": task_id,
                    "status": "decision_forwarded",
                })),
            )
                .into_response());
        }

        if run_exists(task_id).await? {
            return Err(ApiError::BadRequest("task is not active".to_string()));
        }
        return Err(ApiError::RunNotFound(task_id.to_string()));
    }

    let thread_id = if let Some(context_id) = context_id {
        Some(context_id)
    } else if let Some(task_id) = task_id.as_deref() {
        resolve_thread_id_from_run(task_id).await?
    } else {
        None
    };

    if task_id.is_some() && thread_id.is_none() {
        return Err(ApiError::RunNotFound(task_id.unwrap_or_default()));
    }

    let run_request = RunRequest {
        agent_id: agent_id.clone(),
        thread_id,
        run_id: None,
        parent_run_id: task_id,
        parent_thread_id: None,
        resource_id: None,
        state: None,
        messages,
        initial_decisions: decisions,
    };

    let (context_id, task_id) = start_background_run(&st, &agent_id, run_request).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "contextId": context_id,
            "taskId": task_id,
            "status": "submitted",
        })),
    )
        .into_response())
}

async fn get_task(
    State(st): State<AppState>,
    Path((agent_id, task_action)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    st.os
        .validate_agent(&agent_id)
        .map_err(|_| ApiError::AgentNotFound(agent_id.clone()))?;
    if task_action.ends_with(":cancel") {
        return Err(ApiError::BadRequest(
            "use POST for task cancellation".to_string(),
        ));
    }
    let task_id = task_action.trim().to_string();
    if task_id.is_empty() {
        return Err(ApiError::BadRequest(
            "task_id is required in task path".to_string(),
        ));
    }
    let Some(service) = global_run_service() else {
        return Err(ApiError::Internal(
            "run service not initialized".to_string(),
        ));
    };
    let Some(record) = service
        .get_run(&task_id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
    else {
        return Err(ApiError::RunNotFound(task_id));
    };

    Ok(Json(json!({
        "taskId": record.run_id,
        "contextId": record.thread_id,
        "status": record.status,
        "origin": record.origin,
        "terminationCode": record.termination_code,
        "terminationDetail": record.termination_detail,
        "createdAt": record.created_at,
        "updatedAt": record.updated_at,
    }))
    .into_response())
}

async fn cancel_task(
    State(st): State<AppState>,
    Path((agent_id, task_action)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    st.os
        .validate_agent(&agent_id)
        .map_err(|_| ApiError::AgentNotFound(agent_id.clone()))?;

    let Some(task_id) = task_action.strip_suffix(":cancel") else {
        return Err(ApiError::BadRequest(
            "task cancel path must end with ':cancel'".to_string(),
        ));
    };
    let task_id = task_id.trim().to_string();
    if task_id.is_empty() {
        return Err(ApiError::BadRequest(
            "task_id is required in cancel path".to_string(),
        ));
    }

    if try_cancel_active_run_by_id(&task_id).await {
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "taskId": task_id,
                "status": "cancel_requested",
            })),
        )
            .into_response());
    }

    if run_exists(&task_id).await? {
        return Err(ApiError::BadRequest("task is not active".to_string()));
    }

    Err(ApiError::RunNotFound(task_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_known_etag_is_stable_and_changes_with_agent_set() {
        let etag_a = build_well_known_etag(&["alpha".to_string()]);
        let etag_b = build_well_known_etag(&["alpha".to_string()]);
        let etag_c = build_well_known_etag(&["alpha".to_string(), "beta".to_string()]);
        assert_eq!(etag_a, etag_b);
        assert_ne!(etag_a, etag_c);
    }

    #[test]
    fn if_none_match_matches_star_and_csv_values() {
        let etag = "W/\"a2a-agents-deadbeef\"";

        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, HeaderValue::from_static("*"));
        assert!(if_none_match_matches(&headers, etag));

        let mut headers = HeaderMap::new();
        headers.insert(
            IF_NONE_MATCH,
            HeaderValue::from_str(&format!("\"other\", {etag}")).expect("valid header"),
        );
        assert!(if_none_match_matches(&headers, etag));

        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, HeaderValue::from_static("\"other\""));
        assert!(!if_none_match_matches(&headers, etag));
    }
}
