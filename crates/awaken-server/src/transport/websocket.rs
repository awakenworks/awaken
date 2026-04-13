//! WebSocket transport for bidirectional agent communication.
//!
//! Unlike SSE (unidirectional server→client), WebSocket enables true bidirectional
//! messaging: clients can send follow-up messages within a run, and receive streamed
//! events over a single persistent connection.
//!
//! ## Message Flow
//!
//! ```text
//! Client (WebSocket)
//!   ↕ client_msg: { "type": "message", "content": "..." }
//!   ← server_event: { "type": "text_delta", "delta": "..." }
//! Server (Agent Runtime)
//! ```

use std::sync::Arc;

use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::message::Message;
use awaken_runtime::RunRequest;

use crate::app::AppState;
use crate::routes::ApiError;
use crate::transport::channel_sink::BoundedChannelEventSink;
use crate::transport::replay_buffer::EventReplayBuffer;
use crate::transport::transcoder::encode_event_to_sse;

/// RAII guard that decrements the WebSocket connections gauge on drop.
struct WsConnectionGuard;

impl Drop for WsConnectionGuard {
    fn drop(&mut self) {
        crate::metrics::dec_ws_connections();
    }
}

/// Client message types sent over WebSocket.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ClientMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub content: String,
}

/// WebSocket handler for bidirectional agent communication.
///
/// Upgrades an HTTP connection to WebSocket, then relays events from the
/// runtime to the client and forwards client messages back to the runtime.
pub async fn handle_websocket(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    socket: WebSocketUpgrade,
) -> impl IntoResponse {
    socket.on_upgrade(|ws| async move {
        let mailbox = state.mailbox.clone();
        let _guard = WsConnectionGuard;
        crate::metrics::inc_ws_connections();

        let (sender, receiver) = ws.split();

        // Bounded channel for runtime events destined for this WebSocket client.
        let (event_tx, event_rx) = mpsc::channel::<AgentEvent>(256);
        let event_sink = Arc::new(BoundedChannelEventSink::new(event_tx));

        // Replay buffer for reconnection. If the client disconnects and reconnects,
        // we can replay frames it missed.
        let replay_buffer = Arc::new(EventReplayBuffer::new(1024));

        // Spawn background task: pull events from runtime, transcode, send to WebSocket.
        let replay_buffer_clone = Arc::clone(&replay_buffer);
        tokio::spawn(async move {
            relay_events_to_ws(event_rx, sender, &replay_buffer_clone).await;
        });

        // Main loop: pull client messages, forward to runtime.
        if let Err(e) =
            relay_client_messages_to_runtime(receiver, mailbox, thread_id, event_sink).await
        {
            error!("WebSocket relay error: {e:?}");
        }
    })
}

/// Relay events from the runtime to the WebSocket client.
async fn relay_events_to_ws(
    mut event_rx: mpsc::Receiver<AgentEvent>,
    mut sender: futures::stream::SplitSink<WebSocket, axum::extract::ws::Message>,
    replay_buffer: &Arc<EventReplayBuffer>,
) {
    let mut encoder = crate::protocols::ai_sdk_v6::encoder::AiSdkEncoder::new();

    while let Some(event) = event_rx.recv().await {
        let frames = encode_event_to_sse(&mut encoder, &event);
        for frame in frames {
            if let Ok(json) = serde_json::from_slice::<Value>(&frame) {
                let ws_msg = axum::extract::ws::Message::Text(json.to_string());
                if sender.send(ws_msg).await.is_err() {
                    debug!("client disconnected during event relay");
                    return;
                }
                let json_str = serde_json::to_string(&json).unwrap_or_default();
                replay_buffer.push_json(&json_str);
            }
        }
    }
}

/// Relay client WebSocket messages back to the runtime inbox.
async fn relay_client_messages_to_runtime(
    mut receiver: futures::stream::SplitStream<WebSocket>,
    mailbox: Arc<crate::mailbox::Mailbox>,
    thread_id: String,
    _event_sink: Arc<BoundedChannelEventSink>,
) -> Result<(), ApiError> {
    while let Some(msg) = receiver.next().await {
        match msg {
            Ok(axum::extract::ws::Message::Text(json)) => {
                let client_msg: ClientMessage = serde_json::from_str(&json)
                    .map_err(|e| ApiError::BadRequest(format!("invalid message: {e}")))?;

                if client_msg.msg_type == "message" {
                    // Create a Message for the runtime with user role.
                    let message = Message::user(&client_msg.content);
                    let request = RunRequest::new(thread_id.clone(), vec![message]);

                    let (_result, _event_rx) = mailbox
                        .submit(request)
                        .await
                        .map_err(|e| ApiError::Internal(e.to_string()))?;
                }
            }
            Ok(axum::extract::ws::Message::Close(_)) => {
                debug!("client sent close frame");
                return Ok(());
            }
            Ok(_other) => {
                // Binary, Ping, Pong messages are ignored
            }
            Err(e) => {
                warn!("WebSocket error: {e}");
                return Ok(());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_client_message() {
        let json = r#"{"type": "message", "content": "Hello"}"#;
        let msg: ClientMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.msg_type, "message");
        assert_eq!(msg.content, "Hello");
    }

    #[test]
    fn serialize_client_message() {
        let msg = ClientMessage {
            msg_type: "message".to_string(),
            content: "Test".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized["type"], "message");
    }
}
