//! /v1/ai-sdk routes.

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
}

#[derive(Debug, Deserialize)]
struct AiSdkMessage {
    role: String,
    #[serde(default)]
    content: Value,
}

/// Extract text from an AI SDK content value (string or array of parts).
fn extract_text(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => Some(s.clone()),
        Value::Array(arr) => {
            let text: String = arr
                .iter()
                .filter_map(|p| {
                    if p.get("type").and_then(Value::as_str) == Some("text") {
                        p.get("text").and_then(Value::as_str).map(str::to_string)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("");
            Some(text)
        }
        _ => None,
    }
}

fn convert_messages(msgs: Vec<AiSdkMessage>) -> Vec<Message> {
    crate::message_convert::convert_role_content_pairs(
        msgs.into_iter()
            .filter_map(|m| extract_text(&m.content).map(|text| (m.role, text))),
    )
}

/// Reconnect to an active run's event stream.
///
/// Returns 404 if the run is not found or no longer active. Stream
/// resumption requires broadcast channel infrastructure in RunDispatcher
/// which is not yet implemented.
async fn resume_stream(
    State(_st): State<AppState>,
    Path(run_id): Path<String>,
) -> Result<Response, ApiError> {
    // Stream resumption requires RunDispatcher to maintain broadcast channels
    // for active runs. This is a stub that returns a clear error until the
    // broadcast infrastructure is in place.
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
    let messages = convert_messages(payload.messages);
    let (thread_id, messages) =
        crate::run_dispatcher::prepare_run_inputs(payload.thread_id, messages)?;

    let spec = RunSpec {
        thread_id,
        agent_id: payload.agent_id,
        messages,
    };
    let event_rx = st.dispatcher.dispatch(spec).await;
    let encoder = AiSdkEncoder::new();
    let sse_rx = wire_sse_relay(event_rx, encoder, st.config.sse_buffer_size);

    Ok(sse_response(sse_body_stream(sse_rx)))
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn convert_array_content_messages() {
        let msgs = vec![AiSdkMessage {
            role: "user".into(),
            content: json!([
                {"type": "text", "text": "Look at "},
                {"type": "image_url", "url": "https://example.com/img.png"},
                {"type": "text", "text": "this"}
            ]),
        }];
        let converted = convert_messages(msgs);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].text(), "Look at this");
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
}
