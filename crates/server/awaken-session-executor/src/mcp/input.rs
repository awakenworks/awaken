// @formal surface requirement=protocol.mcp.mcp_protocol
// @formal reason: Static owner: MCP application notifications entering the Session through the existing
// @formal reason: Run admission port. Dynamic path: a selected server notification becomes one admitted
// @formal reason: Session message under the configured policy and its generation reference, with no HTTP
// @formal reason: loopback, second dialogue loop, or notification store of its own; a notification whose
// @formal reason: generation is superseded or whose host is gone is dropped rather than admitted.
// @formal reason: Evidence: protocol.mcp.mcp_protocol plus the MCP notification admission tests; this is a
// @formal reason: product-requirement classification, not a direct proof claim.
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

fn notification_generation_matches(bound: &McpGenerationRef, current: &McpGenerationRef) -> bool {
    // The canonical realization owner may extend the same lease without
    // reconnecting this stream. Only the proved monotonic expiry can differ;
    // every identity/content/version/owner coordinate remains exact.
    let mut renewed = bound.clone();
    renewed.lease_expires_at_unix_ms = current.lease_expires_at_unix_ms;
    current == &renewed && current.lease_expires_at_unix_ms >= bound.lease_expires_at_unix_ms
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
            .active_mcp_projections(self.generation.session_id.as_str())
            .iter()
            .any(|projection| {
                let current = &projection.request.generation;
                notification_generation_matches(&self.generation, current)
                    && host.mcp_generation_root_authority(current) == Some(true)
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
                    self.generation.session_id.as_str(),
                    &self.server,
                    &input_method,
                    &input_event_id,
                );
                let target = awaken_session_execution_contract::session_run_id(
                    &self.generation.session_id,
                    &operation,
                );
                if host
                    .cancel_durable(self.generation.session_id.as_str(), &target.0)
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
            self.generation.session_id.as_str(),
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
                    self.generation.session_id.as_str(),
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
        // L1 identical/longer same-epoch lease -> retain the input stream;
        // L2 shorter lease, L3 different Session, L4 owner/epoch changed,
        // L5 content/version/attachment changed -> reject. Root liveness is
        // independently checked at delivery, never inferred from this match.
        use awaken_session_mcp_contract::{McpAttachmentId, McpGeneration, McpTarget};
        let generation = |name: &str, version: u64, url: &str| {
            McpGeneration::of_bare_target(
                McpAttachmentId(name.into()),
                version,
                &McpTarget::parse_http(url).unwrap(),
            )
        };
        let bound = McpGenerationRef {
            session_id: "s1".into(),
            generation: generation("phone", 1, "https://phone.test/mcp"),
            runtime_incarnation: "worker-1".into(),
            lease_epoch: 1,
            lease_expires_at_unix_ms: 100,
        };
        assert!(notification_generation_matches(&bound, &bound));
        let mut current = bound.clone();
        current.lease_expires_at_unix_ms = 200;
        assert!(notification_generation_matches(&bound, &current));
        let mut bad = current.clone();
        bad.lease_expires_at_unix_ms = 99;
        assert!(!notification_generation_matches(&bound, &bad));
        let mut bad = current.clone();
        bad.session_id = "s2".into();
        assert!(!notification_generation_matches(&bound, &bad));
        let mut bad = current.clone();
        bad.runtime_incarnation = "worker-2".into();
        assert!(!notification_generation_matches(&bound, &bad));
        let mut bad = current.clone();
        bad.lease_epoch = 2;
        assert!(!notification_generation_matches(&bound, &bad));
        for changed in [
            generation("phone", 2, "https://phone.test/mcp"),
            generation("other", 1, "https://phone.test/mcp"),
            generation("phone", 1, "https://other.test/mcp"),
        ] {
            let mut bad = current.clone();
            bad.generation = changed;
            assert!(!notification_generation_matches(&bound, &bad));
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
