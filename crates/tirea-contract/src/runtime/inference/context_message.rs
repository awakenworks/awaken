use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};

/// Injection target for a [`ContextMessage`].
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
    /// Inject at the end of the assembled prompt, after conversation history.
    ///
    /// Use this for hidden tail instructions that should shape the next model
    /// turn without being persisted as user-visible conversation messages.
    SuffixSystem,
}

/// A structured, managed context entry with throttle metadata.
///
/// Plugins emit `BeforeInferenceAction::AddContextMessage(ContextMessage)` to inject
/// context with lifecycle control. Each context
/// entry carries metadata that allows the loop runner to:
///
/// - **Deduplicate** by `key` (same key replaces previous content)
/// - **Throttle** by `cooldown_turns` (skip injection if recently injected
///   and content hasn't changed)
/// - **Target** by `target` (prefix system, session context, or suffix system)
///
/// The loop runner maintains a per-run tracker. An entry is injected when:
/// 1. It has never been injected before, OR
/// 2. Its `cooldown_turns` have elapsed since last injection, OR
/// 3. Its content has changed (regardless of cooldown)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextMessage {
    /// Unique identifier for deduplication and throttle tracking.
    pub key: String,
    /// The text content to inject.
    pub content: String,
    /// Number of turns to skip after injection. `0` means inject every turn.
    ///
    /// For example, `cooldown_turns: 5` means: inject on turn 0, skip turns
    /// 1–5, inject again on turn 6 (unless content changed earlier).
    #[serde(default)]
    pub cooldown_turns: u32,
    /// Where to inject the content in the message sequence.
    #[serde(default)]
    pub target: ContextMessageTarget,
    /// Whether a durable backing entry should be consumed after this message is
    /// actually emitted into the prompt.
    #[serde(default)]
    pub consume_after_emit: bool,
}

impl ContextMessage {
    #[must_use]
    pub fn new(key: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            content: content.into(),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_roundtrip() {
        let entry = ContextMessage {
            key: "skills".into(),
            content: "<skills>...</skills>".into(),
            cooldown_turns: 10,
            target: ContextMessageTarget::System,
            consume_after_emit: false,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let restored: ContextMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.key, "skills");
        assert_eq!(restored.cooldown_turns, 10);
        assert_eq!(restored.target, ContextMessageTarget::System);
        assert!(!restored.consume_after_emit);
    }

    #[test]
    fn defaults_are_system_and_zero_cooldown() {
        let json = r#"{"key":"test","content":"hello"}"#;
        let entry: ContextMessage = serde_json::from_str(json).unwrap();
        assert_eq!(entry.cooldown_turns, 0);
        assert_eq!(entry.target, ContextMessageTarget::System);
        assert!(!entry.consume_after_emit);
    }

    #[test]
    fn session_target_serde() {
        let entry = ContextMessage {
            key: "git_status".into(),
            content: "branch: main".into(),
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
    fn suffix_target_serde() {
        let entry = ContextMessage {
            key: "skill_tail".into(),
            content: "Do X".into(),
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
}
