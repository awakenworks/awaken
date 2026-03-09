use serde::Deserialize;
use std::sync::Arc;
use tirea_agentos::runtime::AgentOs;
use tirea_protocol_ai_sdk_v6::{AiSdkEncoder, AiSdkV6RunRequest, UIStreamEvent};

use super::runtime::apply_ai_sdk_extensions;
use crate::transport::nats::NatsTransport;
use crate::transport::NatsProtocolError;

/// Serve AI SDK v6 protocol over NATS.
pub async fn serve(
    transport: NatsTransport,
    os: Arc<AgentOs>,
    subject: String,
) -> Result<(), NatsProtocolError> {
    transport
        .serve(&subject, "aisdk", move |transport, msg| {
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
        #[serde(rename = "sessionId")]
        thread_id: String,
        input: String,
        #[serde(rename = "replySubject")]
        reply_subject: Option<String>,
    }

    let req: Req = serde_json::from_slice(&msg.payload)
        .map_err(|e| NatsProtocolError::BadRequest(e.to_string()))?;
    if req.input.trim().is_empty() {
        return Err(NatsProtocolError::BadRequest(
            "input cannot be empty".to_string(),
        ));
    }

    let reply = msg.reply.or(req.reply_subject.map(Into::into));
    let Some(reply) = reply else {
        return Err(NatsProtocolError::BadRequest(
            "missing reply subject".to_string(),
        ));
    };

    let mut resolved = match os.resolve(&req.agent_id) {
        Ok(r) => r,
        Err(err) => {
            return transport
                .publish_error_event(reply, UIStreamEvent::error(err.to_string()))
                .await;
        }
    };

    let req_for_runtime = AiSdkV6RunRequest::from_thread_input(req.thread_id, req.input);
    apply_ai_sdk_extensions(&mut resolved, &req_for_runtime);
    let run_request = req_for_runtime.into_runtime_run_request(req.agent_id);

    transport
        .run_and_publish(
            os.as_ref(),
            run_request,
            resolved,
            reply,
            false,
            move |_run| AiSdkEncoder::new(),
            UIStreamEvent::error,
        )
        .await
}
