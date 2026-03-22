//! Context message injection types for prompt assembly.
//!
//! Plugins schedule `AddContextMessage` actions to inject content at specific positions
//! in the prompt. The loop runner consumes these actions before building the
//! `InferenceRequest`, applying system-level throttling based on `key` and `cooldown_turns`.

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
/// The loop runner applies throttling: if `cooldown_turns > 0`, the message is
/// skipped unless enough steps have passed since the last injection of the same `key`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextMessage {
    /// Deduplication and throttle identifier. Messages with the same key share
    /// throttle state. Typically `"plugin_name.purpose"`.
    pub key: String,
    /// Message role (typically `System` or `User`).
    pub role: Role,
    /// Content blocks.
    pub content: Vec<ContentBlock>,
    /// Visibility to external consumers.
    pub visibility: Visibility,
    /// Where in the prompt to insert this message.
    pub target: ContextMessageTarget,
    /// Minimum number of steps between injections of this key.
    /// `0` means inject every time (no throttling).
    pub cooldown_turns: u32,
}

impl ContextMessage {
    /// Create a system-target context message (injected after base system prompt).
    pub fn system(key: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            role: Role::System,
            content: vec![ContentBlock::text(text)],
            visibility: Visibility::Internal,
            target: ContextMessageTarget::System,
            cooldown_turns: 0,
        }
    }

    /// Create a suffix system message (appended after conversation history).
    pub fn suffix_system(key: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            role: Role::System,
            content: vec![ContentBlock::text(text)],
            visibility: Visibility::Internal,
            target: ContextMessageTarget::SuffixSystem,
            cooldown_turns: 0,
        }
    }

    /// Create a session-level context message.
    pub fn session(key: impl Into<String>, role: Role, text: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            role,
            content: vec![ContentBlock::text(text)],
            visibility: Visibility::Internal,
            target: ContextMessageTarget::Session,
            cooldown_turns: 0,
        }
    }

    /// Create a conversation-level context message.
    pub fn conversation(key: impl Into<String>, role: Role, text: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            role,
            content: vec![ContentBlock::text(text)],
            visibility: Visibility::All,
            target: ContextMessageTarget::Conversation,
            cooldown_turns: 0,
        }
    }

    /// Set cooldown turns (builder pattern).
    #[must_use]
    pub fn with_cooldown(mut self, turns: u32) -> Self {
        self.cooldown_turns = turns;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_context_message_defaults() {
        let msg = ContextMessage::system("my.key", "remember this");
        assert_eq!(msg.key, "my.key");
        assert_eq!(msg.role, Role::System);
        assert_eq!(msg.target, ContextMessageTarget::System);
        assert_eq!(msg.visibility, Visibility::Internal);
        assert_eq!(msg.cooldown_turns, 0);
    }

    #[test]
    fn with_cooldown_builder() {
        let msg = ContextMessage::system("k", "text").with_cooldown(5);
        assert_eq!(msg.cooldown_turns, 5);
    }

    #[test]
    fn context_message_serde_roundtrip() {
        let msg = ContextMessage {
            key: "test.key".into(),
            role: Role::User,
            content: vec![ContentBlock::text("hello")],
            visibility: Visibility::All,
            target: ContextMessageTarget::Conversation,
            cooldown_turns: 3,
        };
        let json = serde_json::to_value(&msg).unwrap();
        let parsed: ContextMessage = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn conversation_target_visible_by_default() {
        let msg = ContextMessage::conversation("conv.key", Role::User, "visible text");
        assert_eq!(msg.target, ContextMessageTarget::Conversation);
        assert_eq!(msg.visibility, Visibility::All);
    }

    #[test]
    fn system_target_internal_by_default() {
        let msg = ContextMessage::system("sys.key", "internal text");
        assert_eq!(msg.target, ContextMessageTarget::System);
        assert_eq!(msg.visibility, Visibility::Internal);

        let suffix = ContextMessage::suffix_system("suffix.key", "suffix text");
        assert_eq!(suffix.target, ContextMessageTarget::SuffixSystem);
        assert_eq!(suffix.visibility, Visibility::Internal);

        let session = ContextMessage::session("sess.key", Role::System, "session text");
        assert_eq!(session.target, ContextMessageTarget::Session);
        assert_eq!(session.visibility, Visibility::Internal);
    }

    #[test]
    fn with_cooldown_builder_pattern() {
        let msg = ContextMessage::conversation("k", Role::User, "text").with_cooldown(10);
        assert_eq!(msg.cooldown_turns, 10);
        // Verify other fields are preserved through the builder chain
        assert_eq!(msg.key, "k");
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.target, ContextMessageTarget::Conversation);
        assert_eq!(msg.visibility, Visibility::All);
    }
}
