//! /v1/ai-sdk routes.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;

use awaken_contract::contract::content::ContentBlock;
use awaken_contract::contract::message::Message;

use crate::app::AppState;
use crate::http_run::wire_sse_relay_resumable;
use crate::http_sse::{sse_body_stream, sse_response};
use crate::routes::ApiError;
use crate::transport::replay_buffer::EventReplayBuffer;
use awaken_runtime::RunRequest;

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
/// Replays buffered frames after the client's `Last-Event-ID` and then
/// streams any new frames produced by the still-running agent.
/// Returns 404 if the run has no active replay buffer.
async fn resume_stream(
    State(st): State<AppState>,
    Path(run_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let last_event_id: u64 = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let buffer = st
        .replay_buffers
        .lock()
        .get(&run_id)
        .cloned()
        .ok_or_else(|| ApiError::NotFound(format!("no active stream for run {run_id}")))?;

    // Atomically replay + subscribe under a single lock hold.
    // This guarantees no duplicates and no gaps.
    let (replayed, live_rx) = buffer.subscribe_after(last_event_id);

    let replay_stream = futures::stream::iter(replayed.into_iter().map(Ok::<Bytes, Infallible>));
    let live_stream =
        tokio_stream::wrappers::UnboundedReceiverStream::new(live_rx).map(Ok::<Bytes, Infallible>);
    let combined = replay_stream.chain(live_stream);

    Ok(sse_response(combined))
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
    let (thread_id, messages) = crate::request::prepare_run_inputs(payload.thread_id, messages)?;
    let messages = crate::request::inject_frontend_context(messages, payload.state);

    let mut request = RunRequest::new(thread_id, messages);
    if let Some(id) = agent_id {
        request = request.with_agent_id(id);
    }
    let (result, event_rx) = st
        .mailbox
        .submit(request)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let replay_buffer = Arc::new(EventReplayBuffer::new(st.config.replay_buffer_capacity));
    let run_id = result.job_id.clone();

    // Register buffer so resume_stream can find it.
    st.replay_buffers
        .lock()
        .insert(run_id.clone(), Arc::clone(&replay_buffer));

    let encoder = AiSdkEncoder::new();
    let sse_rx =
        wire_sse_relay_resumable(event_rx, encoder, st.config.sse_buffer_size, replay_buffer);

    // Spawn cleanup task: forward frames to client, but keep buffer alive for
    // the full run duration (not tied to client connection). This allows
    // reconnecting clients to use resume_stream even after the original client
    // disconnects.
    let buffers = Arc::clone(&st.replay_buffers);
    let replay_buf_for_cleanup = Arc::clone(st.replay_buffers.lock().get(&run_id).unwrap());
    let cleanup_run_id = run_id;
    let mut sse_rx_forwarded = sse_rx;
    let (final_tx, final_rx) = tokio::sync::mpsc::channel::<Bytes>(st.config.sse_buffer_size);
    tokio::spawn(async move {
        while let Some(frame) = sse_rx_forwarded.recv().await {
            // Best-effort forward; ignore send errors (client gone).
            // Keep consuming so the relay task doesn't stall.
            let _ = final_tx.send(frame).await;
        }
        // Run is done — close subscribers so reconnected clients get EOF,
        // then remove the buffer from registry.
        replay_buf_for_cleanup.close_subscribers();
        buffers.lock().remove(&cleanup_run_id);
    });

    Ok(sse_response(sse_body_stream(final_rx)))
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
