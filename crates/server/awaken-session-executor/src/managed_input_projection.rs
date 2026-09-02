//! Canonical lowering of Managed request inputs before they enter the shared Host.

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_session_contract::RunError;

/// Mint a fresh user message from plain text (Managed `user.message` content is
/// concatenated to text before it enters the host).
pub(crate) fn user_message(content: Vec<ContentBlock>) -> Message {
    Message::new(
        MessageId(awaken_runtime::fresh_process_id("usr")),
        Role::User,
        content,
    )
}

/// Lower one stable Session System input into its sole durable Message form.
/// Fresh Run admission and same-Run tool reply resume share this constructor so
/// System identity, validation, and Role ordering cannot diverge.
pub(crate) fn session_system_message(
    session_id: &str,
    system: &awaken_session_contract::SessionUserRunSystemInput,
) -> Result<Message, RunError> {
    if session_id.trim().is_empty()
        || system.operation_id.trim().is_empty()
        || system.content.is_empty()
    {
        return Err(RunError::bad_request("Session System input is incomplete"));
    }
    Ok(Message::new(
        MessageId::session_system(session_id, &system.operation_id),
        Role::System,
        system.content.clone(),
    ))
}

/// Select the exact target carried by the admitted MCP realization request.
/// Keeping this identity projection explicit prevents credential materialization
/// from silently rebinding the request to a name/target tuple or another derived
/// lookup key.
#[must_use]
pub(crate) fn exact_credential_realization_target<T>(request_target: &T) -> &T {
    request_target
}

#[cfg(kani)]
#[kani::proof]
fn mcp_credential_realization_preserves_the_request_target_exactly() {
    let request_target: u64 = kani::any();
    let selected = exact_credential_realization_target(&request_target);
    assert_eq!(*selected, request_target);
    assert!(std::ptr::eq(selected, &request_target));
}

#[cfg(test)]
mod tests {
    use super::exact_credential_realization_target;

    #[test]
    fn materialization_target_is_the_original_typed_request_target() {
        let target = awaken_session_contract::McpTarget::parse_http(
            "https://credential-bound.example.test/mcp?tenant=exact",
        )
        .expect("valid target");
        let selected = exact_credential_realization_target(&target);
        assert!(std::ptr::eq(selected, &target));
        assert_eq!(selected, &target);
        assert_eq!(
            selected.http_url(),
            Some("https://credential-bound.example.test/mcp?tenant=exact")
        );
    }
}
