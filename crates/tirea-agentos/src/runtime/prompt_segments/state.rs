use serde::{Deserialize, Serialize};
use tirea_contract::runtime::inference::ContextMessage;
use tirea_contract::runtime::state::AnyStateAction;
use tirea_state::State;

/// Durable store for persistent context messages injected into every inference.
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(
    path = "__prompt_segments",
    action = "PromptSegmentAction",
    scope = "thread"
)]
pub struct PromptSegmentState {
    #[serde(default)]
    pub items: Vec<ContextMessage>,
}

/// Reducer actions for [`PromptSegmentState`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PromptSegmentAction {
    /// Insert or replace a context message by key.
    Upsert { message: ContextMessage },
    /// Remove one context message by key.
    Remove { key: String },
    /// Remove every context message whose key starts with the given prefix.
    RemoveByKeyPrefix { prefix: String },
    /// Remove every context message flagged `consume_after_emit`.
    ConsumeAfterEmit,
    /// Clear all stored context messages.
    Clear,
}

impl PromptSegmentState {
    pub fn reduce(&mut self, action: PromptSegmentAction) {
        match action {
            PromptSegmentAction::Upsert { message } => {
                if let Some(existing) = self.items.iter_mut().find(|item| item.key == message.key) {
                    *existing = message;
                } else {
                    self.items.push(message);
                }
            }
            PromptSegmentAction::Remove { key } => {
                self.items.retain(|item| item.key != key);
            }
            PromptSegmentAction::RemoveByKeyPrefix { prefix } => {
                self.items.retain(|item| !item.key.starts_with(&prefix));
            }
            PromptSegmentAction::ConsumeAfterEmit => {
                self.items.retain(|item| !item.consume_after_emit);
            }
            PromptSegmentAction::Clear => self.items.clear(),
        }
    }
}

#[must_use]
pub fn upsert_context_message_action(message: ContextMessage) -> AnyStateAction {
    AnyStateAction::new::<PromptSegmentState>(PromptSegmentAction::Upsert { message })
}

#[must_use]
pub fn remove_context_message_action(key: impl Into<String>) -> AnyStateAction {
    AnyStateAction::new::<PromptSegmentState>(PromptSegmentAction::Remove { key: key.into() })
}

#[must_use]
pub fn remove_context_messages_by_prefix_action(prefix: impl Into<String>) -> AnyStateAction {
    AnyStateAction::new::<PromptSegmentState>(PromptSegmentAction::RemoveByKeyPrefix {
        prefix: prefix.into(),
    })
}

#[must_use]
pub fn consume_after_emit_context_messages_action() -> AnyStateAction {
    AnyStateAction::new::<PromptSegmentState>(PromptSegmentAction::ConsumeAfterEmit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tirea_contract::runtime::inference::ContextMessageTarget;

    fn msg(key: &str, content: &str, consume: bool) -> ContextMessage {
        ContextMessage::session(key, content).with_consume_after_emit(consume)
    }

    #[test]
    fn upsert_replaces_existing_key() {
        let mut state = PromptSegmentState {
            items: vec![msg("a", "old", true)],
        };
        state.reduce(PromptSegmentAction::Upsert {
            message: msg("a", "new", false),
        });
        assert_eq!(state.items.len(), 1);
        assert_eq!(state.items[0].content, "new");
        assert!(!state.items[0].consume_after_emit);
    }

    #[test]
    fn remove_by_key_prefix_drops_matching() {
        let mut state = PromptSegmentState {
            items: vec![msg("reminder:a", "one", true), msg("skill:b", "two", false)],
        };
        state.reduce(PromptSegmentAction::RemoveByKeyPrefix {
            prefix: "reminder:".into(),
        });
        assert_eq!(state.items.len(), 1);
        assert_eq!(state.items[0].key, "skill:b");
    }

    #[test]
    fn consume_after_emit_only_removes_ephemeral() {
        let mut state = PromptSegmentState {
            items: vec![msg("a", "one", true), msg("b", "two", false)],
        };
        state.reduce(PromptSegmentAction::ConsumeAfterEmit);
        assert_eq!(state.items.len(), 1);
        assert_eq!(state.items[0].key, "b");
    }

    #[test]
    fn remove_by_key() {
        let mut state = PromptSegmentState {
            items: vec![msg("a", "one", false), msg("b", "two", false)],
        };
        state.reduce(PromptSegmentAction::Remove { key: "a".into() });
        assert_eq!(state.items.len(), 1);
        assert_eq!(state.items[0].key, "b");
    }

    #[test]
    fn context_message_stored_with_correct_target() {
        let m = msg("test", "hello", false);
        assert_eq!(m.target, ContextMessageTarget::Session);
    }
}
