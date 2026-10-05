//! MCP application notifications enter the existing Session Run admission port.
//! No HTTP loopback, second dialogue loop, or notification persistence owner.

use std::sync::{Arc, Weak};

use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_agent_contract::stable_fingerprint;
use awaken_ext_mcp::notification::{
    McpNotificationConfig, McpNotificationHandler, McpSelectedInput,
};
use awaken_session_application_contract::SessionRunBackgroundApplication;
use awaken_session_mcp_contract::McpGenerationRef;
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
        let admitted = host
            .active_mcp_projections(self.generation.session_id().as_str())
            .iter()
            .any(|projection| {
                let current = &projection.request.generation;
                // The canonical realization owner may extend the same lease
                // without reconnecting this stream: only a renewal of the bound
                // fence (same Session, generation, owner and epoch; expiry never
                // shortened) keeps it.
                awaken_session_realization_contract::realization_generation_authorizes(
                    current,
                    &self.generation,
                ) && host.mcp_generation_root_authority(current) == Some(true)
            });
        if !admitted {
            return;
        }
        let (event_id, text) = match input {
            McpSelectedInput::Message { event_id, text } => (event_id, text),
            McpSelectedInput::CancelResponse {
                input_method,
                input_event_id,
            } => {
                let operation = operation_id(
                    self.generation.session_id().as_str(),
                    &self.server,
                    &input_method,
                    &input_event_id,
                );
                let target = awaken_session_execution_contract::session_run_id(
                    self.generation.session_id(),
                    &operation,
                );
                if host
                    .cancel_durable(self.generation.session_id().as_str(), &target.0)
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
        let operation = operation_id(
            self.generation.session_id().as_str(),
            &self.server,
            method,
            &event_id,
        );
        let message = Message::text(Id(operation.clone()), Role::User,
            json!({"source":"mcp","server":self.server,"method":method,"event_id":event_id,"text":text}).to_string());
        // The Session application's existing operation identity admits retries
        // without a second Run. Notification transport has no acknowledgement:
        // an exhausted retry is observable but not a claim of durable delivery.
        for attempt in 0..3 {
            match application
                .submit_session_run_background(
                    &operation,
                    self.generation.session_id().as_str(),
                    None,
                    vec![message.clone()],
                    None,
                )
                .await
            {
                Ok(_) => return,
                Err(error)
                    if error.kind == awaken_session_error_contract::RunErrorKind::BadRequest =>
                {
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
        validate_input_placement(
            configuration,
            opaque_backend,
            self.is_database_less_worker(),
        )
    }

    pub(crate) fn install_mcp_input_handlers(
        &self,
        projections: &[crate::mcp_material::McpGenerationProjection],
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
                    .dispatch
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
    fn input_subscription_survives_only_same_owner_monotonic_renewal() {
        // The retain predicate of a bound input stream, as a decision table.
        // | Rule | current fence vs bound (s1, phone v1, worker-1, epoch 1, exp 100) | retain |
        // | L1   | identical                                                      | yes    |
        // | L1   | same tenure, expiry 200                                        | yes    |
        // | L2   | same tenure, expiry 99                                         | no     |
        // | L3   | Session s2                                                     | no     |
        // | L4   | incarnation worker-2 / epoch 2                                 | no     |
        // | L5   | version / attachment / content changed                         | no     |
        // Root liveness is independently checked at delivery, never inferred
        // from this match.
        use awaken_session_mcp_contract::{McpAttachmentId, McpGeneration, McpTarget};
        let generation = |name: &str, version: u64, url: &str| {
            McpGeneration::of_bare_target(
                McpAttachmentId(name.into()),
                version,
                &McpTarget::parse_http(url).unwrap(),
            )
        };
        let fence = |session: &str, generation: McpGeneration, incarnation: &str, epoch, expiry| {
            McpGenerationRef::from_persisted(
                session.into(),
                generation,
                incarnation.into(),
                epoch,
                expiry,
            )
            .expect("nonzero epoch")
        };
        let phone = || generation("phone", 1, "https://phone.test/mcp");
        let bound = fence("s1", phone(), "worker-1", 1, 100);
        for (rule, current, retain) in [
            ("L1", bound.clone(), true),
            ("L1", fence("s1", phone(), "worker-1", 1, 200), true),
            ("L2", fence("s1", phone(), "worker-1", 1, 99), false),
            ("L3", fence("s2", phone(), "worker-1", 1, 200), false),
            ("L4", fence("s1", phone(), "worker-2", 1, 200), false),
            ("L4", fence("s1", phone(), "worker-1", 2, 200), false),
            (
                "L5",
                fence(
                    "s1",
                    generation("phone", 2, "https://phone.test/mcp"),
                    "worker-1",
                    1,
                    200,
                ),
                false,
            ),
            (
                "L5",
                fence(
                    "s1",
                    generation("other", 1, "https://phone.test/mcp"),
                    "worker-1",
                    1,
                    200,
                ),
                false,
            ),
            (
                "L5",
                fence(
                    "s1",
                    generation("phone", 1, "https://other.test/mcp"),
                    "worker-1",
                    1,
                    200,
                ),
                false,
            ),
        ] {
            assert_eq!(
                awaken_session_realization_contract::realization_generation_authorizes(
                    &current, &bound
                ),
                retain,
                "{rule}"
            );
        }
    }

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
