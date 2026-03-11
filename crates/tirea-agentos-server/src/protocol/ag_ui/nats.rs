use serde::Deserialize;
use std::sync::Arc;
use tirea_agentos::runtime::AgentOs;
use tirea_protocol_ag_ui::{AgUiProtocolEncoder, Event, RunAgentInput};

use super::runtime::apply_agui_extensions;
use crate::service::{EnqueueOptions, MailboxService};
use crate::transport::nats::NatsTransport;
use crate::transport::NatsProtocolError;

/// Serve AG-UI protocol over NATS.
pub async fn serve(
    transport: NatsTransport,
    os: Arc<AgentOs>,
    mailbox_service: Arc<MailboxService>,
    subject: String,
) -> Result<(), NatsProtocolError> {
    transport
        .serve(&subject, "agui", move |transport, msg| {
            let os = os.clone();
            let mailbox_service = mailbox_service.clone();
            async move { handle_message(transport, os, mailbox_service, msg).await }
        })
        .await
}

async fn handle_message(
    transport: NatsTransport,
    os: Arc<AgentOs>,
    mailbox_service: Arc<MailboxService>,
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
    apply_agui_extensions(&mut resolved, &req.request);
    let agent_id = req.agent_id.clone();
    let frontend_run_id = req.request.run_id.clone();
    let mut run_request = req.request.into_runtime_run_request(agent_id.clone());
    run_request.run_id = None;

    let run = match mailbox_service
        .submit_streaming(&agent_id, run_request, EnqueueOptions::default())
        .await
    {
        Ok(run) => run,
        Err(err) => {
            return transport
                .publish_error_event(reply, Event::run_error(err.to_string(), None))
                .await;
        }
    };

    transport
        .publish_run_stream(run, reply, move |_run| {
            AgUiProtocolEncoder::new_with_frontend_run_id(frontend_run_id)
        })
        .await
}
