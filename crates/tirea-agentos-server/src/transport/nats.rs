use serde::Serialize;
use std::future::Future;
use std::sync::Arc;
use tirea_agentos::contracts::{AgentEvent, RunRequest};
use tirea_agentos::orchestrator::{AgentOs, ResolvedRun, RunStream};
use tirea_contract::{RuntimeInput, Transcoder};

use crate::transport::NatsProtocolError;
use crate::transport::{
    relay_binding, Endpoint, RelayCancellation, RuntimeEndpoint, SessionId, TranscoderEndpoint,
    TransportBinding, TransportCapabilities, TransportError,
};

#[derive(Clone, Debug)]
pub struct NatsTransportConfig {
    pub outbound_buffer: usize,
}

impl Default for NatsTransportConfig {
    fn default() -> Self {
        Self {
            outbound_buffer: 64,
        }
    }
}

/// Owns a NATS connection and transport configuration.
#[derive(Clone)]
pub struct NatsTransport {
    client: async_nats::Client,
    config: NatsTransportConfig,
}

impl NatsTransport {
    pub fn new(client: async_nats::Client, config: NatsTransportConfig) -> Self {
        Self { client, config }
    }

    pub fn client(&self) -> &async_nats::Client {
        &self.client
    }

    /// Subscribe to a NATS subject and dispatch each message to a handler.
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

    pub async fn run_and_publish<E, ErrEvent, BuildEncoder, BuildErrorEvent>(
        &self,
        os: &AgentOs,
        run_request: RunRequest,
        resolved: ResolvedRun,
        reply: async_nats::Subject,
        persist_run: bool,
        build_encoder: BuildEncoder,
        build_error_event: BuildErrorEvent,
    ) -> Result<(), NatsProtocolError>
    where
        E: Transcoder<Input = AgentEvent> + 'static,
        E::Output: Serialize + Send + 'static,
        ErrEvent: Serialize,
        BuildEncoder: FnOnce(&RunStream) -> E,
        BuildErrorEvent: FnOnce(String) -> ErrEvent,
    {
        let owner_agent_id = run_request.agent_id.clone();
        let run = match os
            .start_active_run_with_persistence(
                &owner_agent_id,
                run_request,
                resolved,
                persist_run,
                !persist_run,
            )
            .await
        {
            Ok(run) => run,
            Err(err) => {
                return self
                    .publish_error_event(reply, build_error_event(err.to_string()))
                    .await;
            }
        };
        let session_thread_id = run.thread_id.clone();
        let encoder = build_encoder(&run);
        let upstream = Arc::new(NatsReplyServerEndpoint::new(self.client.clone(), reply));
        let runtime_ep = Arc::new(RuntimeEndpoint::from_run_stream_with_buffer(
            run,
            self.config.outbound_buffer,
        ));
        let downstream = Arc::new(TranscoderEndpoint::new(runtime_ep, encoder));
        let binding = TransportBinding {
            session: SessionId {
                thread_id: session_thread_id,
            },
            caps: TransportCapabilities {
                upstream_async: false,
                downstream_streaming: true,
                single_channel_bidirectional: false,
                resumable_downstream: false,
            },
            upstream,
            downstream,
        };
        relay_binding(binding, RelayCancellation::new())
            .await
            .map_err(|e| NatsProtocolError::Run(format!("transport relay failed: {e}")))?;

        Ok(())
    }

    pub(crate) async fn publish_error_event<ErrEvent: Serialize>(
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
}

pub(crate) struct NatsReplyServerEndpoint {
    client: async_nats::Client,
    reply: async_nats::Subject,
}

impl NatsReplyServerEndpoint {
    pub(crate) fn new(client: async_nats::Client, reply: async_nats::Subject) -> Self {
        Self { client, reply }
    }
}

#[async_trait::async_trait]
impl<Evt> Endpoint<RuntimeInput, Evt> for NatsReplyServerEndpoint
where
    Evt: Serialize + Send + 'static,
{
    async fn recv(&self) -> Result<crate::transport::BoxStream<RuntimeInput>, TransportError> {
        let stream = futures::stream::empty::<Result<RuntimeInput, TransportError>>();
        Ok(Box::pin(stream))
    }

    async fn send(&self, item: Evt) -> Result<(), TransportError> {
        let payload = serde_json::to_vec(&item).map_err(|e| {
            tracing::warn!(error = %e, "failed to serialize NATS protocol event");
            TransportError::Io(format!("serialize event failed: {e}"))
        })?;
        self.client
            .publish(self.reply.clone(), payload.into())
            .await
            .map_err(|e| TransportError::Io(e.to_string()))
    }

    async fn close(&self) -> Result<(), TransportError> {
        Ok(())
    }
}
