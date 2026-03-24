//! /v1/ag-ui routes.

use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;

use awaken_contract::contract::message::Message;

use crate::app::AppState;
use crate::http_run::wire_sse_relay;
use crate::http_sse::{sse_body_stream, sse_response};
use crate::routes::ApiError;
use crate::run_dispatcher::RunSpec;

use super::encoder::AgUiEncoder;
use super::types::Role;

/// Build AG-UI routes.
pub fn ag_ui_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/ag-ui/run", post(ag_ui_run))
        .route("/v1/ag-ui/threads/:id/messages", get(thread_messages))
}

#[derive(Debug, Deserialize)]
struct AgUiRunRequest {
    #[serde(rename = "threadId", alias = "thread_id", default)]
    thread_id: Option<String>,
    #[serde(rename = "agentId", alias = "agent_id", default)]
    agent_id: Option<String>,
    #[serde(default)]
    messages: Vec<AgUiMessage>,
    #[serde(default)]
    #[allow(dead_code)] // deserialized from AG-UI request, reserved for context forwarding
    context: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct AgUiMessage {
    role: String,
    #[serde(default)]
    content: String,
}

fn convert_messages(msgs: Vec<AgUiMessage>) -> Vec<Message> {
    crate::message_convert::convert_role_content_pairs(
        msgs.into_iter().map(|m| (m.role, m.content)),
    )
}

async fn ag_ui_run(
    State(st): State<AppState>,
    Json(payload): Json<AgUiRunRequest>,
) -> Result<Response, ApiError> {
    let messages = convert_messages(payload.messages);
    let (thread_id, messages) =
        crate::run_dispatcher::prepare_run_inputs(payload.thread_id, messages)?;

    let spec = RunSpec {
        thread_id,
        agent_id: payload.agent_id,
        messages,
    };
    let event_rx = st.dispatcher.dispatch(spec).await;
    let encoder = AgUiEncoder::new();
    let sse_rx = wire_sse_relay(event_rx, encoder, st.config.sse_buffer_size);

    Ok(sse_response(sse_body_stream(sse_rx)))
}

#[derive(Debug, Deserialize)]
struct MessageQueryParams {
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    50
}

async fn thread_messages(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<MessageQueryParams>,
) -> Result<Json<Value>, ApiError> {
    let messages = st
        .store
        .load_messages(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .unwrap_or_default();

    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.clamp(1, 200);
    let total = messages.len();

    let encoded: Vec<Value> = messages
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|m| {
            let role = match m.role {
                awaken_contract::contract::message::Role::System => Role::System,
                awaken_contract::contract::message::Role::User => Role::User,
                awaken_contract::contract::message::Role::Assistant => Role::Assistant,
                awaken_contract::contract::message::Role::Tool => Role::Tool,
            };
            serde_json::json!({
                "id": m.id,
                "role": role,
                "content": m.content,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "messages": encoded,
        "total": total,
        "has_more": offset + encoded.len() < total,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_ag_ui_messages() {
        let msgs = vec![
            AgUiMessage {
                role: "user".into(),
                content: "hello".into(),
            },
            AgUiMessage {
                role: "assistant".into(),
                content: "hi".into(),
            },
            AgUiMessage {
                role: "unknown".into(),
                content: "x".into(),
            },
        ];
        let converted = convert_messages(msgs);
        assert_eq!(converted.len(), 2);
        assert_eq!(converted[0].text(), "hello");
        assert_eq!(converted[1].text(), "hi");
    }

    #[test]
    fn convert_empty_messages() {
        assert!(convert_messages(vec![]).is_empty());
    }
}
