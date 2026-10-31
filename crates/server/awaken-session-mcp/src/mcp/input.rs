//! MCP application notifications enter the existing Session Run admission port.
//! No HTTP loopback, second dialogue loop, or notification persistence owner.

use std::sync::{Arc, Weak};

use awaken_agent_contract::agent::message::{Id, Message, Role};
use awaken_agent_contract::stable_fingerprint;
use awaken_ext_mcp::notification::{
    McpNotificationConfig, McpNotificationHandler, McpSelectedInput,
};
use awaken_session_configuration_contract::mcp::McpGenerationRef;
use awaken_session_execution_contract::SessionApplicationBinding;
use serde_json::{Value, json};

use crate::mcp::{McpError, McpGenerationAuthority, McpSessions};
use crate::mcp_material::McpGenerationProjection;

/// The Runtime Host as the recipient of MCP application input: its visible
/// generations, their root authority, and durable Run cancellation.
#[async_trait::async_trait]
pub trait McpInputHost: McpGenerationAuthority {
    /// The Session's visible exact generations; an unreadable installed-Session
    /// registry is an error, never an empty set.
    fn active_mcp_projections(
        &self,
        session: &str,
    ) -> Result<Vec<McpGenerationProjection>, awaken_session_types::RunError>;

    /// Cancel one exact durable Run; `false` when cancellation was not accepted.
    async fn cancel_durable_run(&self, session: &str, run_id: &str) -> bool;
}

/// Where admitted MCP application input goes: the Session Run admission
/// owner, reached through the executor's application binding, and the Host
/// whose authority every delivery rechecks.
pub struct McpInputRecipients {
    pub application: SessionApplicationBinding,
    pub host: Weak<dyn McpInputHost>,
}

struct SessionMcpInput {
    host: Weak<dyn McpInputHost>,
    application: SessionApplicationBinding,
    generation: McpGenerationRef,
    server: String,
    policy: McpNotificationConfig,
}

/// Whether an MCP application input bound to `bound` is admitted: one visible
/// generation renews the bound fence and holds the current root. A notification
/// has no caller to answer, so an unreadable registry is refused and reported.
///
/// | Rule | visible generations      | renews bound fence | root authority | effect   |
/// |---|---|---|---|---|
/// | I1 | readable, one matches    | yes                | `Some(true)`   | admitted |
/// | I2 | readable, one matches    | yes                | `Some(false)` / `None` | refused |
/// | I3 | readable, none renews    | no                 | any            | refused  |
/// | I4 | unreadable registry      | -                  | -              | error (refused, reported) |
/// | I5 | readable, matches        | yes                | unreadable     | error (refused, reported) |
fn admitted_by_root(
    host: &dyn McpInputHost,
    bound: &McpGenerationRef,
) -> Result<bool, awaken_session_types::RunError> {
    let projections = host.active_mcp_projections(bound.session_id().as_str())?;
    input_admitted(
        bound,
        projections
            .iter()
            .map(|projection| &projection.request.generation),
        |current| host.mcp_generation_root_authority(current),
    )
}

/// Rows I1-I3 and I5 of [`admitted_by_root`] over the visible generations.
fn input_admitted<'a>(
    bound: &McpGenerationRef,
    visible: impl IntoIterator<Item = &'a McpGenerationRef>,
    root: impl Fn(&McpGenerationRef) -> Result<Option<bool>, awaken_session_types::RunError>,
) -> Result<bool, awaken_session_types::RunError> {
    for current in visible {
        // The canonical realization owner may extend the same lease without
        // reconnecting this stream: only a renewal of the bound fence (same
        // Session, generation, owner and epoch; expiry never shortened) keeps it.
        if awaken_session_realization_contract::realization_generation_authorizes(current, bound)
            && root(current)? == Some(true)
        {
            return Ok(true);
        }
    }
    Ok(false)
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
        let Ok(application) = self.application.background_runs() else {
            return;
        };
        // The recipient is frozen by the exact connection owner, never chosen
        // by the notification payload. Existing root authority and transport
        // drain fences both apply to this incoming effect.
        let admitted = match admitted_by_root(host.as_ref(), &self.generation) {
            Ok(admitted) => admitted,
            Err(error) => {
                tracing::error!(
                    code = %error.code,
                    error = %error.message,
                    "MCP application input refused: the Session root authority is unreadable"
                );
                false
            }
        };
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
                if !host
                    .cancel_durable_run(self.generation.session_id().as_str(), &target.0)
                    .await
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
                Err(error) if error.kind == awaken_session_types::RunErrorKind::BadRequest => {
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

impl McpSessions {
    /// Configured MCP application input requires a co-located Native Session
    /// application; an opaque backend or a remote Worker rejects it.
    pub fn validate_input_placement(
        &self,
        configuration: &std::collections::BTreeMap<String, Value>,
        opaque_backend: bool,
        remote_worker: bool,
    ) -> Result<(), McpError> {
        validate_input_placement(configuration, opaque_backend, remote_worker)
    }

    /// Bind each visible generation's notification stream to its configured
    /// input policy. `recipients` is resolved only for a non-empty policy.
    pub fn install_input_handlers(
        &self,
        projections: &[McpGenerationProjection],
        configuration: &std::collections::BTreeMap<String, Value>,
        recipients: &dyn Fn() -> Result<McpInputRecipients, McpError>,
    ) -> Result<(), McpError> {
        for projection in projections {
            let Some(wiring) = &projection.native_wiring else {
                continue;
            };
            for (server, transport) in &wiring.notification_transports {
                let policy =
                    McpNotificationConfig::parse(configuration.get(&format!("mcp:{server}")))
                        .map_err(McpError::bad_request)?;
                if policy.notifications.is_empty() {
                    // A connection may survive a context refresh; an inactive
                    // policy must not retain the prior context's input handler.
                    let _ = transport.set_notification_handler(Vec::new(), None);
                    continue;
                }
                let McpInputRecipients { application, host } = recipients()?;
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
                        McpError::bad_request(
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
) -> Result<(), McpError> {
    for (id, section) in configuration {
        if !id.starts_with("mcp:") {
            continue;
        }
        let policy = McpNotificationConfig::parse(Some(section)).map_err(McpError::bad_request)?;
        if !policy.notifications.is_empty() && (opaque_backend || remote_worker) {
            return Err(McpError::bad_request(
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
        use {
            awaken_agent_contract::McpTarget,
            awaken_session_configuration_contract::mcp::{McpAttachmentId, McpGeneration},
        };
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

    fn phone_fence(session: &str, expiry: u64) -> McpGenerationRef {
        use {
            awaken_agent_contract::McpTarget,
            awaken_session_configuration_contract::mcp::{McpAttachmentId, McpGeneration},
        };
        McpGenerationRef::from_persisted(
            session.into(),
            McpGeneration::of_bare_target(
                McpAttachmentId("phone".into()),
                1,
                &McpTarget::parse_http("https://phone.test/mcp").unwrap(),
            ),
            "worker-1".into(),
            1,
            expiry,
        )
        .expect("nonzero epoch")
    }

    #[test]
    fn input_admission_requires_a_renewing_generation_with_a_readable_live_root() {
        // Rows I1-I3 and I5 of `admitted_by_root`; I4 is the unreadable
        // registry, covered by the next test.
        let bound = phone_fence("s1", 100);
        let renewed = phone_fence("s1", 200);
        let foreign = phone_fence("s2", 200);
        let poisoned = || {
            Err(awaken_session_types::RunError::unavailable_classified(
                "session_realization_lock_poisoned",
                "installed Sessions lock poisoned",
            ))
        };
        assert!(
            input_admitted(&bound, [&renewed], |_| Ok(Some(true))).unwrap(),
            "I1"
        );
        assert!(
            !input_admitted(&bound, [&renewed], |_| Ok(Some(false))).unwrap(),
            "I2 refused"
        );
        assert!(
            !input_admitted(&bound, [&renewed], |_| Ok(None)).unwrap(),
            "I2 absent"
        );
        assert!(
            !input_admitted(&bound, [&foreign], |_| Ok(Some(true))).unwrap(),
            "I3"
        );
        assert!(
            !input_admitted(&bound, [], |_| Ok(Some(true))).unwrap(),
            "I3 none visible"
        );
        let error = input_admitted(&bound, [&renewed], |_| poisoned()).unwrap_err();
        assert_eq!(error.code, "session_realization_lock_poisoned", "I5");
        // A foreign generation is never asked for its root, so an unreadable
        // root there cannot turn into an admission or an error.
        assert!(
            !input_admitted(&bound, [&foreign], |_| poisoned()).unwrap(),
            "I3 before I5"
        );
    }

    struct UnreadableRegistry;

    impl McpGenerationAuthority for UnreadableRegistry {
        fn mcp_generation_root_authority(
            &self,
            _generation: &McpGenerationRef,
        ) -> Result<Option<bool>, awaken_session_types::RunError> {
            Ok(Some(true))
        }
    }

    #[async_trait::async_trait]
    impl McpInputHost for UnreadableRegistry {
        fn active_mcp_projections(
            &self,
            _session: &str,
        ) -> Result<Vec<McpGenerationProjection>, awaken_session_types::RunError> {
            Err(awaken_session_types::RunError::unavailable_classified(
                "session_realization_lock_poisoned",
                "installed Sessions lock poisoned",
            ))
        }

        async fn cancel_durable_run(&self, _session: &str, _run_id: &str) -> bool {
            false
        }
    }

    #[test]
    fn an_unreadable_registry_refuses_input_as_an_error_not_an_empty_recipient_set() {
        // Row I4: the root authority would admit, but the visible generations
        // cannot be read, so the input is refused with the typed fault.
        let error = admitted_by_root(&UnreadableRegistry, &phone_fence("s1", 100)).unwrap_err();
        assert_eq!(error.code, "session_realization_lock_poisoned", "I4");
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
