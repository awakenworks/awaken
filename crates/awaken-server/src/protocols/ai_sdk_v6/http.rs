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
use awaken_contract::contract::event_sink::EventSink;
use awaken_contract::contract::message::{Message, Role, ToolCall};
use awaken_contract::registry_spec::AgentSpec;

use crate::app::AppState;
use crate::http_run::wire_sse_relay;
use crate::http_sse::{sse_body_stream, sse_response};
use crate::routes::{ApiError, map_mailbox_error};
use crate::transport::channel_sink::BoundedChannelEventSink;
use crate::transport::replay_buffer::EventReplayBuffer;
use awaken_runtime::RunRequest;
use awaken_runtime::registry::resolve::RegistrySetResolver;
use awaken_runtime::registry::{AgentSpecRegistry, RegistryHandle, RegistrySet};
use awaken_runtime::{AgentResolver, AgentRuntime};

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
        .route(
            "/v1/ai-sdk/agent-previews/runs",
            post(ai_sdk_chat_preview_agent),
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

/// Draft-agent preview route:
/// `POST /v1/ai-sdk/agent-previews/runs`
async fn ai_sdk_chat_preview_agent(
    State(st): State<AppState>,
    Json(payload): Json<super::request::PreviewAgentChatRequest>,
) -> Result<Response, ApiError> {
    let super::request::PreviewAgentChatRequest {
        messages,
        thread_id,
        state,
        mut agent,
    } = payload;

    if agent.model.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "preview agent model cannot be empty".to_string(),
        ));
    }

    if agent.id.trim().is_empty() {
        agent.id = "draft-preview".to_string();
    }

    let processed = super::request::process_preview_chat_request(messages, thread_id, state)
        .map_err(ApiError::BadRequest)?;
    let candidate = build_preview_registry_set(&st, &agent)?;
    let preview_runtime = Arc::new(
        AgentRuntime::new(Arc::new(RegistrySetResolver::new(candidate.clone())))
            .with_registry_handle(RegistryHandle::new(candidate)),
    );

    let mut request = RunRequest::new(
        processed.thread_id,
        crate::request::inject_frontend_context(processed.messages, processed.state),
    )
    .with_agent_id(agent.id);
    if !processed.decisions.is_empty() {
        request = request.with_decisions(processed.decisions);
    }

    let (event_tx, event_rx) = tokio::sync::mpsc::channel(st.config.sse_buffer_size.max(32));
    let sink: Arc<dyn EventSink> = Arc::new(BoundedChannelEventSink::new(event_tx));
    tokio::spawn(async move {
        let _ = preview_runtime.run(request, sink).await;
    });

    let encoder = AiSdkEncoder::new();
    let sse_rx = wire_sse_relay(event_rx, encoder, st.config.sse_buffer_size, None);
    Ok(ai_sdk_sse_response(sse_body_stream(sse_rx)))
}

#[derive(Clone)]
struct PreviewAgentRegistry {
    preview: AgentSpec,
    fallback: Arc<dyn AgentSpecRegistry>,
}

impl PreviewAgentRegistry {
    fn new(preview: AgentSpec, fallback: Arc<dyn AgentSpecRegistry>) -> Self {
        Self { preview, fallback }
    }
}

impl AgentSpecRegistry for PreviewAgentRegistry {
    fn get_agent(&self, id: &str) -> Option<AgentSpec> {
        if id == self.preview.id {
            Some(self.preview.clone())
        } else {
            self.fallback.get_agent(id)
        }
    }

    fn agent_ids(&self) -> Vec<String> {
        let mut ids = self.fallback.agent_ids();
        if !ids.iter().any(|id| id == &self.preview.id) {
            ids.push(self.preview.id.clone());
        }
        ids
    }
}

fn build_preview_registry_set(st: &AppState, agent: &AgentSpec) -> Result<RegistrySet, ApiError> {
    let current = st
        .runtime
        .registry_set()
        .ok_or_else(|| ApiError::Internal("runtime does not expose a registry snapshot".into()))?;

    let candidate = RegistrySet {
        agents: Arc::new(PreviewAgentRegistry::new(
            agent.clone(),
            current.agents.clone(),
        )),
        tools: current.tools.clone(),
        models: current.models.clone(),
        providers: current.providers.clone(),
        plugins: current.plugins.clone(),
        backends: current.backends.clone(),
    };

    if agent.endpoint.is_none() {
        RegistrySetResolver::new(candidate.clone())
            .resolve(&agent.id)
            .map_err(|error| {
                ApiError::BadRequest(format!("invalid preview agent '{}': {error}", agent.id))
            })?;
    }

    Ok(candidate)
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

    // If the request contains tool-call decisions and the thread has an
    // active (suspended) run, reconnect the event sink and deliver
    // decisions to resume the run on a fresh SSE stream.
    if !decisions.is_empty() {
        let (new_event_tx, new_event_rx) = tokio::sync::mpsc::channel(256);
        let reconnected = st.mailbox.reconnect_sink(&thread_id, new_event_tx).await;

        if reconnected {
            let mut any_delivered = false;
            for (tool_call_id, resume) in &decisions {
                if st
                    .mailbox
                    .send_decision(&thread_id, tool_call_id.clone(), resume.clone())
                {
                    any_delivered = true;
                }
            }

            if any_delivered {
                // Wire the reconnected channel to a fresh SSE stream.
                let replay_buffer =
                    Arc::new(EventReplayBuffer::new(st.config.replay_buffer_capacity));
                st.insert_replay_buffer(thread_id.clone(), Arc::clone(&replay_buffer));

                let encoder = AiSdkEncoder::new();
                let sse_rx = wire_sse_relay(
                    new_event_rx,
                    encoder,
                    st.config.sse_buffer_size,
                    Some(Arc::clone(&replay_buffer)),
                );

                let st_cleanup = st.clone();
                let replay_buf = Arc::clone(&replay_buffer);
                let tid = thread_id;
                let mut rx = sse_rx;
                let (final_tx, final_rx) =
                    tokio::sync::mpsc::channel::<Bytes>(st.config.sse_buffer_size);
                tokio::spawn(async move {
                    while let Some(frame) = rx.recv().await {
                        if final_tx.send(frame).await.is_err() {
                            break;
                        }
                    }
                    replay_buf.close_subscribers();
                    st_cleanup.remove_replay_buffer(&tid);
                });

                return Ok(sse_response(sse_body_stream(final_rx)));
            }
        }
        // If reconnect or decision delivery failed, fall through.
    }

    if resume_only {
        // Pure resume with no active run — return empty stream.
        let (_, rx) = tokio::sync::mpsc::channel(1);
        let encoder = AiSdkEncoder::new();
        let sse_rx = crate::http_run::wire_sse_relay(rx, encoder, st.config.sse_buffer_size, None);
        return Ok(sse_response(sse_body_stream(sse_rx)));
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
        .map_err(map_mailbox_error)?;

    let replay_buffer = Arc::new(EventReplayBuffer::new(st.config.replay_buffer_capacity));

    // Register buffer by thread_id (not run_id). External consumers only see
    // threads — runs are internal state. This matches AI SDK's reconnect URL
    // pattern: `{api}/{chatId}/stream` where chatId = threadId.
    st.insert_replay_buffer(thread_id.clone(), Arc::clone(&replay_buffer));

    let encoder = AiSdkEncoder::new();
    let sse_rx = wire_sse_relay(
        event_rx,
        encoder,
        st.config.sse_buffer_size,
        Some(replay_buffer),
    );

    // Spawn cleanup task: forward frames to client, but keep buffer alive for
    // the full run duration (not tied to client connection). This allows
    // reconnecting clients to use resume_stream even after the original client
    // disconnects.
    let st_cleanup = st.clone();
    let replay_buf_for_cleanup = st
        .get_replay_buffer(&thread_id)
        .ok_or_else(|| ApiError::Internal("replay buffer disappeared after insert".into()))?;
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
        st_cleanup.remove_replay_buffer(&cleanup_thread_id);
    });

    Ok(ai_sdk_sse_response(sse_body_stream(final_rx)))
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

    let buffer = st.get_replay_buffer(&thread_id);

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

                // Check if this tool message represents a suspended tool
                // (the runtime appends "suspended: awaiting" messages for
                // suspended tool calls). Suspended tools should show as
                // input-available so the frontend renders its interactive UI.
                let output_text = parse_tool_message_output(&message);
                let is_suspended = output_text
                    .as_str()
                    .is_some_and(|s| s.contains("suspended"));

                if is_suspended {
                    // Keep state as input-available (set during tool part creation)
                    // so the frontend renders the color picker / user input UI.
                } else {
                    part.insert(
                        "state".to_string(),
                        Value::String("output-available".into()),
                    );
                    part.insert("output".to_string(), output_text);
                    part.insert("providerExecuted".to_string(), Value::Bool(true));
                }
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

    // ── content_blocks_to_ui_parts tests ───────────────────────────────

    #[test]
    fn content_blocks_to_ui_parts_text() {
        let blocks = vec![ContentBlock::text("hello")];
        let parts = content_blocks_to_ui_parts(&blocks);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["type"].as_str(), Some("text"));
        assert_eq!(parts[0]["text"].as_str(), Some("hello"));
    }

    #[test]
    fn content_blocks_to_ui_parts_empty() {
        let parts = content_blocks_to_ui_parts(&[]);
        assert!(parts.is_empty());
    }

    #[test]
    fn content_blocks_to_ui_parts_non_text_skipped() {
        let blocks = vec![
            ContentBlock::text("keep"),
            ContentBlock::image_url("https://example.com/img.png"),
        ];
        let parts = content_blocks_to_ui_parts(&blocks);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["text"].as_str(), Some("keep"));
    }

    // ── tool_call_part tests ───────────────────────────────────────────

    #[test]
    fn tool_call_part_structure() {
        let call = ToolCall::new("c1", "search", json!({"q": "rust"}));
        let part = tool_call_part(&call);
        assert_eq!(part["type"].as_str(), Some("tool-search"));
        assert_eq!(part["toolName"].as_str(), Some("search"));
        assert_eq!(part["toolCallId"].as_str(), Some("c1"));
        assert_eq!(part["state"].as_str(), Some("input-available"));
        assert_eq!(part["providerExecuted"].as_bool(), Some(true));
        assert_eq!(part["input"]["q"].as_str(), Some("rust"));
    }

    // ── parse_tool_message_output tests ────────────────────────────────

    #[test]
    fn parse_tool_message_output_json() {
        let msg = Message::tool("c1", r#"{"key": "value"}"#);
        let output = parse_tool_message_output(&msg);
        assert_eq!(output["key"].as_str(), Some("value"));
    }

    #[test]
    fn parse_tool_message_output_plain_text() {
        let msg = Message::tool("c1", "not json at all");
        let output = parse_tool_message_output(&msg);
        assert_eq!(output.as_str(), Some("not json at all"));
    }

    // ── parse_frame_json tests ─────────────────────────────────────────

    #[test]
    fn parse_frame_json_valid() {
        let frame = Bytes::from("id: 1\ndata: {\"type\":\"text\"}\n\n");
        let val = parse_frame_json(&frame).unwrap();
        assert_eq!(val["type"].as_str(), Some("text"));
    }

    #[test]
    fn parse_frame_json_no_data_line() {
        let frame = Bytes::from("id: 1\nevent: ping\n\n");
        assert!(parse_frame_json(&frame).is_none());
    }

    #[test]
    fn parse_frame_json_invalid_json() {
        let frame = Bytes::from("data: {not valid json}\n\n");
        assert!(parse_frame_json(&frame).is_none());
    }

    // ── is_finish_step_frame tests ─────────────────────────────────────

    #[test]
    fn is_finish_step_frame_true() {
        let frame = Bytes::from("id: 5\ndata: {\"type\":\"finish-step\"}\n\n");
        assert!(is_finish_step_frame(&frame));
    }

    #[test]
    fn is_finish_step_frame_false() {
        let frame = Bytes::from("id: 5\ndata: {\"type\":\"text\"}\n\n");
        assert!(!is_finish_step_frame(&frame));
    }
}
