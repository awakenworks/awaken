//! Canonical lowering of Managed request inputs before they enter the shared Host.

use awaken_agent_contract::agent::content::ContentBlock;
use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};

/// Mint a fresh user message from plain text (Managed `user.message` content is
/// concatenated to text before it enters the host).
pub(crate) fn user_message(content: Vec<ContentBlock>) -> Message {
    Message::new(
        MessageId(awaken_runtime::fresh_process_id("usr")),
        Role::User,
        content,
    )
}
