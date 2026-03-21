use crate::thread::{gen_message_id, Message, Role, Visibility};
use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};

/// Injection target for a [`ContextMessage`].
///
/// `Conversation` reuses the same data model for regular messages while
/// bypassing prompt-context throttling. This matches the reverts split:
/// prompt-only context is distinct in lifecycle, but the payload model is
/// shared with conversation messages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextMessageTarget {
    /// Inject immediately after the base system prompt.
    ///
    /// This keeps static agent identity text at the front while allowing
    /// dynamic prefix context to remain independently cacheable.
    #[default]
    System,
    /// Inject in the session-context band before conversation history.
    Session,
    /// Inject additional conversation messages before thread history.
    ///
    /// This is intended for reverts-style prompt/new message alignment where
    /// the runtime should share one message model but preserve separate
    /// lifecycle semantics. These entries are never throttled by the prompt
    /// context cooldown tracker.
    Conversation,
    /// Inject at the end of the assembled prompt, after conversation history.
    ///
    /// Use this for hidden tail instructions that should shape the next model
    /// turn without being persisted as user-visible conversation messages.
    SuffixSystem,
}

/// A unified runtime message envelope for prompt-only context and ordinary
/// conversation messages.
///
/// For prompt-only context (`System`, `Session`, `SuffixSystem`):
/// - default role is `System`
/// - default visibility is `Internal`
/// - entries participate in key-based deduplication and cooldown throttling
///
/// For `Conversation` messages:
/// - role/visibility may be any supported combination
/// - entries bypass context throttling and deduplication
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextMessage {
    /// Unique identifier for deduplication and throttle tracking.
    ///
    /// For `Conversation` messages this is informational only.
    pub key: String,
    /// Message role.
    #[serde(default = "default_role")]
    pub role: Role,
    /// The text content to inject or append.
    pub content: String,
    /// Message visibility. Context defaults to `Internal`.
    #[serde(default = "default_visibility")]
    pub visibility: Visibility,
    /// Number of turns to skip after injection. `0` means inject every turn.
    ///
    /// Only applies to prompt-context targets. `Conversation` entries bypass
    /// cooldown filtering regardless of this value.
    #[serde(default)]
    pub cooldown_turns: u32,
    /// Where to inject the content in the message sequence.
    #[serde(default)]
    pub target: ContextMessageTarget,
    /// Whether a durable backing entry should be consumed after this message is
    /// actually emitted into the prompt.
    ///
    /// Only applies to prompt-context targets.
    #[serde(default)]
    pub consume_after_emit: bool,
}

const fn default_role() -> Role {
    Role::System
}

const fn default_visibility() -> Visibility {
    Visibility::Internal
}

impl ContextMessage {
    #[must_use]
    pub fn new(key: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            role: Role::System,
            content: content.into(),
            visibility: Visibility::Internal,
            cooldown_turns: 0,
            target: ContextMessageTarget::System,
            consume_after_emit: false,
        }
    }

    #[must_use]
    pub fn with_target(mut self, target: ContextMessageTarget) -> Self {
        self.target = target;
        self
    }

    #[must_use]
    pub fn with_role(mut self, role: Role) -> Self {
        self.role = role;
        self
    }

    #[must_use]
    pub fn with_visibility(mut self, visibility: Visibility) -> Self {
        self.visibility = visibility;
        self
    }

    #[must_use]
    pub fn with_cooldown_turns(mut self, cooldown_turns: u32) -> Self {
        self.cooldown_turns = cooldown_turns;
        self
    }

    #[must_use]
    pub fn with_consume_after_emit(mut self, consume_after_emit: bool) -> Self {
        self.consume_after_emit = consume_after_emit;
        self
    }

    #[must_use]
    pub fn system(key: impl Into<String>, content: impl Into<String>) -> Self {
        Self::new(key, content)
    }

    #[must_use]
    pub fn session(key: impl Into<String>, content: impl Into<String>) -> Self {
        Self::new(key, content).with_target(ContextMessageTarget::Session)
    }

    #[must_use]
    pub fn suffix_system(key: impl Into<String>, content: impl Into<String>) -> Self {
        Self::new(key, content).with_target(ContextMessageTarget::SuffixSystem)
    }

    #[must_use]
    pub fn conversation(
        key: impl Into<String>,
        role: Role,
        visibility: Visibility,
        content: impl Into<String>,
    ) -> Self {
        Self {
            key: key.into(),
            role,
            content: content.into(),
            visibility,
            cooldown_turns: 0,
            target: ContextMessageTarget::Conversation,
            consume_after_emit: false,
        }
    }

    #[must_use]
    pub fn conversation_user(content: impl Into<String>) -> Self {
        let content = content.into();
        let key = conversation_key(Role::User, Visibility::All, &content);
        Self::conversation(key, Role::User, Visibility::All, content)
    }

    #[must_use]
    pub fn conversation_internal_system(content: impl Into<String>) -> Self {
        let content = content.into();
        let key = conversation_key(Role::System, Visibility::Internal, &content);
        Self::conversation(key, Role::System, Visibility::Internal, content)
    }

    /// Build a session-scoped context entry using a deterministic key derived
    /// from the content. Intended for legacy session-context callers that lack
    /// an explicit stable key but still need unified throttle/placement logic.
    #[must_use]
    pub fn session_text(content: impl Into<String>) -> Self {
        let content = content.into();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        content.hash(&mut hasher);
        Self::session(format!("legacy_session:{:016x}", hasher.finish()), content)
    }

    #[must_use]
    pub fn is_prompt_injected(&self) -> bool {
        self.target != ContextMessageTarget::Conversation
    }

    #[must_use]
    pub fn should_throttle(&self) -> bool {
        self.is_prompt_injected()
    }

    #[must_use]
    pub fn to_message(&self) -> Message {
        Message {
            id: Some(gen_message_id()),
            role: self.role,
            content: self.content.clone(),
            tool_calls: None,
            tool_call_id: None,
            visibility: self.visibility,
            metadata: None,
        }
    }
}

fn conversation_key(role: Role, visibility: Visibility, content: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    role.hash(&mut hasher);
    visibility.hash(&mut hasher);
    content.hash(&mut hasher);
    format!("conversation:{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip() {
        let entry = ContextMessage {
            key: "skills".into(),
            role: Role::System,
            content: "<skills>...</skills>".into(),
            visibility: Visibility::Internal,
            cooldown_turns: 10,
            target: ContextMessageTarget::System,
            consume_after_emit: false,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let restored: ContextMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.key, "skills");
        assert_eq!(restored.role, Role::System);
        assert_eq!(restored.visibility, Visibility::Internal);
        assert_eq!(restored.cooldown_turns, 10);
        assert_eq!(restored.target, ContextMessageTarget::System);
        assert!(!restored.consume_after_emit);
    }

    #[test]
    fn defaults_are_hidden_system_and_zero_cooldown() {
        let json = r#"{"key":"test","content":"hello"}"#;
        let entry: ContextMessage = serde_json::from_str(json).unwrap();
        assert_eq!(entry.role, Role::System);
        assert_eq!(entry.visibility, Visibility::Internal);
        assert_eq!(entry.cooldown_turns, 0);
        assert_eq!(entry.target, ContextMessageTarget::System);
        assert!(!entry.consume_after_emit);
    }

    #[test]
    fn session_target_serde() {
        let entry = ContextMessage {
            key: "git_status".into(),
            role: Role::System,
            content: "branch: main".into(),
            visibility: Visibility::Internal,
            cooldown_turns: 3,
            target: ContextMessageTarget::Session,
            consume_after_emit: false,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"session\""));
        let restored: ContextMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.target, ContextMessageTarget::Session);
    }

    #[test]
    fn conversation_target_serde() {
        let entry = ContextMessage::conversation_user("continue");
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"conversation\""));
        let restored: ContextMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.target, ContextMessageTarget::Conversation);
        assert_eq!(restored.role, Role::User);
        assert_eq!(restored.visibility, Visibility::All);
    }

    #[test]
    fn suffix_target_serde() {
        let entry = ContextMessage {
            key: "skill_tail".into(),
            role: Role::System,
            content: "Do X".into(),
            visibility: Visibility::Internal,
            cooldown_turns: 0,
            target: ContextMessageTarget::SuffixSystem,
            consume_after_emit: false,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"suffix_system\""));
        let restored: ContextMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.target, ContextMessageTarget::SuffixSystem);
    }

    #[test]
    fn session_text_auto_keys_by_content() {
        let a = ContextMessage::session_text("branch: main");
        let b = ContextMessage::session_text("branch: main");
        let c = ContextMessage::session_text("branch: dev");
        assert_eq!(a.target, ContextMessageTarget::Session);
        assert_eq!(a.key, b.key);
        assert_ne!(a.key, c.key);
    }

    #[test]
    fn consume_after_emit_roundtrip() {
        let entry = ContextMessage::session("reminder", "Remember").with_consume_after_emit(true);
        let json = serde_json::to_string(&entry).unwrap();
        let restored: ContextMessage = serde_json::from_str(&json).unwrap();
        assert!(restored.consume_after_emit);
    }

    #[test]
    fn conversation_messages_do_not_throttle() {
        let entry = ContextMessage::conversation_user("continue");
        assert!(!entry.should_throttle());
    }

    #[test]
    fn to_message_preserves_role_visibility_and_content() {
        let entry = ContextMessage::conversation(
            "m1",
            Role::Assistant,
            Visibility::Internal,
            "hidden reply",
        );
        let message = entry.to_message();
        assert_eq!(message.role, Role::Assistant);
        assert_eq!(message.visibility, Visibility::Internal);
        assert_eq!(message.content, "hidden reply");
    }
}
