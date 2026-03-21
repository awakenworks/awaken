use std::hash::{Hash, Hasher};
use tirea_contract::runtime::inference::{
    clear_prompt_segment_namespace_action, upsert_prompt_segment_action,
    PromptSegmentConsumePolicy, StoredPromptSegment,
};
use tirea_contract::runtime::phase::{ActionSet, BeforeInferenceAction};

/// Create a state action that adds a reminder item.
pub fn add_reminder_action(
    text: impl Into<String>,
) -> tirea_contract::runtime::state::AnyStateAction {
    reminder_action(text.into(), PromptSegmentConsumePolicy::AfterEmit)
}

/// Create a state action that adds a persistent reminder item.
pub fn add_persistent_reminder_action(
    text: impl Into<String>,
) -> tirea_contract::runtime::state::AnyStateAction {
    reminder_action(text.into(), PromptSegmentConsumePolicy::Persistent)
}

fn reminder_key(text: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    format!("reminder:{:016x}", hasher.finish())
}

fn reminder_action(
    text: String,
    consume: PromptSegmentConsumePolicy,
) -> tirea_contract::runtime::state::AnyStateAction {
    upsert_prompt_segment_action(
        StoredPromptSegment::new("reminder", reminder_key(&text), format!("Reminder: {text}"))
            .with_target(tirea_contract::runtime::inference::ContextMessageTarget::Session)
            .with_consume_policy(consume),
    )
}

/// Inject reminder texts as session-context prompt segments.
pub fn inject_reminders(texts: Vec<String>) -> ActionSet<BeforeInferenceAction> {
    texts
        .into_iter()
        .map(|text| {
            BeforeInferenceAction::AddContextMessage(
                tirea_contract::runtime::inference::ContextMessage::session(
                    reminder_key(&text),
                    format!("Reminder: {text}"),
                ),
            )
        })
        .collect::<Vec<_>>()
        .into()
}

/// Create a state action that clears reminder state.
pub fn clear_reminder_action() -> tirea_contract::runtime::state::AnyStateAction {
    clear_prompt_segment_namespace_action("reminder")
}
