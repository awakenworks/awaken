//! Action handlers and helpers for loop-consumed actions.
//!
//! Each action type has a handler that runs during `run_phase(BeforeInference)`,
//! writing results to accumulator state keys. The orchestrator reads these
//! accumulators after the phase to build the inference request.

use async_trait::async_trait;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::runtime::{PhaseContext, TypedScheduledActionHandler};
use crate::state::StateCommand;
use awaken_contract::StateError;
use awaken_contract::contract::context_message::ContextMessage;
use awaken_contract::contract::message::{Message, Role};

use super::super::state::{
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
