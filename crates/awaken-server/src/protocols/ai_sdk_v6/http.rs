//! /v1/ai-sdk HTTP routes and SSE wiring.
//!
//! This module is responsible only for routing, SSE stream management, and
//! replay-buffer lifecycle. All AI SDK–specific request parsing (message
//! conversion, deduplication, decision extraction) lives in [`super::request`].

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use futures::StreamExt;
use serde_json::{Value, json};

use awaken_contract::contract::content::{ContentBlock, extract_text};
use awaken_contract::contract::message::{Message, Role, ToolCall};

use crate::app::AppState;
use crate::http_run::wire_sse_relay_resumable;
use crate::http_sse::{sse_body_stream, sse_response};
use crate::routes::ApiError;
use crate::transport::replay_buffer::EventReplayBuffer;
use awaken_runtime::RunRequest;

use super::encoder::AiSdkEncoder;
use super::request::{AiSdkChatRequest, ProcessedRequest};

/// AI SDK v6 header required by `DefaultChatTransport` to identify the stream format.
const AI_SDK_STREAM_HEADER: &str = "x-vercel-ai-ui-message-stream";
const AI_SDK_STREAM_VERSION: &str = "v1";

/// Wrap [`sse_response`] with the AI SDK–specific stream protocol header.
fn ai_sdk_sse_response<S>(stream: S) -> Response
where
    S: futures::Stream<Item = Result<Bytes, Infallible>> + Send + 'static,
{
    let mut response = sse_response(stream);
    response.headers_mut().insert(
        axum::http::HeaderName::from_static(AI_SDK_STREAM_HEADER),
        axum::http::HeaderValue::from_static(AI_SDK_STREAM_VERSION),
    );
    response
}

/// Build AI SDK v6 routes.
///
/// The resume route `{api}/{chatId}/stream` matches the AI SDK's
/// `HttpChatTransport.reconnectToStream()` URL pattern, where `chatId`
/// maps to awaken's `threadId`.
pub fn ai_sdk_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/ai-sdk/chat", post(ai_sdk_chat))
        .route(
            "/v1/ai-sdk/threads/:thread_id/runs",
            post(ai_sdk_chat_threaded),
        )
        .route(
            "/v1/ai-sdk/agents/:agent_id/runs",
            post(ai_sdk_chat_agent_scoped),
        )
        // AI SDK reconnect: GET {api}/{chatId}/stream → chatId = thread_id
        .route("/v1/ai-sdk/chat/:thread_id/stream", get(resume_stream))
        .route("/v1/ai-sdk/threads/:thread_id/stream", get(resume_stream))
        .route(
            "/v1/ai-sdk/threads/:thread_id/messages",
            get(thread_messages),
        )
        .route("/v1/ai-sdk/threads/:thread_id/cancel", post(cancel_thread))
        .route(
            "/v1/ai-sdk/threads/:thread_id/interrupt",
            post(interrupt_thread),
        )
}

// ── Route handlers ──────────────────────────────────────────────────

async fn ai_sdk_chat(
    State(st): State<AppState>,
    Json(payload): Json<AiSdkChatRequest>,
) -> Result<Response, ApiError> {
    ai_sdk_chat_inner(st, payload).await
}

/// Thread-centric route: `POST /v1/ai-sdk/threads/:thread_id/runs`
async fn ai_sdk_chat_threaded(
    State(st): State<AppState>,
    Path(thread_id): Path<String>,
    Json(mut payload): Json<AiSdkChatRequest>,
) -> Result<Response, ApiError> {
    payload.thread_id = Some(thread_id);
    ai_sdk_chat_inner(st, payload).await
}

/// Agent-scoped route: `POST /v1/ai-sdk/agents/:agent_id/runs`
async fn ai_sdk_chat_agent_scoped(
    State(st): State<AppState>,
    Path(agent_id): Path<String>,
    Json(mut payload): Json<AiSdkChatRequest>,
) -> Result<Response, ApiError> {
    payload.agent_id = Some(agent_id);
    ai_sdk_chat_inner(st, payload).await
}

// ── Core chat handler ───────────────────────────────────────────────

async fn ai_sdk_chat_inner(st: AppState, payload: AiSdkChatRequest) -> Result<Response, ApiError> {
    let processed = super::request::process_chat_request(st.store.as_ref(), payload)
        .await
        .map_err(ApiError::BadRequest)?;

    let resume_only = processed.is_resume_only();

    let ProcessedRequest {
        thread_id,
        messages,
        decisions,
        state,
        agent_id,
    } = processed;

    if resume_only {
        // Resume-only: no new content, just tool-call decisions.
        let response = stream_existing_thread_from_now(&st, &thread_id)?;

        for (tool_call_id, resume) in decisions {
            if !st.mailbox.send_decision(&thread_id, tool_call_id, resume) {
                return Err(ApiError::BadRequest(
                    "no active run available for interaction responses".to_string(),
                ));
            }
        }

        return Ok(response);
    }

    let messages = crate::request::inject_frontend_context(messages, state);

    let mut request = RunRequest::new(thread_id.clone(), messages);
    if let Some(id) = agent_id {
        request = request.with_agent_id(id);
    }
    if !decisions.is_empty() {
        request = request.with_decisions(decisions);
    }
    let (_result, event_rx) = st
        .mailbox
        .submit(request)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let replay_buffer = Arc::new(EventReplayBuffer::new(st.config.replay_buffer_capacity));

    // Register buffer by thread_id (not run_id). External consumers only see
    // threads — runs are internal state. This matches AI SDK's reconnect URL
    // pattern: `{api}/{chatId}/stream` where chatId = threadId.
    st.replay_buffers
        .lock()
        .insert(thread_id.clone(), Arc::clone(&replay_buffer));

    let encoder = AiSdkEncoder::new();
    let sse_rx =
        wire_sse_relay_resumable(event_rx, encoder, st.config.sse_buffer_size, replay_buffer);

    // Spawn cleanup task: forward frames to client, but keep buffer alive for
    // the full run duration (not tied to client connection). This allows
    // reconnecting clients to use resume_stream even after the original client
    // disconnects.
    let buffers = Arc::clone(&st.replay_buffers);
    let replay_buf_for_cleanup = Arc::clone(st.replay_buffers.lock().get(&thread_id).unwrap());
    let cleanup_thread_id = thread_id;
    let mut sse_rx_forwarded = sse_rx;
    let (final_tx, final_rx) = tokio::sync::mpsc::channel::<Bytes>(st.config.sse_buffer_size);
    tokio::spawn(async move {
        let mut client_tx = Some(final_tx);
        let mut waiting_for_client_finish = false;
        while let Some(frame) = sse_rx_forwarded.recv().await {
            if is_waiting_state_snapshot_frame(&frame) {
                waiting_for_client_finish = true;
            }

            let should_close_client = is_suspended_finish_frame(&frame);
            let is_finish_step = is_finish_step_frame(&frame);

            if let Some(tx) = client_tx.as_ref() {
                if tx.send(frame).await.is_err() {
                    client_tx = None;
                } else if should_close_client {
                    // Stream naturally finished with tool-calls reason.
                    client_tx = None;
                } else if waiting_for_client_finish && is_finish_step {
                    let (_, finish_frame) = replay_buf_for_cleanup
                        .push_json(r#"{"type":"finish","finishReason":"tool-calls"}"#);
                    let _ = tx.send(finish_frame).await;
                    client_tx = None;
                    waiting_for_client_finish = false;
                }
            }
        }
        // Run is done — close subscribers so reconnected clients get EOF,
        // then remove the buffer from registry.
        replay_buf_for_cleanup.close_subscribers();
        buffers.lock().remove(&cleanup_thread_id);
    });

    Ok(ai_sdk_sse_response(sse_body_stream(final_rx)))
}

// ── SSE stream helpers ──────────────────────────────────────────────

fn stream_existing_thread_from_now(st: &AppState, thread_id: &str) -> Result<Response, ApiError> {
    let Some(buffer) = st.replay_buffers.lock().get(thread_id).cloned() else {
        return Err(ApiError::BadRequest(
            "no active run available for interaction responses".to_string(),
        ));
    };

    let (_replayed, live_rx) = buffer.subscribe_after(u64::MAX);
    let live_stream =
        tokio_stream::wrappers::UnboundedReceiverStream::new(live_rx).map(Ok::<Bytes, Infallible>);

    Ok(ai_sdk_sse_response(live_stream))
}

/// Reconnect to an active thread's event stream.
///
/// Called by AI SDK's `HttpChatTransport.reconnectToStream()` which issues
/// `GET {api}/{chatId}/stream`. In awaken, `chatId` maps to `thread_id`.
///
/// Replays buffered frames after the client's `Last-Event-ID` and then
/// streams any new frames produced by the still-running agent.
///
/// Returns **204 No Content** if no active stream exists for the thread.
/// AI SDK treats 204 as "nothing to resume" and returns `null` to the caller.
async fn resume_stream(
    State(st): State<AppState>,
    Path(thread_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let last_event_id: u64 = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let buffer = st.replay_buffers.lock().get(&thread_id).cloned();

    let Some(buffer) = buffer else {
        // AI SDK expects 204 for "no active stream" — not 404.
        return axum::http::StatusCode::NO_CONTENT.into_response();
    };

    // Atomically replay + subscribe under a single lock hold.
    // This guarantees no duplicates and no gaps.
    let (replayed, live_rx) = buffer.subscribe_after(last_event_id);

    let replay_stream = futures::stream::iter(replayed.into_iter().map(Ok::<Bytes, Infallible>));
    let live_stream =
        tokio_stream::wrappers::UnboundedReceiverStream::new(live_rx).map(Ok::<Bytes, Infallible>);
    let combined = replay_stream.chain(live_stream);

    ai_sdk_sse_response(combined)
}

// ── Thread messages (history) ───────────────────────────────────────

async fn thread_messages(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<crate::query::MessageQueryParams>,
) -> Result<Json<Value>, ApiError> {
    let messages = st
        .store
        .load_messages(&id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .unwrap_or_default();

    let offset = params.offset_or_default();
    let limit = params.clamped_limit();
    let encoded_messages = encode_history_messages(messages);
    let total = encoded_messages.len();

    let encoded = encoded_messages
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();

    Ok(Json(serde_json::json!({
        "messages": encoded,
        "total": total,
        "has_more": offset + encoded.len() < total,
    })))
}

fn encode_history_messages(messages: Vec<Message>) -> Vec<Value> {
    let mut encoded: Vec<Value> = Vec::new();
    let mut pending_tool_parts: std::collections::HashMap<String, (usize, usize)> =
        std::collections::HashMap::new();

    for message in messages {
        match message.role {
            Role::User | Role::System => {
                let parts = content_blocks_to_ui_parts(&message.content);
                if parts.is_empty() {
                    continue;
                }
                encoded.push(json!({
                    "id": message.id,
                    "role": match message.role {
                        Role::User => "user",
                        Role::System => "system",
                        _ => unreachable!(),
                    },
                    "parts": parts,
                }));
            }
            Role::Assistant => {
                let mut parts = content_blocks_to_ui_parts(&message.content);
                let message_index = encoded.len();
                if let Some(tool_calls) = &message.tool_calls {
                    for call in tool_calls {
                        let part_index = parts.len();
                        parts.push(tool_call_part(call));
                        pending_tool_parts.insert(call.id.clone(), (message_index, part_index));
                    }
                }
                if parts.is_empty() {
                    continue;
                }
                encoded.push(json!({
                    "id": message.id,
                    "role": "assistant",
                    "parts": parts,
                }));
            }
            Role::Tool => {
                let Some(call_id) = message.tool_call_id.as_ref() else {
                    encoded.push(json!({
                        "id": message.id,
                        "role": "tool",
                        "parts": content_blocks_to_ui_parts(&message.content),
                    }));
                    continue;
                };

                let Some((message_index, part_index)) = pending_tool_parts.remove(call_id) else {
                    encoded.push(json!({
                        "id": message.id,
                        "role": "tool",
                        "parts": content_blocks_to_ui_parts(&message.content),
                    }));
                    continue;
                };

                let Some(message_object) = encoded
                    .get_mut(message_index)
                    .and_then(Value::as_object_mut)
                else {
                    continue;
                };
                let Some(parts) = message_object
                    .get_mut("parts")
                    .and_then(Value::as_array_mut)
                else {
                    continue;
                };
                let Some(part) = parts.get_mut(part_index).and_then(Value::as_object_mut) else {
                    continue;
                };

                part.insert(
                    "state".to_string(),
                    Value::String("output-available".into()),
                );
                part.insert("output".to_string(), parse_tool_message_output(&message));
                part.insert("providerExecuted".to_string(), Value::Bool(true));
            }
        }
    }

    encoded
}

fn content_blocks_to_ui_parts(content: &[ContentBlock]) -> Vec<Value> {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(json!({"type": "text", "text": text})),
            _ => None,
        })
        .collect()
}

fn tool_call_part(call: &ToolCall) -> Value {
    json!({
        "type": format!("tool-{}", call.name),
        "toolName": call.name,
        "toolCallId": call.id,
        "state": "input-available",
        "input": call.arguments,
        "providerExecuted": true,
    })
}

fn parse_tool_message_output(message: &Message) -> Value {
    let text = extract_text(&message.content);
    serde_json::from_str(&text).unwrap_or(Value::String(text))
}

// ── Cancel / Interrupt ──────────────────────────────────────────────

async fn cancel_thread(
    State(st): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<Response, ApiError> {
    let cancelled = st
        .mailbox
        .cancel(&thread_id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    if cancelled {
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "cancel_requested",
                "thread_id": thread_id,
            })),
        )
            .into_response());
    }

    Err(ApiError::ThreadNotFound(thread_id))
}

async fn interrupt_thread(
    State(st): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<Response, ApiError> {
    let interrupted = st
        .mailbox
        .interrupt(&thread_id)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    if interrupted.active_job.is_some() || interrupted.superseded_count > 0 {
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "interrupt_requested",
                "thread_id": thread_id,
                "superseded_jobs": interrupted.superseded_count,
            })),
        )
            .into_response());
    }

    Err(ApiError::ThreadNotFound(thread_id))
}

// ── Frame inspection helpers ────────────────────────────────────────

fn is_suspended_finish_frame(frame: &Bytes) -> bool {
    let Ok(text) = std::str::from_utf8(frame) else {
        return false;
    };
    let Some(data_line) = text.lines().find_map(|line| line.strip_prefix("data: ")) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(data_line) else {
        return false;
    };

    value.get("type").and_then(Value::as_str) == Some("finish")
        && value.get("finishReason").and_then(Value::as_str) == Some("tool-calls")
}

fn parse_frame_json(frame: &Bytes) -> Option<Value> {
    let text = std::str::from_utf8(frame).ok()?;
    let data_line = text.lines().find_map(|line| line.strip_prefix("data: "))?;
    serde_json::from_str::<Value>(data_line).ok()
}

fn is_waiting_state_snapshot_frame(frame: &Bytes) -> bool {
    let Some(value) = parse_frame_json(frame) else {
        return false;
    };

    value.get("type").and_then(Value::as_str) == Some("data-state-snapshot")
        && value
            .get("data")
            .and_then(|data| data.get("extensions"))
            .and_then(|ext| ext.get("__runtime.run_lifecycle"))
            .and_then(|lifecycle| lifecycle.get("status"))
            .and_then(Value::as_str)
            == Some("waiting")
}

fn is_finish_step_frame(frame: &Bytes) -> bool {
    parse_frame_json(frame)
        .and_then(|value| value.get("type").and_then(Value::as_str).map(str::to_owned))
        .as_deref()
        == Some("finish-step")
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_suspended_finish_frame() {
        let frame =
            Bytes::from("id: 7\ndata: {\"type\":\"finish\",\"finishReason\":\"tool-calls\"}\n\n");
        assert!(is_suspended_finish_frame(&frame));

        let natural =
            Bytes::from("id: 8\ndata: {\"type\":\"finish\",\"finishReason\":\"stop\"}\n\n");
        assert!(!is_suspended_finish_frame(&natural));
    }

    #[test]
    fn encodes_tool_history_as_assistant_tool_parts() {
        let messages = vec![
            Message::user("show me a dashboard").with_id("u1".into()),
            Message::assistant_with_tool_calls(
                "Generating the dashboard now.",
                vec![ToolCall::new(
                    "call-1",
                    "render_json_ui",
                    json!({"prompt": "Quarterly dashboard"}),
                )],
            )
            .with_id("a1".into()),
            Message::tool("call-1", r#"{"content":{"root":"page"},"steps":1}"#)
                .with_id("t1".into()),
            Message::assistant("Done.").with_id("a2".into()),
        ];

        let encoded = encode_history_messages(messages);
        assert_eq!(encoded.len(), 3);

        let assistant_parts = encoded[1]["parts"].as_array().expect("assistant parts");
        assert_eq!(assistant_parts[0]["type"].as_str(), Some("text"));
        assert_eq!(
            assistant_parts[1]["type"].as_str(),
            Some("tool-render_json_ui")
        );
        assert_eq!(assistant_parts[1]["toolCallId"].as_str(), Some("call-1"));
        assert_eq!(
            assistant_parts[1]["state"].as_str(),
            Some("output-available")
        );
        assert_eq!(assistant_parts[1]["providerExecuted"].as_bool(), Some(true));
        assert_eq!(
            assistant_parts[1]["output"]["content"]["root"].as_str(),
            Some("page")
        );
    }

    #[test]
    fn encoded_history_total_matches_encoded_messages() {
        let messages = vec![
            Message::user("show me a dashboard").with_id("u1".into()),
            Message::assistant_with_tool_calls(
                "Generating the dashboard now.",
                vec![ToolCall::new(
                    "call-1",
                    "render_json_ui",
                    json!({"prompt": "Quarterly dashboard"}),
                )],
            )
            .with_id("a1".into()),
            Message::tool("call-1", r#"{"content":{"root":"page"},"steps":1}"#)
                .with_id("t1".into()),
        ];

        let encoded = encode_history_messages(messages);
        assert_eq!(encoded.len(), 2);
        assert_eq!(
            encoded.len(),
            2,
            "history pagination must use encoded message count"
        );
    }
}
