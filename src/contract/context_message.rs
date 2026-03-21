//! Context message injection types for prompt assembly.
//!
//! Plugins schedule `AddContextMessage` actions during `BeforeInference` to inject
//! content at specific positions in the prompt. The loop runner consumes these actions
//! and assembles the final message list before building the `InferenceRequest`.

use serde::{Deserialize, Serialize};

use super::content::ContentBlock;
use super::message::{Role, Visibility};

/// Where in the prompt a context message should be inserted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextMessageTarget {
    /// Immediately after the base system prompt.
    #[default]
    System,
    /// In the session-context band, after all system messages, before conversation history.
    Session,
    /// Additional conversation messages before thread history.
    Conversation,
    /// At the end of the assembled prompt, after conversation history.
    SuffixSystem,
}

/// A context message to be injected into the prompt.
///
/// Scheduled by plugins via `cmd.schedule_action::<AddContextMessage>(...)`.
/// Throttling and deduplication are the plugin's responsibility — by the time
/// a `ContextMessage` reaches the loop runner, it is guaranteed to be injected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextMessage {
    /// Message role (typically `System` or `User`).
    pub role: Role,
    /// Content blocks.
    pub content: Vec<ContentBlock>,
    /// Visibility to external consumers.
    pub visibility: Visibility,
    /// Where in the prompt to insert this message.
    pub target: ContextMessageTarget,
}

impl ContextMessage {
    /// Create a system-target context message (injected after base system prompt).
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: vec![ContentBlock::text(text)],
            visibility: Visibility::Internal,
            target: ContextMessageTarget::System,
        }
    }

    /// Create a suffix system message (appended after conversation history).
    pub fn suffix_system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: vec![ContentBlock::text(text)],
            visibility: Visibility::Internal,
            target: ContextMessageTarget::SuffixSystem,
        }
    }

    /// Create a session-level context message.
    pub fn session(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentBlock::text(text)],
            visibility: Visibility::Internal,
            target: ContextMessageTarget::Session,
        }
    }

    /// Create a conversation-level context message.
    pub fn conversation(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentBlock::text(text)],
            visibility: Visibility::All,
            target: ContextMessageTarget::Conversation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_context_message_defaults() {
        let msg = ContextMessage::system("remember this");
        assert_eq!(msg.role, Role::System);
        assert_eq!(msg.target, ContextMessageTarget::System);
        assert_eq!(msg.visibility, Visibility::Internal);
    }

    #[test]
    fn suffix_system_target() {
        let msg = ContextMessage::suffix_system("final instruction");
        assert_eq!(msg.target, ContextMessageTarget::SuffixSystem);
    }

    #[test]
    fn context_message_serde_roundtrip() {
        let msg = ContextMessage {
            role: Role::User,
            content: vec![ContentBlock::text("hello")],
            visibility: Visibility::All,
            target: ContextMessageTarget::Conversation,
        };
        let json = serde_json::to_value(&msg).unwrap();
        let parsed: ContextMessage = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, msg);
    }
}
