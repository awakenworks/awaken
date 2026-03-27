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
use awaken_runtime::RunRequest;

use super::encoder::AgUiEncoder;
use super::types::Role;

/// Build AG-UI routes.
pub fn ag_ui_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/ag-ui/run", post(ag_ui_run))
        .route("/v1/ag-ui/threads/:id/messages", get(thread_messages))
}

#[derive(Debug, Deserialize)]
struct AgUiResumePayload {
    #[serde(rename = "interruptId", alias = "interrupt_id")]
    #[allow(dead_code)]
    interrupt_id: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    payload: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct AgUiRunRequest {
    #[serde(rename = "threadId", alias = "thread_id", default)]
    thread_id: Option<String>,
    #[serde(rename = "agentId", alias = "agent_id", default)]
    agent_id: Option<String>,
    #[serde(default)]
    messages: Vec<AgUiMessage>,
    #[serde(default, alias = "context")]
    state: Option<Value>,
    #[serde(default)]
    #[allow(dead_code)] // deserialized from AG-UI request, reserved for resume handling
    resume: Option<AgUiResumePayload>,
}

#[derive(Debug, Deserialize)]
struct AgUiMessage {
    role: String,
    #[serde(default)]
    content: serde_json::Value,
}

fn convert_messages(msgs: Vec<AgUiMessage>) -> Vec<Message> {
    msgs.into_iter()
        .filter_map(|m| {
            let blocks = parse_ag_ui_content(&m.content)?;
            match m.role.as_str() {
                "user" => Some(Message::user_with_content(blocks)),
                "assistant" => Some(Message::assistant(
                    awaken_contract::contract::content::extract_text(&blocks),
                )),
                "system" => Some(Message::system(
                    awaken_contract::contract::content::extract_text(&blocks),
                )),
                _ => None,
            }
        })
        .collect()
}

fn parse_ag_ui_content(
    content: &serde_json::Value,
) -> Option<Vec<awaken_contract::contract::content::ContentBlock>> {
    use awaken_contract::contract::content::ContentBlock;

    match content {
        serde_json::Value::String(s) => Some(vec![ContentBlock::text(s.as_str())]),
        serde_json::Value::Array(arr) => {
            let blocks: Vec<ContentBlock> = arr
                .iter()
                .filter_map(|v| {
                    let part: super::types::InputContentPart =
                        serde_json::from_value(v.clone()).ok()?;
                    input_part_to_block(part)
                })
                .collect();
            if blocks.is_empty() {
                None
            } else {
                Some(blocks)
            }
        }
        serde_json::Value::Null => None,
        _ => None,
    }
}

fn input_part_to_block(
    part: super::types::InputContentPart,
) -> Option<awaken_contract::contract::content::ContentBlock> {
    use super::types::{InputContentPart, InputContentSource};
    use awaken_contract::contract::content::ContentBlock;

    match part {
        InputContentPart::Text { text } => Some(ContentBlock::text(text)),
        InputContentPart::Image { source, .. } => Some(match source {
            InputContentSource::Data { value, mime_type } => {
                ContentBlock::image_base64(mime_type, value)
            }
            InputContentSource::Url { value, .. } => ContentBlock::image_url(value),
        }),
        InputContentPart::Audio { source, .. } => Some(match source {
            InputContentSource::Data { value, mime_type } => {
                ContentBlock::audio_base64(mime_type, value)
            }
            InputContentSource::Url { value, .. } => ContentBlock::audio_url(value),
        }),
        InputContentPart::Video { source, .. } => Some(match source {
            InputContentSource::Data { value, mime_type } => {
                ContentBlock::video_base64(mime_type, value)
            }
            InputContentSource::Url { value, .. } => ContentBlock::video_url(value),
        }),
        InputContentPart::Document { source, .. } => Some(match source {
            InputContentSource::Data { value, mime_type } => {
                ContentBlock::document_base64(mime_type, value, None)
            }
            InputContentSource::Url { value, .. } => ContentBlock::document_url(value, None),
        }),
    }
}

async fn ag_ui_run(
    State(st): State<AppState>,
    Json(payload): Json<AgUiRunRequest>,
) -> Result<Response, ApiError> {
    let agent_id = payload.agent_id;
    let messages = convert_messages(payload.messages);
    let (thread_id, messages) = crate::request::prepare_run_inputs(payload.thread_id, messages)?;
    let messages = crate::request::inject_frontend_context(messages, payload.state);

    let mut request = RunRequest::new(thread_id, messages);
    if let Some(id) = agent_id {
        request = request.with_agent_id(id);
    }
    let (_result, event_rx) = st
        .mailbox
        .submit(request)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

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
    use serde_json::json;

    #[test]
    fn convert_ag_ui_messages() {
        let msgs = vec![
            AgUiMessage {
                role: "user".into(),
                content: json!("hello"),
            },
            AgUiMessage {
                role: "assistant".into(),
                content: json!("hi"),
            },
            AgUiMessage {
                role: "unknown".into(),
                content: json!("x"),
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

    #[test]
    fn convert_multimodal_user_message() {
        let msgs = vec![AgUiMessage {
            role: "user".into(),
            content: json!([
                {"type": "text", "text": "Look at this"},
                {"type": "image", "source": {"type": "url", "value": "https://example.com/img.png"}}
            ]),
        }];
        let converted = convert_messages(msgs);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].content.len(), 2);
    }
}
