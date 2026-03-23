//! Action handlers and helpers for loop-consumed actions.
//!
//! Each action type has a handler that runs during `run_phase(BeforeInference)`,
//! writing results to accumulator state keys. The orchestrator reads these
//! accumulators after the phase to build the inference request.

use async_trait::async_trait;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use super::super::{PhaseContext, TypedScheduledActionHandler};
use crate::state::StateCommand;
use awaken_contract::StateError;
use awaken_contract::contract::context_message::ContextMessage;
use awaken_contract::contract::message::{Message, Role};

use crate::agent::state::{
    AccumulatedOverrides, AccumulatedOverridesUpdate, AccumulatedToolExclusions,
    AccumulatedToolExclusionsUpdate, AccumulatedToolInclusions, AccumulatedToolInclusionsUpdate,
    AddContextMessage, ContextMessageAction, ContextMessageStore, ContextThrottleState,
    ContextThrottleUpdate, ExcludeTool, IncludeOnlyTools, RunLifecycle, SetInferenceOverride,
};

// ---------------------------------------------------------------------------
// Action handlers
// ---------------------------------------------------------------------------

/// Handler for `SetInferenceOverride` — merges overrides into [`AccumulatedOverrides`].
pub(super) struct InferenceOverrideHandler;

#[async_trait]
impl TypedScheduledActionHandler<SetInferenceOverride> for InferenceOverrideHandler {
    async fn handle_typed(
        &self,
        _ctx: &PhaseContext,
        payload: awaken_contract::contract::inference::InferenceOverride,
    ) -> Result<StateCommand, StateError> {
        let mut cmd = StateCommand::new();
        cmd.update::<AccumulatedOverrides>(AccumulatedOverridesUpdate::Merge(payload));
        Ok(cmd)
    }
}

/// Handler for `AddContextMessage` — applies throttle logic, upserts accepted
/// messages into [`ContextMessageStore`], updates [`ContextThrottleState`].
pub(super) struct ContextMessageHandler;

#[async_trait]
impl TypedScheduledActionHandler<AddContextMessage> for ContextMessageHandler {
    async fn handle_typed(
        &self,
        ctx: &PhaseContext,
        payload: ContextMessage,
    ) -> Result<StateCommand, StateError> {
        let mut cmd = StateCommand::new();

        // Determine current step from RunLifecycle.step_count + 1
        // (step_count records completed steps; current step is one ahead)
        let current_step = ctx
            .snapshot
            .get::<RunLifecycle>()
            .map(|s| s.step_count as usize + 1)
            .unwrap_or(1);

        let content_hash = {
            let mut hasher = DefaultHasher::new();
            if let Ok(json) = serde_json::to_string(&payload.content) {
                json.hash(&mut hasher);
            }
            hasher.finish()
        };

        let should_inject = if payload.cooldown_turns == 0 {
            true
        } else {
            let throttle_state = ctx
                .snapshot
                .get::<ContextThrottleState>()
                .cloned()
                .unwrap_or_default();
            match throttle_state.entries.get(&payload.key) {
                None => true,
                Some(entry) => {
                    entry.content_hash != content_hash
                        || current_step.saturating_sub(entry.last_step)
                            >= payload.cooldown_turns as usize
                }
            }
        };

        if should_inject {
            cmd.update::<ContextThrottleState>(ContextThrottleUpdate::Injected {
                key: payload.key.clone(),
                step: current_step,
                content_hash,
            });
            cmd.update::<ContextMessageStore>(ContextMessageAction::Upsert(payload));
        }

        Ok(cmd)
    }
}

/// Handler for `ExcludeTool` — adds the tool ID to [`AccumulatedToolExclusions`].
pub(super) struct ExcludeToolHandler;

#[async_trait]
impl TypedScheduledActionHandler<ExcludeTool> for ExcludeToolHandler {
    async fn handle_typed(
        &self,
        _ctx: &PhaseContext,
        payload: String,
    ) -> Result<StateCommand, StateError> {
        let mut cmd = StateCommand::new();
        cmd.update::<AccumulatedToolExclusions>(AccumulatedToolExclusionsUpdate::Add(payload));
        Ok(cmd)
    }
}

/// Handler for `IncludeOnlyTools` — extends [`AccumulatedToolInclusions`] with the allow-list.
pub(super) struct IncludeOnlyToolsHandler;

#[async_trait]
impl TypedScheduledActionHandler<IncludeOnlyTools> for IncludeOnlyToolsHandler {
    async fn handle_typed(
        &self,
        _ctx: &PhaseContext,
        payload: Vec<String>,
    ) -> Result<StateCommand, StateError> {
        let mut cmd = StateCommand::new();
        cmd.update::<AccumulatedToolInclusions>(AccumulatedToolInclusionsUpdate::Extend(payload));
        Ok(cmd)
    }
}

// ---------------------------------------------------------------------------
// Plugin for registering action handlers
// ---------------------------------------------------------------------------

/// Internal plugin that registers handlers for the four loop action types.
///
/// Added to the `ExecutionEnv` plugins list in `build_agent_env` and
/// `RegistrySet::resolve` so that these actions are processed during
/// `run_phase(BeforeInference)` like any other handler-based action.
pub(crate) struct LoopActionHandlersPlugin;

impl crate::plugins::Plugin for LoopActionHandlersPlugin {
    fn descriptor(&self) -> crate::plugins::PluginDescriptor {
        crate::plugins::PluginDescriptor {
            name: "__loop_action_handlers",
        }
    }

    fn register(
        &self,
        r: &mut crate::plugins::PluginRegistrar,
    ) -> Result<(), awaken_contract::StateError> {
        r.register_scheduled_action::<SetInferenceOverride, _>(InferenceOverrideHandler)?;
        r.register_scheduled_action::<AddContextMessage, _>(ContextMessageHandler)?;
        r.register_scheduled_action::<ExcludeTool, _>(ExcludeToolHandler)?;
        r.register_scheduled_action::<IncludeOnlyTools, _>(IncludeOnlyToolsHandler)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Orchestrator helpers — read accumulators after run_phase(BeforeInference)
// ---------------------------------------------------------------------------

/// Read and clear accumulated inference overrides from the state store.
pub(super) fn take_accumulated_overrides(
    store: &crate::state::StateStore,
) -> Result<Option<awaken_contract::contract::inference::InferenceOverride>, StateError> {
    let result = store.read::<AccumulatedOverrides>().flatten();
    if result.is_some() {
        let mut patch = crate::state::MutationBatch::new();
        patch.update::<AccumulatedOverrides>(AccumulatedOverridesUpdate::Clear);
        store.commit(patch)?;
    }
    Ok(result)
}

/// Read context messages from the store, return sorted list, then apply lifecycle cleanup.
///
/// Lifecycle rules applied after injection:
/// - Non-persistent (ephemeral) messages are removed.
/// - Messages with `consume_after_emit` are removed.
/// - Persistent messages remain for subsequent steps.
pub(super) fn take_context_messages(
    store: &crate::state::StateStore,
) -> Result<Vec<ContextMessage>, StateError> {
    let store_value = store.read::<ContextMessageStore>().unwrap_or_default();

    if store_value.messages.is_empty() {
        return Ok(Vec::new());
    }

    // Collect all messages sorted by (target, priority, key)
    let result: Vec<ContextMessage> = store_value.sorted_messages().into_iter().cloned().collect();

    // Apply lifecycle: remove ephemeral + consume-after-emit
    let mut patch = crate::state::MutationBatch::new();
    patch.update::<ContextMessageStore>(ContextMessageAction::RemoveEphemeral);
    patch.update::<ContextMessageStore>(ContextMessageAction::ConsumeAfterEmit);
    store.commit(patch)?;

    Ok(result)
}

/// Read and clear accumulated tool filters, then apply them to the tool list.
///
/// - If any `IncludeOnlyTools` actions were processed, only tools in the combined
///   allow-list are kept.
/// - Then any `ExcludeTool` tool IDs are removed.
pub(super) fn take_and_apply_tool_filters(
    store: &crate::state::StateStore,
    tools: &mut Vec<awaken_contract::contract::tool::ToolDescriptor>,
) -> Result<(), StateError> {
    let exclusions = store
        .read::<AccumulatedToolExclusions>()
        .unwrap_or_default();
    let inclusions = store
        .read::<AccumulatedToolInclusions>()
        .unwrap_or_default();

    let has_filters = !exclusions.is_empty() || inclusions.0.is_some();

    if has_filters {
        let mut patch = crate::state::MutationBatch::new();
        patch.update::<AccumulatedToolExclusions>(AccumulatedToolExclusionsUpdate::Clear);
        patch.update::<AccumulatedToolInclusions>(AccumulatedToolInclusionsUpdate::Clear);
        store.commit(patch)?;
    }

    // Apply include-only filter first
    if let Some(ref allowed) = inclusions.0 {
        tools.retain(|t| allowed.contains(&t.id));
    }

    // Apply exclusions
    if !exclusions.is_empty() {
        tools.retain(|t| !exclusions.contains(&t.id));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Message placement (unchanged)
// ---------------------------------------------------------------------------

/// Insert context messages into the message list at their declared target positions.
pub(super) fn apply_context_messages(
    messages: &mut Vec<Message>,
    context_messages: Vec<ContextMessage>,
    has_system_prompt: bool,
) {
    use awaken_contract::contract::context_message::ContextMessageTarget;

    let mut system = Vec::new();
    let mut session = Vec::new();
    let mut conversation = Vec::new();
    let mut suffix = Vec::new();

    for entry in context_messages {
        let msg = Message {
            id: Some(awaken_contract::contract::message::gen_message_id()),
            role: entry.role,
            content: entry.content,
            tool_calls: None,
            tool_call_id: None,
            visibility: entry.visibility,
            metadata: None,
        };
        match entry.target {
            ContextMessageTarget::System => system.push(msg),
            ContextMessageTarget::Session => session.push(msg),
            ContextMessageTarget::Conversation => conversation.push(msg),
            ContextMessageTarget::SuffixSystem => suffix.push(msg),
        }
    }

    // System: insert after base system prompt
    let system_insert_pos = usize::from(has_system_prompt);
    for (offset, msg) in system.into_iter().enumerate() {
        messages.insert(system_insert_pos + offset, msg);
    }

    // Session: insert after all system-role messages
    let session_insert_pos = messages
        .iter()
        .take_while(|m| m.role == Role::System)
        .count();
    for (offset, msg) in session.into_iter().enumerate() {
        messages.insert(session_insert_pos + offset, msg);
    }

    // Conversation: insert after system messages, before history
    let conversation_insert_pos = messages
        .iter()
        .take_while(|m| m.role == Role::System)
        .count();
    for (offset, msg) in conversation.into_iter().enumerate() {
        messages.insert(conversation_insert_pos + offset, msg);
    }

    // Suffix: append at end
    messages.extend(suffix);
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::context_message::ContextMessage;

    // ---- apply_context_messages ----

    #[test]
    fn apply_context_messages_empty_input() {
        let mut messages = vec![Message::system("sys prompt"), Message::user("hello")];
        apply_context_messages(&mut messages, vec![], true);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text(), "sys prompt");
        assert_eq!(messages[1].text(), "hello");
    }

    #[test]
    fn apply_context_messages_system_target() {
        let mut messages = vec![
            Message::system("base system"),
            Message::user("hello"),
            Message::assistant("hi"),
        ];
        let ctx_msgs = vec![ContextMessage::system("test.key", "injected system")];
        apply_context_messages(&mut messages, ctx_msgs, true);

        // System context should be inserted after the base system prompt (index 1)
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].text(), "base system");
        assert_eq!(messages[1].text(), "injected system");
        assert_eq!(messages[1].role, Role::System);
        assert_eq!(messages[2].text(), "hello");
    }

    #[test]
    fn apply_context_messages_system_target_no_system_prompt() {
        let mut messages = vec![Message::user("hello"), Message::assistant("hi")];
        let ctx_msgs = vec![ContextMessage::system("test.key", "injected")];
        apply_context_messages(&mut messages, ctx_msgs, false);

        // Without system prompt, insert at position 0
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].text(), "injected");
        assert_eq!(messages[1].text(), "hello");
    }

    #[test]
    fn apply_context_messages_suffix_target() {
        let mut messages = vec![
            Message::system("sys"),
            Message::user("hello"),
            Message::assistant("hi"),
        ];
        let ctx_msgs = vec![ContextMessage::suffix_system(
            "suffix.key",
            "suffix content",
        )];
        apply_context_messages(&mut messages, ctx_msgs, true);

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[3].text(), "suffix content");
    }

    #[test]
    fn apply_context_messages_session_target() {
        let mut messages = vec![Message::system("sys"), Message::user("hello")];
        let ctx_msgs = vec![ContextMessage::session(
            "session.key",
            Role::System,
            "session context",
        )];
        apply_context_messages(&mut messages, ctx_msgs, true);

        // Session: after all system-role messages. After injecting a system context_msg,
        // the system count changes. The session-target message goes after system messages.
        assert_eq!(messages.len(), 3);
        // The session msg is inserted after the system prompt
        let system_count = messages.iter().filter(|m| m.role == Role::System).count();
        assert!(system_count >= 2); // base system + session context
    }

    #[test]
    fn apply_context_messages_conversation_target() {
        let mut messages = vec![
            Message::system("sys"),
            Message::user("hello"),
            Message::assistant("hi"),
        ];
        let ctx_msgs = vec![ContextMessage::conversation(
            "conv.key",
            Role::User,
            "conversation context",
        )];
        apply_context_messages(&mut messages, ctx_msgs, true);

        assert_eq!(messages.len(), 4);
        // Conversation messages are inserted after system messages, before history
        assert_eq!(messages[0].role, Role::System);
    }

    #[test]
    fn apply_context_messages_multiple_targets() {
        let mut messages = vec![
            Message::system("sys"),
            Message::user("hello"),
            Message::assistant("hi"),
        ];
        let ctx_msgs = vec![
            ContextMessage::system("sys.key", "system inject"),
            ContextMessage::suffix_system("suffix.key", "suffix inject"),
        ];
        apply_context_messages(&mut messages, ctx_msgs, true);

        assert_eq!(messages.len(), 5);
        // System inject should be near the beginning
        assert_eq!(messages[1].text(), "system inject");
        // Suffix inject should be at the end
        assert_eq!(messages[4].text(), "suffix inject");
    }

    #[test]
    fn apply_context_messages_ordering_preserved_within_target() {
        let mut messages = vec![Message::system("sys"), Message::user("hello")];
        let ctx_msgs = vec![
            ContextMessage::system("a", "first system"),
            ContextMessage::system("b", "second system"),
        ];
        apply_context_messages(&mut messages, ctx_msgs, true);

        assert_eq!(messages[1].text(), "first system");
        assert_eq!(messages[2].text(), "second system");
    }

    #[test]
    fn apply_context_messages_empty_messages_list() {
        let mut messages: Vec<Message> = vec![];
        let ctx_msgs = vec![ContextMessage::system("key", "inject")];
        apply_context_messages(&mut messages, ctx_msgs, false);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text(), "inject");
    }

    #[test]
    fn apply_context_messages_suffix_with_empty_messages() {
        let mut messages: Vec<Message> = vec![];
        let ctx_msgs = vec![ContextMessage::suffix_system("key", "suffix")];
        apply_context_messages(&mut messages, ctx_msgs, false);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text(), "suffix");
    }
}
