//! A2A HTTP endpoints and agent card.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use awaken_contract::contract::message::Message;

use crate::app::AppState;
use crate::routes::ApiError;
use awaken_runtime::RunRequest;

/// Build A2A routes.
pub fn a2a_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/a2a/tasks/send", post(a2a_task_send))
        .route("/v1/a2a/tasks/:task_id", get(a2a_task_status))
        .route("/v1/a2a/tasks/:task_id/cancel", post(a2a_task_cancel))
        .route("/v1/a2a/.well-known/agent", get(a2a_agent_card))
        .route("/v1/a2a/agents", get(a2a_list_agents))
        .route(
            "/v1/a2a/agents/:agent_id/agent-card",
            get(a2a_agent_card_by_id),
        )
        .route(
            "/v1/a2a/agents/:agent_id/message:send",
            post(a2a_message_send),
        )
        .route(
            "/v1/a2a/agents/:agent_id/tasks/:task_action",
            get(a2a_task_action_get).post(a2a_task_action_post),
        )
}

/// A2A Agent Card — describes agent capabilities for discovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCard {
    /// Agent identifier used for A2A discovery.
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub url: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<AgentCapabilities>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<AgentSkill>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCapabilities {
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub push_notifications: bool,
    #[serde(default)]
    pub state_transition_history: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSkill {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// A2A task send request.
#[derive(Debug, Deserialize)]
struct A2aTaskSendRequest {
    #[serde(rename = "taskId", alias = "task_id", default)]
    task_id: Option<String>,
    #[serde(rename = "agentId", alias = "agent_id", default)]
    agent_id: Option<String>,
    #[serde(default)]
    message: Option<A2aMessage>,
    #[serde(default)]
    #[allow(dead_code)] // deserialized from A2A request, reserved for task metadata forwarding
    metadata: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct A2aMessage {
    role: String,
    #[serde(default)]
    parts: Vec<A2aMessagePart>,
}

#[derive(Debug, Deserialize)]
struct A2aMessagePart {
    #[serde(rename = "type", default)]
    part_type: String,
    #[serde(default)]
    text: Option<String>,
}

/// A2A task send response.
#[derive(Debug, Serialize)]
struct A2aTaskSendResponse {
    #[serde(rename = "taskId")]
    task_id: String,
    status: A2aTaskStatus,
}

#[derive(Debug, Serialize)]
struct A2aTaskStatus {
    state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<Value>,
}

async fn a2a_task_send(
    State(st): State<AppState>,
    Json(payload): Json<A2aTaskSendRequest>,
) -> Result<Response, ApiError> {
    let task_id = payload
        .task_id
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());

    let messages = match payload.message {
        Some(msg) => {
            let text: String = msg
                .parts
                .iter()
                .filter(|p| p.part_type == "text")
                .filter_map(|p| p.text.as_deref())
                .collect::<Vec<_>>()
                .join("");
            if text.is_empty() {
                return Err(ApiError::BadRequest("message text is empty".to_string()));
            }
            match msg.role.as_str() {
                "user" => vec![Message::user(text)],
                _ => vec![Message::user(text)],
            }
        }
        None => {
            return Err(ApiError::BadRequest("message is required".to_string()));
        }
    };

    let mut request = RunRequest::new(task_id.clone(), messages);
    if let Some(id) = payload.agent_id {
        request = request.with_agent_id(id);
    }
    // Fire-and-forget: submit the run in the background.
    let _result = st
        .mailbox
        .submit_background(request)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    Ok((
        StatusCode::OK,
        Json(A2aTaskSendResponse {
            task_id,
            status: A2aTaskStatus {
                state: "submitted".to_string(),
                message: None,
            },
        }),
    )
        .into_response())
}

async fn a2a_message_send(
    State(st): State<AppState>,
    Path(agent_id): Path<String>,
    Json(mut payload): Json<A2aTaskSendRequest>,
) -> Result<Response, ApiError> {
    payload.agent_id = Some(agent_id);
    a2a_task_send(State(st), Json(payload)).await
}

/// GET /v1/a2a/tasks/:task_id — query run status from RunStore.
///
/// In the A2A model, task_id maps to the run's thread_id (set during `a2a_task_send`).
async fn a2a_task_status(
    State(st): State<AppState>,
    Path(task_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    // A2A tasks use task_id as thread_id, so query latest run for that thread.
    let record = st
        .store
        .latest_run(&task_id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or(ApiError::NotFound(format!("task not found: {task_id}")))?;

    let state = match record.status {
        awaken_contract::contract::lifecycle::RunStatus::Running => "working",
        awaken_contract::contract::lifecycle::RunStatus::Waiting => "input-required",
        awaken_contract::contract::lifecycle::RunStatus::Done => "completed",
    };

    Ok(Json(serde_json::json!({
        "taskId": task_id,
        "status": {
            "state": state,
        },
        "runId": record.run_id,
        "agentId": record.agent_id,
        "createdAt": record.created_at,
        "updatedAt": record.updated_at,
    })))
}

/// POST /v1/a2a/tasks/:task_id/cancel — cancel via runtime.
async fn a2a_task_cancel(
    State(st): State<AppState>,
    Path(task_id): Path<String>,
) -> Result<Response, ApiError> {
    let cancelled = st
        .mailbox
        .cancel(&task_id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    if cancelled {
        return Ok((
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "taskId": task_id,
                "status": { "state": "canceled" },
            })),
        )
            .into_response());
    }

    Err(ApiError::NotFound(format!("task not found: {task_id}")))
}

async fn a2a_agent_card(State(_st): State<AppState>) -> Json<AgentCard> {
    Json(AgentCard {
        id: "awaken-agent".to_string(),
        name: "awaken-agent".to_string(),
        description: Some("Awaken AI Agent".to_string()),
        url: String::new(),
        version: "0.1.0".to_string(),
        capabilities: Some(AgentCapabilities {
            streaming: true,
            push_notifications: false,
            state_transition_history: false,
        }),
        skills: Vec::new(),
    })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct A2aAgentListEntry {
    agent_id: String,
    name: String,
}

async fn a2a_list_agents(State(st): State<AppState>) -> Json<Vec<A2aAgentListEntry>> {
    let mut ids = st.resolver.agent_ids();
    if ids.is_empty() {
        ids.push("default".to_string());
    }
    ids.sort();
    ids.dedup();
    Json(
        ids.into_iter()
            .map(|id| A2aAgentListEntry {
                name: id.clone(),
                agent_id: id,
            })
            .collect(),
    )
}

async fn a2a_agent_card_by_id(
    State(st): State<AppState>,
    Path(agent_id): Path<String>,
) -> Result<Json<AgentCard>, ApiError> {
    if st.resolver.resolve(&agent_id).is_err() {
        return Err(ApiError::NotFound(format!("agent not found: {agent_id}")));
    }
    Ok(Json(AgentCard {
        id: agent_id.clone(),
        name: agent_id.clone(),
        description: Some(format!("Awaken AI Agent '{agent_id}'")),
        url: format!("/v1/a2a/agents/{agent_id}/message:send"),
        version: "0.1.0".to_string(),
        capabilities: Some(AgentCapabilities {
            streaming: true,
            push_notifications: false,
            state_transition_history: false,
        }),
        skills: Vec::new(),
    }))
}

async fn a2a_task_action_get(
    State(st): State<AppState>,
    Path((_agent_id, task_action)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    a2a_task_status(State(st), Path(task_action))
        .await
        .map(|json| json.into_response())
}

async fn a2a_task_action_post(
    State(st): State<AppState>,
    Path((_agent_id, task_action)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    a2a_task_cancel(State(st), Path(task_action)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn agent_card_serde_roundtrip() {
        let card = AgentCard {
            id: "test-agent".into(),
            name: "test-agent".into(),
            description: Some("A test agent".into()),
            url: "http://localhost:3000".into(),
            version: "1.0.0".into(),
            capabilities: Some(AgentCapabilities {
                streaming: true,
                push_notifications: false,
                state_transition_history: true,
            }),
            skills: vec![AgentSkill {
                id: "s1".into(),
                name: "search".into(),
                description: Some("Web search".into()),
                tags: vec!["web".into()],
            }],
        };
        let json = serde_json::to_string(&card).unwrap();
        let parsed: AgentCard = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "test-agent");
        assert_eq!(parsed.name, "test-agent");
        assert_eq!(parsed.skills.len(), 1);
    }

    #[test]
    fn agent_card_empty_skills_omitted() {
        let card = AgentCard {
            id: String::new(),
            name: "minimal".into(),
            description: None,
            url: String::new(),
            version: "0.1.0".into(),
            capabilities: None,
            skills: Vec::new(),
        };
        let json = serde_json::to_string(&card).unwrap();
        assert!(!json.contains("skills"));
        assert!(!json.contains("description"));
        assert!(!json.contains("capabilities"));
    }

    #[test]
    fn a2a_task_send_request_deserialize() {
        let json = json!({
            "taskId": "task-1",
            "agentId": "agent-1",
            "message": {
                "role": "user",
                "parts": [
                    {"type": "text", "text": "hello"}
                ]
            }
        });
        let req: A2aTaskSendRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.task_id.as_deref(), Some("task-1"));
        assert_eq!(req.agent_id.as_deref(), Some("agent-1"));
        assert!(req.message.is_some());
    }

    #[test]
    fn a2a_task_status_serialization() {
        let resp = A2aTaskSendResponse {
            task_id: "task-1".into(),
            status: A2aTaskStatus {
                state: "completed".into(),
                message: Some(json!({"text": "done"})),
            },
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"taskId\":\"task-1\""));
        assert!(json.contains("\"state\":\"completed\""));
    }
}
