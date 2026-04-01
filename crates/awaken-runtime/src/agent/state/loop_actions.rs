use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::state::{MergeStrategy, StateKey};
use awaken_contract::contract::context_message::ContextMessage;
use awaken_contract::contract::inference::InferenceOverride;
use awaken_contract::contract::tool_intercept::ToolInterceptPayload;

// ---------------------------------------------------------------------------
// Action specs
// ---------------------------------------------------------------------------

/// Action spec for injecting a context message into the prompt.
///
/// Scheduled by `BeforeInference` hooks via `cmd.schedule_action::<AddContextMessage>(...)`.
/// Handled during EXECUTE by `ContextMessageHandler` which applies throttle logic
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
/// Handled during EXECUTE by `SetInferenceOverrideHandler` which writes to [`InferenceOverrideState`].
pub struct SetInferenceOverride;

impl awaken_contract::model::ScheduledActionSpec for SetInferenceOverride {
    const KEY: &'static str = "runtime.set_inference_override";
    const PHASE: awaken_contract::model::Phase = awaken_contract::model::Phase::BeforeInference;
    type Payload = InferenceOverride;
}

/// Action spec for excluding a specific tool from the current inference step.
///
/// Scheduled by `BeforeInference` hooks via `cmd.schedule_action::<ExcludeTool>(...)`.
/// Handled during EXECUTE by `ExcludeToolHandler` which writes to [`ToolFilterState`].
pub struct ExcludeTool;

impl awaken_contract::model::ScheduledActionSpec for ExcludeTool {
    const KEY: &'static str = "runtime.exclude_tool";
    const PHASE: awaken_contract::model::Phase = awaken_contract::model::Phase::BeforeInference;
    type Payload = String;
}

/// Action spec for restricting tools to an explicit allow-list for the current inference step.
///
/// Scheduled by `BeforeInference` hooks via `cmd.schedule_action::<IncludeOnlyTools>(...)`.
/// Handled during EXECUTE by `IncludeOnlyToolsHandler` which writes to [`ToolFilterState`].
pub struct IncludeOnlyTools;

impl awaken_contract::model::ScheduledActionSpec for IncludeOnlyTools {
    const KEY: &'static str = "runtime.include_only_tools";
    const PHASE: awaken_contract::model::Phase = awaken_contract::model::Phase::BeforeInference;
    type Payload = Vec<String>;
}

// ---------------------------------------------------------------------------
// Persistent state keys (not accumulators)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Accumulator state keys (written by handlers, read/cleared by orchestrator)
// ---------------------------------------------------------------------------

/// Accumulated tool filter state for the current inference step.
/// Written by `ExcludeTool` and `IncludeOnlyTools` handlers.
/// Read and cleared by the orchestrator after the EXECUTE loop.
pub struct ToolFilterState;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolFilterStateValue {
    pub excluded: Vec<String>,
    pub include_only: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolFilterStateAction {
    Exclude(String),
    IncludeOnly(Vec<String>),
    Clear,
}

impl StateKey for ToolFilterState {
    const KEY: &'static str = "__runtime.tool_filter_state";
    const MERGE: MergeStrategy = MergeStrategy::Commutative;
    type Value = ToolFilterStateValue;
    type Update = ToolFilterStateAction;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        match update {
            ToolFilterStateAction::Exclude(id) => value.excluded.push(id),
            ToolFilterStateAction::IncludeOnly(ids) => value.include_only.push(ids),
            ToolFilterStateAction::Clear => {
                value.excluded.clear();
                value.include_only.clear();
            }
        }
    }
}

/// Accumulated inference override for the current step.
/// Written by `SetInferenceOverride` handler. Read and cleared by orchestrator.
pub struct InferenceOverrideState;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InferenceOverrideStateValue {
    pub overrides: Option<InferenceOverride>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InferenceOverrideStateAction {
    Merge(InferenceOverride),
    Clear,
}

impl StateKey for InferenceOverrideState {
    const KEY: &'static str = "__runtime.inference_override_state";
    const MERGE: MergeStrategy = MergeStrategy::Commutative;
    type Value = InferenceOverrideStateValue;
    type Update = InferenceOverrideStateAction;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        match update {
            InferenceOverrideStateAction::Merge(ovr) => {
                if let Some(existing) = value.overrides.as_mut() {
                    existing.merge(ovr);
                } else {
                    value.overrides = Some(ovr);
                }
            }
            InferenceOverrideStateAction::Clear => {
                value.overrides = None;
            }
        }
    }
}

/// Accumulated tool intercept decisions for the current tool call.
/// Written by `ToolInterceptAction` handler. Read and cleared by orchestrator.
pub struct ToolInterceptState;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolInterceptStateValue {
    pub payloads: Vec<ToolInterceptPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolInterceptStateAction {
    Push(Box<ToolInterceptPayload>),
    Clear,
}

impl StateKey for ToolInterceptState {
    const KEY: &'static str = "__runtime.tool_intercept_state";
    const MERGE: MergeStrategy = MergeStrategy::Commutative;
    type Value = ToolInterceptStateValue;
    type Update = ToolInterceptStateAction;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        match update {
            ToolInterceptStateAction::Push(payload) => value.payloads.push(*payload),
            ToolInterceptStateAction::Clear => value.payloads.clear(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::context_message::ContextMessage as ContractContextMessage;

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
}
