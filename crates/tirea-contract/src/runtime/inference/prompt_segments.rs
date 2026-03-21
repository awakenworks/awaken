use super::{ContextMessage, ContextMessageTarget};
use crate::runtime::state::AnyStateAction;
use serde::{Deserialize, Serialize};
use tirea_state::State;

/// Consumption policy for a stored prompt segment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptSegmentConsumePolicy {
    /// Keep the segment in state until explicitly removed.
    #[default]
    Persistent,
    /// Remove the segment after it is emitted into a prompt.
    AfterEmit,
}

/// Durable hidden prompt segment stored in thread state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredPromptSegment {
    /// Logical owner/group for bulk operations.
    pub namespace: String,
    /// Stable identity within the namespace.
    pub key: String,
    /// Hidden prompt text to inject.
    pub content: String,
    /// Number of turns to skip after injection. `0` means inject every turn.
    #[serde(default)]
    pub cooldown_turns: u32,
    /// Where to place the segment in the assembled prompt.
    #[serde(default)]
    pub target: ContextMessageTarget,
    /// Whether to keep or consume the segment after emission.
    #[serde(default)]
    pub consume: PromptSegmentConsumePolicy,
}

impl StoredPromptSegment {
    #[must_use]
    pub fn new(
        namespace: impl Into<String>,
        key: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            namespace: namespace.into(),
            key: key.into(),
            content: content.into(),
            cooldown_turns: 0,
            target: ContextMessageTarget::System,
            consume: PromptSegmentConsumePolicy::Persistent,
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
    pub fn with_consume_policy(mut self, consume: PromptSegmentConsumePolicy) -> Self {
        self.consume = consume;
        self
    }

    #[must_use]
    pub fn into_context_message(self) -> ContextMessage {
        ContextMessage {
            key: format!("{}:{}", self.namespace, self.key),
            content: self.content,
            cooldown_turns: self.cooldown_turns,
            target: self.target,
        }
    }

    #[must_use]
    pub fn to_context_message(&self) -> ContextMessage {
        self.clone().into_context_message()
    }
}

/// Durable store for hidden prompt segments.
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[tirea(
    path = "__prompt_segments",
    action = "PromptSegmentAction",
    scope = "thread"
)]
pub struct PromptSegmentState {
    #[serde(default)]
    pub items: Vec<StoredPromptSegment>,
}

/// Reducer actions for [`PromptSegmentState`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PromptSegmentAction {
    /// Insert or replace a segment by `(namespace, key)`.
    Upsert { segment: StoredPromptSegment },
    /// Remove one segment by `(namespace, key)`.
    Remove { namespace: String, key: String },
    /// Remove every segment in a namespace.
    RemoveNamespace { namespace: String },
    /// Remove every segment flagged `after_emit`.
    ConsumeAfterEmit,
    /// Clear all prompt segments.
    Clear,
}

impl PromptSegmentState {
    pub fn reduce(&mut self, action: PromptSegmentAction) {
        match action {
            PromptSegmentAction::Upsert { segment } => {
                if let Some(existing) = self
                    .items
                    .iter_mut()
                    .find(|item| item.namespace == segment.namespace && item.key == segment.key)
                {
                    *existing = segment;
                } else {
                    self.items.push(segment);
                }
            }
            PromptSegmentAction::Remove { namespace, key } => {
                self.items
                    .retain(|item| !(item.namespace == namespace && item.key == key));
            }
            PromptSegmentAction::RemoveNamespace { namespace } => {
                self.items.retain(|item| item.namespace != namespace);
            }
            PromptSegmentAction::ConsumeAfterEmit => self
                .items
                .retain(|item| item.consume != PromptSegmentConsumePolicy::AfterEmit),
            PromptSegmentAction::Clear => self.items.clear(),
        }
    }
}

#[must_use]
pub fn upsert_prompt_segment_action(segment: StoredPromptSegment) -> AnyStateAction {
    AnyStateAction::new::<PromptSegmentState>(PromptSegmentAction::Upsert { segment })
}

#[must_use]
pub fn remove_prompt_segment_action(
    namespace: impl Into<String>,
    key: impl Into<String>,
) -> AnyStateAction {
    AnyStateAction::new::<PromptSegmentState>(PromptSegmentAction::Remove {
        namespace: namespace.into(),
        key: key.into(),
    })
}

#[must_use]
pub fn clear_prompt_segment_namespace_action(namespace: impl Into<String>) -> AnyStateAction {
    AnyStateAction::new::<PromptSegmentState>(PromptSegmentAction::RemoveNamespace {
        namespace: namespace.into(),
    })
}

#[must_use]
pub fn consume_after_emit_prompt_segments_action() -> AnyStateAction {
    AnyStateAction::new::<PromptSegmentState>(PromptSegmentAction::ConsumeAfterEmit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(
        namespace: &str,
        key: &str,
        content: &str,
        consume: PromptSegmentConsumePolicy,
    ) -> StoredPromptSegment {
        StoredPromptSegment {
            namespace: namespace.into(),
            key: key.into(),
            content: content.into(),
            cooldown_turns: 0,
            target: ContextMessageTarget::Session,
            consume,
        }
    }

    #[test]
    fn upsert_replaces_existing_namespace_key_pair() {
        let mut state = PromptSegmentState {
            items: vec![segment(
                "reminder",
                "a",
                "old",
                PromptSegmentConsumePolicy::AfterEmit,
            )],
        };

        state.reduce(PromptSegmentAction::Upsert {
            segment: segment(
                "reminder",
                "a",
                "new",
                PromptSegmentConsumePolicy::Persistent,
            ),
        });

        assert_eq!(state.items.len(), 1);
        assert_eq!(state.items[0].content, "new");
        assert_eq!(
            state.items[0].consume,
            PromptSegmentConsumePolicy::Persistent
        );
    }

    #[test]
    fn remove_namespace_drops_only_matching_items() {
        let mut state = PromptSegmentState {
            items: vec![
                segment(
                    "reminder",
                    "a",
                    "one",
                    PromptSegmentConsumePolicy::AfterEmit,
                ),
                segment("skill", "b", "two", PromptSegmentConsumePolicy::Persistent),
            ],
        };

        state.reduce(PromptSegmentAction::RemoveNamespace {
            namespace: "reminder".into(),
        });

        assert_eq!(state.items.len(), 1);
        assert_eq!(state.items[0].namespace, "skill");
    }

    #[test]
    fn consume_after_emit_only_removes_ephemeral_segments() {
        let mut state = PromptSegmentState {
            items: vec![
                segment(
                    "reminder",
                    "a",
                    "one",
                    PromptSegmentConsumePolicy::AfterEmit,
                ),
                segment("skill", "b", "two", PromptSegmentConsumePolicy::Persistent),
            ],
        };

        state.reduce(PromptSegmentAction::ConsumeAfterEmit);

        assert_eq!(state.items.len(), 1);
        assert_eq!(state.items[0].namespace, "skill");
    }

    #[test]
    fn stored_segment_maps_to_context_message_with_namespaced_key() {
        let entry = segment(
            "reminder",
            "abc",
            "hello",
            PromptSegmentConsumePolicy::AfterEmit,
        );
        let context = entry.to_context_message();
        assert_eq!(context.key, "reminder:abc");
        assert_eq!(context.content, "hello");
        assert_eq!(context.target, ContextMessageTarget::Session);
    }
}
