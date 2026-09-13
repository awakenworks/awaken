//! MCP application notifications enter the existing Session Run admission port.
//! No HTTP loopback, second dialogue loop, or notification persistence owner.

use std::sync::{Arc, Weak};

use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_ext_mcp::notification::{
    McpNotificationConfig, McpNotificationHandler, McpSelectedInput,
};
use awaken_session_contract::{
    McpGenerationRef, SessionRunBackgroundApplication, stable_fingerprint,
};
use serde_json::{Value, json};

use crate::{HostError, SharedHost};

struct SessionMcpInput {
    host: Weak<SharedHost>,
    application: Weak<dyn SessionRunBackgroundApplication>,
    generation: McpGenerationRef,
    server: String,
    policy: McpNotificationConfig,
}

fn operation_id(session: &str, server: &str, method: &str, event: &str) -> String {
    format!(
        "mcp-input-{}",
        stable_fingerprint(&("mcp-application-input-v1", session, server, method, event))
    )
}

#[async_trait::async_trait]
impl McpNotificationHandler for SessionMcpInput {
    async fn receive(&self, method: &str, params: Value) {
        let Some(input) = self.policy.select(method, &params) else {
            return;
        };
        let Some(host) = self.host.upgrade() else {
            return;
        };
        let Some(application) = self.application.upgrade() else {
            return;
        };
        // The recipient is frozen by the exact connection owner, never chosen
        // by the notification payload. Existing root authority and transport
        // drain fences both apply to this incoming effect.
        if host.mcp_generation_root_authority(&self.generation) != Some(true)
            || !host
                .mcp_projection(&self.generation)
                .is_some_and(|p| p.state == crate::session_slot::McpProjectionState::Active)
        {
            return;
        }
        let (event_id, text) = match input {
            McpSelectedInput::Message { event_id, text } => (event_id, text),
            McpSelectedInput::CancelResponse {
                input_method,
                input_event_id,
            } => {
                let operation = operation_id(
                    &self.generation.session_id,
                    &self.server,
                    &input_method,
                    &input_event_id,
                );
                let target = awaken_session_contract::session_run_id(
                    &self.generation.session_id,
                    &operation,
                );
                if host
                    .cancel_durable(&self.generation.session_id, &target.0)
                    .await
                    .is_err()
                {
                    tracing::warn!(
                        code = "mcp_response_cancel_unaccepted",
                        "MCP exact response cancellation was not accepted"
                    );
                }
                return;
            }
        };
        let operation = operation_id(&self.generation.session_id, &self.server, method, &event_id);
        let message = Message::text(Id(operation.clone()), Role::User,
            json!({"source":"mcp","server":self.server,"method":method,"event_id":event_id,"text":text}).to_string());
        // The Session application's existing operation identity admits retries
        // without a second Run. Notification transport has no acknowledgement:
        // an exhausted retry is observable but not a claim of durable delivery.
        for attempt in 0..3 {
            match application
                .submit_session_run_background(
                    &operation,
                    &self.generation.session_id,
                    None,
                    vec![message.clone()],
                    None,
                )
                .await
            {
                Ok(_) => return,
                Err(error) if error.kind == awaken_session_contract::RunErrorKind::BadRequest => {
                    tracing::warn!(
                        code = "mcp_input_rejected",
                        "MCP input admission rejected; provider payload omitted"
                    );
                    return;
                }
                Err(_) if attempt < 2 => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await
                }
                Err(_) => tracing::warn!(
                    code = "mcp_input_unaccepted",
                    "MCP input admission unresolved; provider replay required"
                ),
            }
        }
    }
}

impl SharedHost {
    pub(crate) fn validate_mcp_input_placement(
        &self,
        configuration: &std::collections::BTreeMap<String, Value>,
        opaque_backend: bool,
    ) -> Result<(), HostError> {
        validate_input_placement(configuration, opaque_backend, self.upstream.is_some())
    }

    pub(crate) fn install_mcp_input_handlers(
        &self,
        projections: &[crate::session_slot::McpGenerationProjection],
        configuration: &std::collections::BTreeMap<String, Value>,
    ) -> Result<(), HostError> {
        for projection in projections {
            let Some(wiring) = &projection.native_wiring else {
                continue;
            };
            for (server, transport) in &wiring.notification_transports {
                let policy =
                    McpNotificationConfig::parse(configuration.get(&format!("mcp:{server}")))
                        .map_err(HostError::bad_request)?;
                if policy.notifications.is_empty() {
                    // A connection may survive a context refresh; an inactive
                    // policy must not retain the prior context's input handler.
                    let _ = transport.set_notification_handler(Vec::new(), None);
                    continue;
                }
                let application = self
                    .session_background_runs
                    .read()
                    .unwrap()
                    .clone()
                    .filter(|p| p.strong_count() > 0)
                    .ok_or_else(|| {
                        HostError::internal(
                            "MCP application input has no Session admission authority",
                        )
                    })?;
                let host = self
                    .dispatch_session_runtime()
                    .map_err(|e| HostError::internal(e.to_string()))?
                    .host;
                let methods = policy.notifications.keys().cloned().collect();
                transport
                    .set_notification_handler(
                        methods,
                        Some(Arc::new(SessionMcpInput {
                            host,
                            application,
                            generation: projection.request.generation.clone(),
                            server: server.clone(),
                            policy,
                        })),
                    )
                    .map_err(|_| {
                        HostError::bad_request(
                            "MCP transport does not support configured application input",
                        )
                    })?;
            }
        }
        Ok(())
    }
}

fn validate_input_placement(
    configuration: &std::collections::BTreeMap<String, Value>,
    opaque_backend: bool,
    remote_worker: bool,
) -> Result<(), HostError> {
    for (id, section) in configuration {
        if !id.starts_with("mcp:") {
            continue;
        }
        let policy = McpNotificationConfig::parse(Some(section)).map_err(HostError::bad_request)?;
        if !policy.notifications.is_empty() && (opaque_backend || remote_worker) {
            return Err(HostError::bad_request(
                "MCP application input requires a co-located Native Session application",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_placement_rejects_inert_opaque_and_remote_configuration() {
        // P1 configured input + Native + co-located -> admit; P2 ACP or remote
        // Worker -> reject, never silently ignore the input handler; P3 absent
        // input policy -> preserve ordinary MCP behavior in every placement.
        let configured = std::collections::BTreeMap::from([(
            "mcp:phone".into(),
            json!({"notifications":{
                "notifications/phone/input":{"event_id":"/id","delivery":{"kind":"message","text":"/text"}}
            }}),
        )]);
        for opaque in [false, true] {
            for remote in [false, true] {
                assert_eq!(
                    validate_input_placement(&configured, opaque, remote).is_ok(),
                    !opaque && !remote
                );
                assert!(validate_input_placement(&Default::default(), opaque, remote).is_ok());
            }
        }
    }
}
