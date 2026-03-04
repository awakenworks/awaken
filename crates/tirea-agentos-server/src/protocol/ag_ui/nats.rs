use serde::Deserialize;
use std::sync::Arc;
use tirea_agentos::orchestrator::AgentOs;
use tirea_protocol_ag_ui::{AgUiProtocolEncoder, Event, RunAgentInput};

use super::runtime::apply_agui_extensions;

use crate::transport::nats::NatsTransport;
use crate::transport::NatsProtocolError;

/// Serve AG-UI protocol over NATS.
pub async fn serve(
    transport: NatsTransport,
    os: Arc<AgentOs>,
    subject: String,
) -> Result<(), NatsProtocolError> {
    transport
        .serve(&subject, "agui", move |transport, msg| {
            let os = os.clone();
            async move { handle_message(transport, os, msg).await }
        })
        .await
}

async fn handle_message(
    transport: NatsTransport,
    os: Arc<AgentOs>,
    msg: async_nats::Message,
) -> Result<(), NatsProtocolError> {
    #[derive(Debug, Deserialize)]
    struct Req {
        #[serde(rename = "agentId")]
        agent_id: String,
        request: RunAgentInput,
        #[serde(rename = "replySubject")]
        reply_subject: Option<String>,
    }

    let req: Req = serde_json::from_slice(&msg.payload)
        .map_err(|e| NatsProtocolError::BadRequest(e.to_string()))?;
    req.request
        .validate()
        .map_err(|e| NatsProtocolError::BadRequest(e.to_string()))?;

    let reply = msg.reply.or(req.reply_subject.map(Into::into));
    let Some(reply) = reply else {
        return Err(NatsProtocolError::BadRequest(
            "missing reply subject".to_string(),
        ));
    };

    let resolved = match os.resolve(&req.agent_id) {
        Ok(r) => r,
        Err(err) => {
            return transport
                .publish_error_event(reply, Event::run_error(err.to_string(), None))
                .await;
        }
    };

    let mut resolved = resolved;
    if let Err(err) = apply_agui_extensions(&mut resolved, &req.request) {
        return transport
            .publish_error_event(reply, Event::run_error(err.to_string(), None))
            .await;
    }
    let run_request = req.request.into_runtime_run_request(req.agent_id);

    transport
        .run_and_publish(
            os.as_ref(),
            run_request,
            resolved,
            reply,
            |_run| AgUiProtocolEncoder::new(),
            |msg| Event::run_error(msg, None),
        )
        .await
}
