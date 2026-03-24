//! NATS transport integration (feature-gated behind "nats").
//!
//! Provides [`NatsTransport`] for serving agent events over NATS publish/subscribe.
//! Each request arrives on a subscribed subject and events are published to the
//! reply subject using a protocol encoder (AG-UI or AI SDK).
//!
//! # Subject naming conventions
//!
//! - AG-UI protocol: `awaken.ag_ui.{thread_id}` (request/reply)
//! - AI SDK v6 protocol: `awaken.ai_sdk_v6.{thread_id}` (request/reply)
//!
//! Clients send a JSON request to the subject with a reply inbox.
//! The server publishes encoded events to the reply subject.

use std::future::Future;
use std::sync::Arc;

use awaken_contract::contract::event::AgentEvent;
use awaken_contract::contract::message::Message;
use awaken_contract::contract::transport::Transcoder;
use serde::{Deserialize, Serialize};

/// NATS transport configuration.
#[derive(Clone, Debug)]
pub struct NatsTransportConfig {
    /// Maximum number of outbound events buffered before back-pressure.
    pub outbound_buffer: usize,
}

impl Default for NatsTransportConfig {
    fn default() -> Self {
        Self {
            outbound_buffer: 64,
        }
    }
}

/// Error type for NATS protocol operations.
#[derive(Debug, thiserror::Error)]
pub enum NatsProtocolError {
    #[error("nats connect error: {0}")]
    Connect(#[from] async_nats::error::Error<async_nats::ConnectErrorKind>),

    #[error("nats subscribe error: {0}")]
    Subscribe(#[from] async_nats::SubscribeError),

    #[error("nats error: {0}")]
    Nats(#[from] async_nats::Error),

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("run error: {0}")]
    Run(String),
}

/// NATS run request sent by clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NatsRunRequest {
    /// Thread ID. New threads are created automatically.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Target agent ID. `None` = use default.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Messages for this run.
    #[serde(default)]
    pub messages: Vec<NatsRunMessage>,
}

/// A message in a NATS run request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NatsRunMessage {
    pub role: String,
    #[serde(default)]
    pub content: String,
}

/// Convert NATS run messages to contract messages.
pub fn convert_nats_messages(msgs: Vec<NatsRunMessage>) -> Vec<Message> {
    crate::message_convert::convert_role_content_pairs(
        msgs.into_iter().map(|m| (m.role, m.content)),
    )
}

/// Owns a NATS connection and transport configuration.
#[derive(Clone)]
pub struct NatsTransport {
    client: async_nats::Client,
    config: NatsTransportConfig,
}

impl NatsTransport {
    /// Create a new transport with an existing NATS client connection.
    pub fn new(client: async_nats::Client, config: NatsTransportConfig) -> Self {
        Self { client, config }
    }

    /// Access the underlying NATS client.
    pub fn client(&self) -> &async_nats::Client {
        &self.client
    }

    /// Access the transport configuration.
    pub fn config(&self) -> &NatsTransportConfig {
        &self.config
    }

    /// Subscribe to a NATS subject and dispatch each message to a handler.
    ///
    /// Spawns a tokio task per incoming message. The `protocol_label` is used in
    /// error log messages to identify which protocol handler failed.
    pub async fn serve<H, Fut>(
        &self,
        subject: &str,
        protocol_label: &'static str,
        handler: H,
    ) -> Result<(), NatsProtocolError>
    where
        H: Fn(NatsTransport, async_nats::Message) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), NatsProtocolError>> + Send + 'static,
    {
        use futures::StreamExt;
        let handler = Arc::new(handler);
        let mut sub = self.client.subscribe(subject.to_string()).await?;
        while let Some(msg) = sub.next().await {
            let transport = self.clone();
            let handler = handler.clone();
            tokio::spawn(async move {
                if let Err(e) = handler(transport, msg).await {
                    tracing::error!(error = %e, "nats {protocol_label} handler failed");
                }
            });
        }
        Ok(())
    }

    /// Publish a sequence of agent events to a NATS reply subject.
    ///
    /// Uses the shared [`relay_events_bounded`](crate::event_relay::relay_events_bounded)
    /// pipeline for prologue→transcode→epilogue, then publishes each serialized
    /// item to `reply`.
    pub async fn publish_events<E>(
        &self,
        rx: tokio::sync::mpsc::Receiver<AgentEvent>,
        reply: async_nats::Subject,
        encoder: E,
    ) -> Result<(), NatsProtocolError>
    where
        E: Transcoder<Input = AgentEvent> + 'static,
        E::Output: Serialize + Send + 'static,
    {
        use futures::StreamExt;

        let mut stream = Box::pin(crate::event_relay::relay_events_bounded(rx, encoder));

        while let Some(payload) = stream.next().await {
            self.client
                .publish(reply.clone(), payload.into())
                .await
                .map_err(|e| NatsProtocolError::Run(format!("publish event failed: {e}")))?;
        }

        Ok(())
    }

    /// Publish a serialized error event to a NATS reply subject.
    pub async fn publish_error_event<ErrEvent: Serialize>(
        &self,
        reply: async_nats::Subject,
        event: ErrEvent,
    ) -> Result<(), NatsProtocolError> {
        let payload = serde_json::to_vec(&event)
            .map_err(|e| NatsProtocolError::Run(format!("serialize error event failed: {e}")))?
            .into();
        if let Err(publish_err) = self.client.publish(reply, payload).await {
            return Err(NatsProtocolError::Run(format!(
                "publish error event failed: {publish_err}"
            )));
        }
        Ok(())
    }

    /// Serve AG-UI protocol over NATS.
    ///
    /// Subscribes to `awaken.ag_ui.*` and dispatches run requests using
    /// the AG-UI encoder. Requires a runtime to execute runs.
    pub async fn serve_ag_ui(
        &self,
        runtime: Arc<awaken_runtime::AgentRuntime>,
    ) -> Result<(), NatsProtocolError> {
        let rt = runtime;
        self.serve("awaken.ag_ui.*", "ag-ui", move |transport, msg| {
            let rt = rt.clone();
            async move {
                handle_protocol_request(transport, msg, &rt, "AG-UI", || {
                    crate::protocols::ag_ui::encoder::AgUiEncoder::new()
                })
                .await
            }
        })
        .await
    }

    /// Serve AI SDK v6 protocol over NATS.
    ///
    /// Subscribes to `awaken.ai_sdk_v6.*` and dispatches run requests using
    /// the AI SDK v6 encoder. Requires a runtime to execute runs.
    pub async fn serve_ai_sdk_v6(
        &self,
        runtime: Arc<awaken_runtime::AgentRuntime>,
    ) -> Result<(), NatsProtocolError> {
        let rt = runtime;
        self.serve("awaken.ai_sdk_v6.*", "ai-sdk-v6", move |transport, msg| {
            let rt = rt.clone();
            async move {
                handle_protocol_request(transport, msg, &rt, "AI SDK v6", || {
                    crate::protocols::ai_sdk_v6::encoder::AiSdkEncoder::new()
                })
                .await
            }
        })
        .await
    }
}

/// Generic handler for NATS protocol requests.
///
/// Parses the request, starts a run, and publishes encoded events to the reply
/// subject. The `encoder_factory` creates a protocol-specific encoder.
async fn handle_protocol_request<E>(
    transport: NatsTransport,
    msg: async_nats::Message,
    runtime: &awaken_runtime::AgentRuntime,
    protocol_label: &str,
    encoder_factory: impl FnOnce() -> E,
) -> Result<(), NatsProtocolError>
where
    E: Transcoder<Input = AgentEvent> + 'static,
    E::Output: Serialize + Send + 'static,
{
    let reply = msg
        .reply
        .ok_or_else(|| NatsProtocolError::BadRequest("missing reply subject".to_string()))?;

    let request: NatsRunRequest = serde_json::from_slice(&msg.payload)
        .map_err(|e| NatsProtocolError::BadRequest(format!("invalid request: {e}")))?;

    let messages = convert_nats_messages(request.messages);
    let (thread_id, messages) = crate::mailbox::prepare_run_inputs(request.thread_id, messages)
        .map_err(|e| {
            let message = match e {
                crate::routes::ApiError::BadRequest(msg)
                | crate::routes::ApiError::NotFound(msg)
                | crate::routes::ApiError::ThreadNotFound(msg)
                | crate::routes::ApiError::RunNotFound(msg)
                | crate::routes::ApiError::Internal(msg) => msg,
            };
            NatsProtocolError::BadRequest(message)
        })?;

    let (event_tx, event_rx) = tokio::sync::mpsc::channel(transport.config.outbound_buffer);
    let encoder = encoder_factory();

    // Spawn the event publisher
    let pub_transport = transport.clone();
    let pub_reply = reply.clone();
    let label = protocol_label.to_string();
    let publisher = tokio::spawn(async move {
        if let Err(e) = pub_transport
            .publish_events(event_rx, pub_reply, encoder)
            .await
        {
            tracing::warn!(error = %e, "{label} NATS event publish failed");
        }
    });

    // Execute the run
    let sink: Arc<dyn awaken_contract::contract::event_sink::EventSink> =
        Arc::new(crate::transport::channel_sink::BoundedChannelEventSink::new(event_tx));
    let run_request = awaken_runtime::RunRequest::new(thread_id, messages);
    let run_request = if let Some(aid) = request.agent_id {
        run_request.with_agent_id(aid)
    } else {
        run_request
    };

    if let Err(e) = runtime.run(run_request, sink.clone()).await {
        tracing::warn!(error = %e, "{protocol_label} NATS run failed");
    }

    // Drop sink to close the channel, then wait for publisher
    drop(sink);
    let _ = publisher.await;

    Ok(())
}

// ---------------------------------------------------------------------------
// NatsFlushPlugin
// ---------------------------------------------------------------------------

/// Plugin that automatically flushes a [`NatsBufferedWriter`] at run end.
///
/// Registers a [`Phase::RunEnd`](awaken_contract::model::Phase::RunEnd) hook
/// that calls [`NatsBufferedWriter::flush`] for the active thread, ensuring
/// buffered checkpoint data is persisted to the inner store.
pub struct NatsFlushPlugin {
    writer: Arc<awaken_stores::nats::NatsBufferedWriter>,
}

impl NatsFlushPlugin {
    /// Create a new flush plugin wrapping the given buffered writer.
    pub fn new(writer: Arc<awaken_stores::nats::NatsBufferedWriter>) -> Self {
        Self { writer }
    }
}

impl awaken_runtime::Plugin for NatsFlushPlugin {
    fn descriptor(&self) -> awaken_runtime::PluginDescriptor {
        awaken_runtime::PluginDescriptor { name: "nats_flush" }
    }

    fn register(
        &self,
        registrar: &mut awaken_runtime::PluginRegistrar,
    ) -> Result<(), awaken_contract::StateError> {
        registrar.register_phase_hook(
            "nats_flush",
            awaken_contract::model::Phase::RunEnd,
            NatsFlushHook {
                writer: self.writer.clone(),
            },
        )?;
        Ok(())
    }
}

/// Phase hook that flushes NATS buffered writes at run end.
struct NatsFlushHook {
    writer: Arc<awaken_stores::nats::NatsBufferedWriter>,
}

#[async_trait::async_trait]
impl awaken_runtime::PhaseHook for NatsFlushHook {
    async fn run(
        &self,
        ctx: &awaken_runtime::PhaseContext,
    ) -> Result<awaken_runtime::StateCommand, awaken_contract::StateError> {
        let thread_id = &ctx.run_input.identity.thread_id;
        if let Err(e) = self.writer.flush(thread_id).await {
            tracing::warn!(
                thread_id = %thread_id,
                error = %e,
                "NatsFlushPlugin: failed to flush buffered writes at run end"
            );
        }
        Ok(awaken_runtime::StateCommand::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nats_transport_config_default_buffer() {
        let config = NatsTransportConfig::default();
        assert_eq!(config.outbound_buffer, 64);
    }

    #[test]
    fn nats_protocol_error_display() {
        let err = NatsProtocolError::BadRequest("missing subject".to_string());
        assert_eq!(err.to_string(), "bad request: missing subject");

        let err = NatsProtocolError::Run("connection lost".to_string());
        assert_eq!(err.to_string(), "run error: connection lost");
    }

    #[test]
    fn nats_transport_config_custom_buffer() {
        let config = NatsTransportConfig {
            outbound_buffer: 128,
        };
        assert_eq!(config.outbound_buffer, 128);
    }

    #[test]
    fn convert_nats_messages_filters_unknown() {
        let msgs = vec![
            NatsRunMessage {
                role: "user".into(),
                content: "hello".into(),
            },
            NatsRunMessage {
                role: "assistant".into(),
                content: "hi".into(),
            },
            NatsRunMessage {
                role: "unknown".into(),
                content: "x".into(),
            },
            NatsRunMessage {
                role: "system".into(),
                content: "sys".into(),
            },
        ];
        let converted = convert_nats_messages(msgs);
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[0].text(), "hello");
        assert_eq!(converted[1].text(), "hi");
        assert_eq!(converted[2].text(), "sys");
    }

    #[test]
    fn convert_empty_nats_messages() {
        assert!(convert_nats_messages(vec![]).is_empty());
    }

    #[test]
    fn nats_run_request_serde_roundtrip() {
        let req = NatsRunRequest {
            thread_id: Some("t-1".to_string()),
            agent_id: Some("agent-a".to_string()),
            messages: vec![NatsRunMessage {
                role: "user".into(),
                content: "hello".into(),
            }],
        };
        let json = serde_json::to_string(&req).unwrap();
        let decoded: NatsRunRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.thread_id.unwrap(), "t-1");
        assert_eq!(decoded.agent_id.unwrap(), "agent-a");
        assert_eq!(decoded.messages.len(), 1);
    }

    #[test]
    fn nats_run_request_optional_fields() {
        let json = r#"{"messages":[{"role":"user","content":"hi"}]}"#;
        let req: NatsRunRequest = serde_json::from_str(json).unwrap();
        assert!(req.thread_id.is_none());
        assert!(req.agent_id.is_none());
        assert_eq!(req.messages.len(), 1);
    }

    /// Compile-time check that `NatsFlushPlugin` implements `Plugin`.
    /// Runtime integration test requires a NATS server, see `#[ignore]` tests.
    fn _assert_nats_flush_plugin_is_plugin() {
        fn _assert_impl<T: awaken_runtime::Plugin>() {}
        _assert_impl::<NatsFlushPlugin>();
    }
}
