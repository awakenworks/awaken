//! /v1/ai-sdk routes.

use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;

use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::message::Message;

use crate::app::AppState;
use crate::http_run::wire_sse_relay;
use crate::http_sse::{sse_body_stream, sse_response};
use crate::mailbox::RunSpec;
use crate::routes::ApiError;

use super::encoder::AiSdkEncoder;

/// Build AI SDK v6 routes.
pub fn ai_sdk_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/ai-sdk/chat", post(ai_sdk_chat))
        .route("/v1/ai-sdk/streams/:run_id", get(resume_stream))
        .route("/v1/ai-sdk/runs/:run_id/stream", get(resume_stream))
        .route("/v1/ai-sdk/threads/:id/messages", get(thread_messages))
}

#[derive(Debug, Deserialize)]
struct AiSdkChatRequest {
    #[serde(default)]
    messages: Vec<AiSdkMessage>,
    #[serde(rename = "threadId", alias = "thread_id", default)]
    thread_id: Option<String>,
    #[serde(rename = "agentId", alias = "agent_id", default)]
    agent_id: Option<String>,
    #[serde(default)]
    state: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct AiSdkMessage {
    role: String,
    #[serde(default)]
    content: Value,
}

/// Parse a data-URI of the form `data:<media_type>;base64,<data>`.
fn parse_data_uri(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(",")?;
    let media_type = meta.strip_suffix(";base64")?;
    Some((media_type.to_string(), data.to_string()))
}

/// Parse a single content part from the AI SDK array format into a `ContentBlock`.
fn parse_content_part(part: &Value) -> Option<ContentBlock> {
    match part.get("type").and_then(Value::as_str)? {
        "text" => {
            let text = part.get("text").and_then(Value::as_str)?;
            Some(ContentBlock::text(text))
        }
        "image_url" => {
            let url = part
                .get("image_url")
                .and_then(|v| v.get("url"))
                .and_then(Value::as_str)
                // Also support flat `url` field (seen in some clients).
                .or_else(|| part.get("url").and_then(Value::as_str))?;
            if let Some((media_type, data)) = parse_data_uri(url) {
                Some(ContentBlock::image_base64(media_type, data))
            } else {
                Some(ContentBlock::image_url(url))
            }
        }
        _ => None,
    }
}

/// Extract content blocks from an AI SDK content value (string or array of parts).
fn extract_content_blocks(content: &Value) -> Option<Vec<ContentBlock>> {
    match content {
        Value::String(s) => Some(vec![ContentBlock::text(s.as_str())]),
        Value::Array(arr) => {
            let blocks: Vec<ContentBlock> = arr.iter().filter_map(parse_content_part).collect();
            if blocks.is_empty() {
                None
            } else {
                Some(blocks)
            }
        }
        _ => None,
    }
}

fn convert_messages(msgs: Vec<AiSdkMessage>) -> Vec<Message> {
    use awaken_contract::contract::content::extract_text;

    msgs.into_iter()
        .filter_map(|m| {
            let blocks = extract_content_blocks(&m.content)?;
            match m.role.as_str() {
                "user" => Some(Message::user_with_content(blocks)),
                // Assistant and system messages are text-only in practice;
                // extract concatenated text for backward compatibility.
                "assistant" => Some(Message::assistant(extract_text(&blocks))),
                "system" => Some(Message::system(extract_text(&blocks))),
                _ => None,
            }
        })
        .collect()
}

/// Reconnect to an active run's event stream.
///
/// Returns 404 if the run is not found or no longer active. Stream
/// resumption requires broadcast channel infrastructure which is not yet
/// implemented.
async fn resume_stream(
    State(_st): State<AppState>,
    Path(run_id): Path<String>,
) -> Result<Response, ApiError> {
    // Stream resumption requires broadcast channels for active runs.
    // This is a stub that returns a clear error until the broadcast
    // infrastructure is in place.
    Err(ApiError::NotFound(format!(
        "stream resumption not yet available for run {run_id}"
    )))
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
            serde_json::json!({
                "id": m.id,
                "role": match m.role {
                    awaken_contract::contract::message::Role::System => "system",
                    awaken_contract::contract::message::Role::User => "user",
                    awaken_contract::contract::message::Role::Assistant => "assistant",
                    awaken_contract::contract::message::Role::Tool => "tool",
                },
                "content": [{
                    "type": "text",
                    "text": m.content
                }],
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "messages": encoded,
        "total": total,
        "has_more": offset + encoded.len() < total,
    })))
}

async fn ai_sdk_chat(
    State(st): State<AppState>,
    Json(payload): Json<AiSdkChatRequest>,
) -> Result<Response, ApiError> {
    let agent_id = payload.agent_id;
    let messages = convert_messages(payload.messages);
    let (thread_id, messages) =
        crate::mailbox::prepare_run_inputs(payload.thread_id, messages, payload.state)?;

    let spec = RunSpec {
        thread_id,
        agent_id,
        messages,
    };
    let (_result, event_rx) = st
        .mailbox
        .submit(spec)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let encoder = AiSdkEncoder::new();
    let sse_rx = wire_sse_relay(event_rx, encoder, st.config.sse_buffer_size);

    Ok(sse_response(sse_body_stream(sse_rx)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::content::ContentBlock;
    use serde_json::json;

    #[test]
    fn convert_string_content_messages() {
        let msgs = vec![
            AiSdkMessage {
                role: "user".into(),
                content: json!("hello"),
            },
            AiSdkMessage {
                role: "assistant".into(),
                content: json!("hi there"),
            },
        ];
        let converted = convert_messages(msgs);
        assert_eq!(converted.len(), 2);
        assert_eq!(converted[0].text(), "hello");
        assert_eq!(converted[1].text(), "hi there");
    }

    #[test]
    fn convert_array_content_preserves_image_url() {
        let msgs = vec![AiSdkMessage {
            role: "user".into(),
            content: json!([
                {"type": "text", "text": "Look at "},
                {"type": "image_url", "image_url": {"url": "https://example.com/img.png"}},
                {"type": "text", "text": "this"}
            ]),
        }];
        let converted = convert_messages(msgs);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].content.len(), 3);
        assert_eq!(converted[0].content[0], ContentBlock::text("Look at "));
        assert_eq!(
            converted[0].content[1],
            ContentBlock::image_url("https://example.com/img.png")
        );
        assert_eq!(converted[0].content[2], ContentBlock::text("this"));
    }

    #[test]
    fn convert_array_content_parses_base64_image() {
        let msgs = vec![AiSdkMessage {
            role: "user".into(),
            content: json!([
                {"type": "text", "text": "Describe"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBOR"}}
            ]),
        }];
        let converted = convert_messages(msgs);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].content.len(), 2);
        assert_eq!(
            converted[0].content[1],
            ContentBlock::image_base64("image/png", "iVBOR")
        );
    }

    #[test]
    fn convert_array_content_flat_url_fallback() {
        // Some clients send {"type": "image_url", "url": "..."} without nesting.
        let msgs = vec![AiSdkMessage {
            role: "user".into(),
            content: json!([
                {"type": "text", "text": "Look"},
                {"type": "image_url", "url": "https://example.com/img.png"}
            ]),
        }];
        let converted = convert_messages(msgs);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].content.len(), 2);
        assert_eq!(
            converted[0].content[1],
            ContentBlock::image_url("https://example.com/img.png")
        );
    }

    #[test]
    fn convert_skips_unknown_roles() {
        let msgs = vec![AiSdkMessage {
            role: "function".into(),
            content: json!("result"),
        }];
        let converted = convert_messages(msgs);
        assert!(converted.is_empty());
    }

    #[test]
    fn convert_system_messages() {
        let msgs = vec![AiSdkMessage {
            role: "system".into(),
            content: json!("You are a helpful assistant"),
        }];
        let converted = convert_messages(msgs);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].text(), "You are a helpful assistant");
    }

    #[test]
    fn convert_empty_messages() {
        let converted = convert_messages(vec![]);
        assert!(converted.is_empty());
    }

    #[test]
    fn parse_data_uri_valid() {
        let (mime, data) = parse_data_uri("data:image/jpeg;base64,/9j/4AA").unwrap();
        assert_eq!(mime, "image/jpeg");
        assert_eq!(data, "/9j/4AA");
    }

    #[test]
    fn parse_data_uri_invalid() {
        assert!(parse_data_uri("https://example.com/img.png").is_none());
        assert!(parse_data_uri("data:image/png,raw-data").is_none());
    }
}
