use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::state::{MergeStrategy, StateKey};
use awaken_contract::contract::context_message::ContextMessage;
use awaken_contract::contract::inference::InferenceOverride;

// ---------------------------------------------------------------------------
// Action specs
// ---------------------------------------------------------------------------

/// Action spec for injecting a context message into the prompt.
///
/// Scheduled by `BeforeInference` hooks via `cmd.schedule_action::<AddContextMessage>(...)`.
/// Handled during `run_phase(BeforeInference)` — the handler applies throttle logic
/// and writes accepted messages to [`ContextMessageStore`].
pub struct AddContextMessage;

impl awaken_contract::model::ScheduledActionSpec for AddContextMessage {
    const KEY: &'static str = "runtime.add_context_message";
    const PHASE: awaken_contract::model::Phase = awaken_contract::model::Phase::BeforeInference;
    type Payload = ContextMessage;
}

/// Action spec for per-inference parameter overrides.
///
/// Scheduled by `BeforeInference` hooks via `cmd.schedule_action::<SetInferenceOverride>(...)`.
/// Handled during `run_phase(BeforeInference)` — the handler merges payloads with
/// last-wins semantics per field into [`AccumulatedOverrides`].
pub struct SetInferenceOverride;

impl awaken_contract::model::ScheduledActionSpec for SetInferenceOverride {
    const KEY: &'static str = "runtime.set_inference_override";
    const PHASE: awaken_contract::model::Phase = awaken_contract::model::Phase::BeforeInference;
    type Payload = InferenceOverride;
}

/// Action spec for excluding a specific tool from the current inference step.
///
/// Scheduled by `BeforeInference` hooks via `cmd.schedule_action::<ExcludeTool>(...)`.
/// Handled during `run_phase(BeforeInference)` — the handler accumulates tool IDs
/// into [`AccumulatedToolExclusions`].
pub struct ExcludeTool;

impl awaken_contract::model::ScheduledActionSpec for ExcludeTool {
    const KEY: &'static str = "runtime.exclude_tool";
    const PHASE: awaken_contract::model::Phase = awaken_contract::model::Phase::BeforeInference;
    type Payload = String;
}

/// Action spec for restricting tools to an explicit allow-list for the current inference step.
///
/// Scheduled by `BeforeInference` hooks via `cmd.schedule_action::<IncludeOnlyTools>(...)`.
/// Handled during `run_phase(BeforeInference)` — the handler merges allow-lists
/// into [`AccumulatedToolInclusions`].
pub struct IncludeOnlyTools;

impl awaken_contract::model::ScheduledActionSpec for IncludeOnlyTools {
    const KEY: &'static str = "runtime.include_only_tools";
    const PHASE: awaken_contract::model::Phase = awaken_contract::model::Phase::BeforeInference;
    type Payload = Vec<String>;
}

// ---------------------------------------------------------------------------
// Accumulator state keys — written by action handlers, read by the orchestrator
// ---------------------------------------------------------------------------

/// Accumulated inference overrides for the current step.
///
/// The `SetInferenceOverride` handler merges each payload into this accumulator.
/// The orchestrator reads and clears it after `run_phase(BeforeInference)`.
pub struct AccumulatedOverrides;

/// Update for [`AccumulatedOverrides`].
pub enum AccumulatedOverridesUpdate {
    /// Merge an override (last-wins per field).
    Merge(InferenceOverride),
    /// Clear the accumulator (at step start).
    Clear,
}

impl StateKey for AccumulatedOverrides {
    const KEY: &'static str = "__runtime.accumulated_overrides";
    const MERGE: MergeStrategy = MergeStrategy::Commutative;

    type Value = Option<InferenceOverride>;
    type Update = AccumulatedOverridesUpdate;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        match update {
            AccumulatedOverridesUpdate::Merge(ovr) => {
                if let Some(existing) = value.as_mut() {
                    existing.merge(ovr);
                } else {
                    *value = Some(ovr);
                }
            }
            AccumulatedOverridesUpdate::Clear => {
                *value = None;
            }
        }
    }
}

/// Persistent store for context messages across the agent loop.
///
/// Messages are keyed by their `key` field for upsert semantics.
/// The `AddContextMessage` handler applies throttle logic and upserts accepted
/// messages here. The orchestrator reads messages, injects them, then applies
/// lifecycle rules (removing ephemeral and consume-after-emit messages).
pub struct ContextMessageStore;

/// Durable map of context messages keyed by message key.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ContextMessageStoreValue {
    pub messages: HashMap<String, ContextMessage>,
}

impl ContextMessageStoreValue {
    /// Return all messages sorted by (target, priority, key) for deterministic ordering.
    pub fn sorted_messages(&self) -> Vec<&ContextMessage> {
        let mut sorted: Vec<&ContextMessage> = self.messages.values().collect();
        sorted.sort_by(|a, b| {
            a.target
                .cmp(&b.target)
                .then(a.priority.cmp(&b.priority))
                .then(a.key.cmp(&b.key))
        });
        sorted
    }
}

/// Update actions for [`ContextMessageStore`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContextMessageAction {
    /// Add or update a context message by key.
    Upsert(ContextMessage),
    /// Remove a context message by key.
    Remove(String),
    /// Remove all messages with keys starting with prefix.
    RemoveByPrefix(String),
    /// Remove all non-persistent messages (ephemeral lifecycle cleanup).
    RemoveEphemeral,
    /// Remove all messages flagged `consume_after_emit`.
    ConsumeAfterEmit,
    /// Clear all messages.
    Clear,
}

impl StateKey for ContextMessageStore {
    const KEY: &'static str = "__runtime.context_message_store";
    const MERGE: MergeStrategy = MergeStrategy::Commutative;

    type Value = ContextMessageStoreValue;
    type Update = ContextMessageAction;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        match update {
            ContextMessageAction::Upsert(msg) => {
                value.messages.insert(msg.key.clone(), msg);
            }
            ContextMessageAction::Remove(key) => {
                value.messages.remove(&key);
            }
            ContextMessageAction::RemoveByPrefix(prefix) => {
                value.messages.retain(|k, _| !k.starts_with(&prefix));
            }
            ContextMessageAction::RemoveEphemeral => {
                value.messages.retain(|_, m| m.persistent);
            }
            ContextMessageAction::ConsumeAfterEmit => {
                value.messages.retain(|_, m| !m.consume_after_emit);
            }
            ContextMessageAction::Clear => {
                value.messages.clear();
            }
        }
    }
}

/// Accumulated tool exclusion IDs for the current step.
///
/// The `ExcludeTool` handler pushes each tool ID here.
/// The orchestrator reads and clears after `run_phase(BeforeInference)`.
pub struct AccumulatedToolExclusions;

/// Update for [`AccumulatedToolExclusions`].
pub enum AccumulatedToolExclusionsUpdate {
    /// Add a tool ID to exclude.
    Add(String),
    /// Clear the accumulator (at step start).
    Clear,
}

impl StateKey for AccumulatedToolExclusions {
    const KEY: &'static str = "__runtime.accumulated_tool_exclusions";
    const MERGE: MergeStrategy = MergeStrategy::Commutative;

    type Value = HashSet<String>;
    type Update = AccumulatedToolExclusionsUpdate;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        match update {
            AccumulatedToolExclusionsUpdate::Add(id) => {
                value.insert(id);
            }
            AccumulatedToolExclusionsUpdate::Clear => value.clear(),
        }
    }
}

/// Accumulated tool inclusion allow-list for the current step.
///
/// The `IncludeOnlyTools` handler extends this with each allow-list union.
/// The orchestrator reads and clears after `run_phase(BeforeInference)`.
pub struct AccumulatedToolInclusions;

/// Update for [`AccumulatedToolInclusions`].
pub enum AccumulatedToolInclusionsUpdate {
    /// Extend the allow-list with additional tool IDs.
    Extend(Vec<String>),
    /// Clear the accumulator (at step start).
    Clear,
}

/// The value is `None` when no `IncludeOnlyTools` action has been scheduled,
/// and `Some(set)` when at least one has been processed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInclusionSet(pub Option<HashSet<String>>);

impl StateKey for AccumulatedToolInclusions {
    const KEY: &'static str = "__runtime.accumulated_tool_inclusions";
    const MERGE: MergeStrategy = MergeStrategy::Commutative;

    type Value = ToolInclusionSet;
    type Update = AccumulatedToolInclusionsUpdate;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        match update {
            AccumulatedToolInclusionsUpdate::Extend(ids) => {
                let set = value.0.get_or_insert_with(HashSet::new);
                set.extend(ids);
            }
            AccumulatedToolInclusionsUpdate::Clear => {
                value.0 = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::context_message::ContextMessage as ContractContextMessage;
    use awaken_contract::contract::inference::InferenceOverride;

    // -----------------------------------------------------------------------
    // AccumulatedOverrides tests
    // -----------------------------------------------------------------------

    #[test]
    fn accumulated_overrides_default_is_none() {
        let val: Option<InferenceOverride> = None;
        assert!(val.is_none());
    }

    #[test]
    fn accumulated_overrides_merge_first() {
        let mut val: Option<InferenceOverride> = None;
        AccumulatedOverrides::apply(
            &mut val,
            AccumulatedOverridesUpdate::Merge(InferenceOverride {
                model: Some("gpt-4".into()),
                ..Default::default()
            }),
        );
        assert!(val.is_some());
        assert_eq!(val.as_ref().unwrap().model.as_deref(), Some("gpt-4"));
    }

    #[test]
    fn accumulated_overrides_merge_second_last_wins() {
        let mut val: Option<InferenceOverride> = None;
        AccumulatedOverrides::apply(
            &mut val,
            AccumulatedOverridesUpdate::Merge(InferenceOverride {
                model: Some("gpt-4".into()),
                temperature: Some(0.5),
                ..Default::default()
            }),
        );
        AccumulatedOverrides::apply(
            &mut val,
            AccumulatedOverridesUpdate::Merge(InferenceOverride {
                temperature: Some(0.9),
                max_tokens: Some(1000),
                ..Default::default()
            }),
        );
        let ovr = val.unwrap();
        assert_eq!(ovr.model.as_deref(), Some("gpt-4")); // from first
        assert_eq!(ovr.temperature, Some(0.9)); // last wins
        assert_eq!(ovr.max_tokens, Some(1000)); // from second
    }

    #[test]
    fn accumulated_overrides_clear() {
        let mut val = Some(InferenceOverride {
            model: Some("test".into()),
            ..Default::default()
        });
        AccumulatedOverrides::apply(&mut val, AccumulatedOverridesUpdate::Clear);
        assert!(val.is_none());
    }

    // -----------------------------------------------------------------------
    // ContextMessageStore tests
    // -----------------------------------------------------------------------

    #[test]
    fn context_message_store_upsert() {
        let mut val = ContextMessageStoreValue::default();
        ContextMessageStore::apply(
            &mut val,
            ContextMessageAction::Upsert(ContractContextMessage::system("k1", "msg1")),
        );
        assert_eq!(val.messages.len(), 1);
        assert!(val.messages.contains_key("k1"));
    }

    #[test]
    fn context_message_store_upsert_replaces() {
        let mut val = ContextMessageStoreValue::default();
        ContextMessageStore::apply(
            &mut val,
            ContextMessageAction::Upsert(ContractContextMessage::system("k1", "msg1")),
        );
        ContextMessageStore::apply(
            &mut val,
            ContextMessageAction::Upsert(ContractContextMessage::system("k1", "updated")),
        );
        assert_eq!(val.messages.len(), 1);
        assert_eq!(
            val.messages["k1"].content[0],
            awaken_contract::contract::content::ContentBlock::text("updated")
        );
    }

    #[test]
    fn context_message_store_upsert_multiple() {
        let mut val = ContextMessageStoreValue::default();
        for i in 0..5 {
            ContextMessageStore::apply(
                &mut val,
                ContextMessageAction::Upsert(ContractContextMessage::system(
                    format!("k{i}"),
                    format!("msg{i}"),
                )),
            );
        }
        assert_eq!(val.messages.len(), 5);
    }

    #[test]
    fn context_message_store_remove() {
        let mut val = ContextMessageStoreValue::default();
        ContextMessageStore::apply(
            &mut val,
            ContextMessageAction::Upsert(ContractContextMessage::system("k1", "msg1")),
        );
        ContextMessageStore::apply(
            &mut val,
            ContextMessageAction::Upsert(ContractContextMessage::system("k2", "msg2")),
        );
        ContextMessageStore::apply(&mut val, ContextMessageAction::Remove("k1".into()));
        assert_eq!(val.messages.len(), 1);
        assert!(val.messages.contains_key("k2"));
    }

    #[test]
    fn context_message_store_remove_by_prefix() {
        let mut val = ContextMessageStoreValue::default();
        ContextMessageStore::apply(
            &mut val,
            ContextMessageAction::Upsert(ContractContextMessage::system("mcp:tool1", "t1")),
        );
        ContextMessageStore::apply(
            &mut val,
            ContextMessageAction::Upsert(ContractContextMessage::system("mcp:tool2", "t2")),
        );
        ContextMessageStore::apply(
            &mut val,
            ContextMessageAction::Upsert(ContractContextMessage::system("skill:a", "s1")),
        );
        ContextMessageStore::apply(
            &mut val,
            ContextMessageAction::RemoveByPrefix("mcp:".into()),
        );
        assert_eq!(val.messages.len(), 1);
        assert!(val.messages.contains_key("skill:a"));
    }

    #[test]
    fn context_message_store_remove_ephemeral() {
        let mut val = ContextMessageStoreValue::default();
        ContextMessageStore::apply(
            &mut val,
            ContextMessageAction::Upsert(ContractContextMessage::system("eph", "ephemeral")),
        );
        ContextMessageStore::apply(
            &mut val,
            ContextMessageAction::Upsert(ContractContextMessage::system_persistent(
                "pers",
                "persistent",
            )),
        );
        ContextMessageStore::apply(&mut val, ContextMessageAction::RemoveEphemeral);
        assert_eq!(val.messages.len(), 1);
        assert!(val.messages.contains_key("pers"));
    }

    #[test]
    fn context_message_store_consume_after_emit() {
        let mut val = ContextMessageStoreValue::default();
        ContextMessageStore::apply(
            &mut val,
            ContextMessageAction::Upsert(ContractContextMessage::emit_once(
                "once",
                "once",
                awaken_contract::contract::context_message::ContextMessageTarget::System,
            )),
        );
        ContextMessageStore::apply(
            &mut val,
            ContextMessageAction::Upsert(ContractContextMessage::system_persistent("keep", "keep")),
        );
        ContextMessageStore::apply(&mut val, ContextMessageAction::ConsumeAfterEmit);
        assert_eq!(val.messages.len(), 1);
        assert!(val.messages.contains_key("keep"));
    }

    #[test]
    fn context_message_store_clear() {
        let mut val = ContextMessageStoreValue::default();
        ContextMessageStore::apply(
            &mut val,
            ContextMessageAction::Upsert(ContractContextMessage::system("k1", "msg1")),
        );
        ContextMessageStore::apply(&mut val, ContextMessageAction::Clear);
        assert!(val.messages.is_empty());
    }

    #[test]
    fn context_message_store_sorted_messages() {
        let mut val = ContextMessageStoreValue::default();
        ContextMessageStore::apply(
            &mut val,
            ContextMessageAction::Upsert(
                ContractContextMessage::suffix_system("z_suffix", "last").with_priority(0),
            ),
        );
        ContextMessageStore::apply(
            &mut val,
            ContextMessageAction::Upsert(
                ContractContextMessage::system("a_sys", "first").with_priority(0),
            ),
        );
        ContextMessageStore::apply(
            &mut val,
            ContextMessageAction::Upsert(
                ContractContextMessage::system("b_sys", "second").with_priority(10),
            ),
        );
        let sorted = val.sorted_messages();
        assert_eq!(sorted[0].key, "a_sys");
        assert_eq!(sorted[1].key, "b_sys");
        assert_eq!(sorted[2].key, "z_suffix");
    }

    // -----------------------------------------------------------------------
    // AccumulatedToolExclusions tests
    // -----------------------------------------------------------------------

    #[test]
    fn accumulated_tool_exclusions_add() {
        let mut val = HashSet::new();
        AccumulatedToolExclusions::apply(
            &mut val,
            AccumulatedToolExclusionsUpdate::Add("search".into()),
        );
        assert!(val.contains("search"));
        assert_eq!(val.len(), 1);
    }

    #[test]
    fn accumulated_tool_exclusions_add_deduplicates() {
        let mut val = HashSet::new();
        AccumulatedToolExclusions::apply(
            &mut val,
            AccumulatedToolExclusionsUpdate::Add("search".into()),
        );
        AccumulatedToolExclusions::apply(
            &mut val,
            AccumulatedToolExclusionsUpdate::Add("search".into()),
        );
        assert_eq!(val.len(), 1);
    }

    #[test]
    fn accumulated_tool_exclusions_add_multiple() {
        let mut val = HashSet::new();
        for tool in ["search", "calc", "browser"] {
            AccumulatedToolExclusions::apply(
                &mut val,
                AccumulatedToolExclusionsUpdate::Add(tool.into()),
            );
        }
        assert_eq!(val.len(), 3);
    }

    #[test]
    fn accumulated_tool_exclusions_clear() {
        let mut val: HashSet<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        AccumulatedToolExclusions::apply(&mut val, AccumulatedToolExclusionsUpdate::Clear);
        assert!(val.is_empty());
    }

    // -----------------------------------------------------------------------
    // AccumulatedToolInclusions tests
    // -----------------------------------------------------------------------

    #[test]
    fn accumulated_tool_inclusions_default_is_none() {
        let val = ToolInclusionSet::default();
        assert!(val.0.is_none());
    }

    #[test]
    fn accumulated_tool_inclusions_extend_creates_set() {
        let mut val = ToolInclusionSet::default();
        AccumulatedToolInclusions::apply(
            &mut val,
            AccumulatedToolInclusionsUpdate::Extend(vec!["search".into(), "calc".into()]),
        );
        assert!(val.0.is_some());
        let set = val.0.as_ref().unwrap();
        assert_eq!(set.len(), 2);
        assert!(set.contains("search"));
        assert!(set.contains("calc"));
    }

    #[test]
    fn accumulated_tool_inclusions_extend_merges() {
        let mut val = ToolInclusionSet::default();
        AccumulatedToolInclusions::apply(
            &mut val,
            AccumulatedToolInclusionsUpdate::Extend(vec!["a".into()]),
        );
        AccumulatedToolInclusions::apply(
            &mut val,
            AccumulatedToolInclusionsUpdate::Extend(vec!["b".into(), "c".into()]),
        );
        let set = val.0.as_ref().unwrap();
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn accumulated_tool_inclusions_extend_deduplicates() {
        let mut val = ToolInclusionSet::default();
        AccumulatedToolInclusions::apply(
            &mut val,
            AccumulatedToolInclusionsUpdate::Extend(vec!["a".into()]),
        );
        AccumulatedToolInclusions::apply(
            &mut val,
            AccumulatedToolInclusionsUpdate::Extend(vec!["a".into(), "b".into()]),
        );
        let set = val.0.as_ref().unwrap();
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn accumulated_tool_inclusions_clear() {
        let mut val = ToolInclusionSet(Some(["x", "y"].iter().map(|s| s.to_string()).collect()));
        AccumulatedToolInclusions::apply(&mut val, AccumulatedToolInclusionsUpdate::Clear);
        assert!(val.0.is_none());
    }

    #[test]
    fn accumulated_tool_inclusions_serde_roundtrip() {
        let val = ToolInclusionSet(Some(
            ["search", "calc"].iter().map(|s| s.to_string()).collect(),
        ));
        let json = serde_json::to_string(&val).unwrap();
        let parsed: ToolInclusionSet = serde_json::from_str(&json).unwrap();
        assert_eq!(val, parsed);
    }

    #[test]
    fn accumulated_tool_inclusions_empty_extend() {
        let mut val = ToolInclusionSet::default();
        AccumulatedToolInclusions::apply(&mut val, AccumulatedToolInclusionsUpdate::Extend(vec![]));
        // Should create a Some(empty set), not remain None
        assert!(val.0.is_some());
        assert!(val.0.as_ref().unwrap().is_empty());
    }
}
