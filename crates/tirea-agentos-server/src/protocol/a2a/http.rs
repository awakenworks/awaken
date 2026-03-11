use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, ETAG, IF_NONE_MATCH};
use axum::http::StatusCode;
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tirea_agentos::contracts::thread::Message;
use tirea_agentos::contracts::{RunOrigin, RunRequest, ToolCallDecision};

use crate::service::{
    check_run_liveness, load_background_task, normalize_optional_id, require_mailbox_store,
    resolve_thread_id_from_run, start_background_run, try_cancel_active_or_queued_run_by_id,
    try_forward_decisions_to_active_run_by_id, ApiError, AppState, BackgroundTaskLookup, RunLookup,
};

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

fn sorted_agent_ids(agent_ids: &[String]) -> Vec<String> {
    let mut ids = agent_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn build_gateway_card(agent_ids: &[String]) -> A2aGatewayCard {
    let normalized_agent_ids = sorted_agent_ids(agent_ids);
    let (name, description, url) = match normalized_agent_ids.as_slice() {
        [single_agent_id] => (
            format!("tirea-agent-{single_agent_id}"),
            format!("A2A discovery card for Tirea agent '{single_agent_id}'"),
            format!("/v1/a2a/agents/{single_agent_id}/message:send"),
        ),
        _ => (
            "tirea-a2a-gateway".to_string(),
            format!(
                "A2A discovery card for Tirea multi-agent gateway ({} agents)",
                normalized_agent_ids.len()
            ),
            "/v1/a2a/agents".to_string(),
        ),
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
            "agentCount": normalized_agent_ids.len(),
            "agents": normalized_agent_ids,
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
    let canonical = format!("v1|{}", sorted_agent_ids(agent_ids).join("\u{001f}"));
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
    let mut agent_ids = st.os.agent_ids();
    agent_ids.sort_unstable();
    agent_ids.dedup();
    let entries = agent_ids
        .into_iter()
        .map(|agent_id| A2aAgentEntry {
            agent_card_url: format!("/v1/a2a/agents/{agent_id}/agent-card"),
            message_send_url: format!("/v1/a2a/agents/{agent_id}/message:send"),
            agent_id,
        })
        .collect::<Vec<_>>();
    Json(entries)
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

    let task_id = normalize_optional_id(payload.task_id.clone());
    let context_id = normalize_optional_id(payload.context_id.clone());
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

        let forwarded = try_forward_decisions_to_active_run_by_id(
            &st.os,
            st.read_store.as_ref(),
            task_id,
            decisions,
        )
        .await?;
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "contextId": forwarded.thread_id,
                "taskId": task_id,
                "status": "decision_forwarded",
            })),
        )
            .into_response());
    }

    let thread_id = if let Some(context_id) = context_id {
        Some(context_id)
    } else if let Some(task_id) = task_id.as_deref() {
        resolve_thread_id_from_run(st.read_store.as_ref(), task_id).await?
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
        origin: RunOrigin::A2a,
        state: None,
        messages,
        initial_decisions: decisions,
        source_mailbox_entry_id: None,
    };

    let mailbox_store = require_mailbox_store(&st)?;
    let (context_id, _run_id, task_id) =
        start_background_run(&st.os, &mailbox_store, &agent_id, run_request, "a2a").await?;
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
    let mailbox_store = require_mailbox_store(&st)?;
    let Some(task) =
        load_background_task(st.read_store.as_ref(), mailbox_store.as_ref(), &task_id).await?
    else {
        return Err(ApiError::RunNotFound(task_id));
    };

    Ok(match task {
        BackgroundTaskLookup::Run(record) => Json(json!({
            "taskId": task_id,
            "contextId": record.thread_id,
            "status": record.status,
            "origin": record.origin,
            "terminationCode": record.termination_code,
            "terminationDetail": record.termination_detail,
            "createdAt": record.created_at,
            "updatedAt": record.updated_at,
        })),
        BackgroundTaskLookup::Mailbox(entry) => Json(json!({
            "taskId": task_id,
            "contextId": entry.mailbox_id,
            "status": entry.status,
            "createdAt": entry.created_at,
            "updatedAt": entry.updated_at,
        })),
    }
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

    let mailbox_store = require_mailbox_store(&st)?;
    if try_cancel_active_or_queued_run_by_id(&st.os, &mailbox_store, &task_id)
        .await?
        .is_some()
    {
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "taskId": task_id,
                "status": "cancel_requested",
            })),
        )
            .into_response());
    }

    Err(
        match load_background_task(st.read_store.as_ref(), mailbox_store.as_ref(), &task_id).await?
        {
            Some(_) => ApiError::BadRequest("task is not active".to_string()),
            None => match check_run_liveness(st.read_store.as_ref(), &task_id).await? {
                RunLookup::ExistsButInactive => {
                    ApiError::BadRequest("task is not active".to_string())
                }
                RunLookup::NotFound => ApiError::RunNotFound(task_id),
            },
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_known_etag_is_stable_and_changes_with_agent_set() {
        let etag_a = build_well_known_etag(&["alpha".to_string()]);
        let etag_b = build_well_known_etag(&["alpha".to_string()]);
        let etag_c = build_well_known_etag(&["beta".to_string(), "alpha".to_string()]);
        let etag_d = build_well_known_etag(&["alpha".to_string(), "beta".to_string()]);
        assert_eq!(etag_a, etag_b);
        assert_eq!(etag_c, etag_d);
        assert_ne!(
            etag_a, etag_d,
            "adding an extra agent id should change ETag"
        );
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
