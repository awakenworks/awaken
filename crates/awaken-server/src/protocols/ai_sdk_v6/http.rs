//! /v1/ai-sdk routes.

use axum::extract::State;
use axum::response::Response;
use axum::routing::post;
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
    Router::new().route("/v1/ai-sdk/chat", post(ai_sdk_chat))
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
    let event_rx = st.dispatcher.dispatch(spec);
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
