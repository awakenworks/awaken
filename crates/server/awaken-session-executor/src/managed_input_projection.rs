//! Canonical lowering of Managed request inputs before they enter the shared Host.

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_session_error_contract::RunError;

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
    system: &awaken_session_event_contract::SessionUserRunSystemInput,
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
