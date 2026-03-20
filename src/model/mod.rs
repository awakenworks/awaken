mod action;
mod codec;
mod effect;
mod phase;

pub use action::{
    FailedScheduledAction, FailedScheduledActionUpdate, FailedScheduledActions,
    PendingScheduledActions, ScheduledAction, ScheduledActionEnvelope, ScheduledActionLog,
    ScheduledActionLogEntry, ScheduledActionLogUpdate, ScheduledActionQueueUpdate,
    ScheduledActionSpec,
};
pub use codec::{JsonValue, decode_json, encode_json};
pub use effect::{
    EffectLog, EffectLogEntry, EffectLogUpdate, EffectSpec, RuntimeEffect, TypedEffect,
};
pub use phase::Phase;

/// Declare an append-only log `StateSlot` with `Append`, `TrimToLast`, and `Clear` operations.
///
/// Generates: `$update` enum, `$slot` struct, and the `StateSlot` impl.
macro_rules! define_log_slot {
    (
        slot = $slot:ident,
        update = $update:ident,
        entry = $entry:ty,
        key = $key:expr $(,)?
    ) => {
        #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
        #[serde(tag = "op", rename_all = "snake_case")]
        pub enum $update {
            Append($entry),
            TrimToLast { keep: usize },
            Clear,
        }

        pub struct $slot;

        impl $crate::state::StateSlot for $slot {
            const KEY: &'static str = $key;

            type Value = Vec<$entry>;
            type Update = $update;

            fn apply(value: &mut Self::Value, update: Self::Update) {
                match update {
                    $update::Append(entry) => value.push(entry),
                    $update::TrimToLast { keep } => $crate::model::trim_to_last(value, keep),
                    $update::Clear => value.clear(),
                }
            }
        }
    };
}
pub(crate) use define_log_slot;

/// Trim a vector to keep only the last `keep` elements.
pub(crate) fn trim_to_last<T>(value: &mut Vec<T>, keep: usize) {
    if keep == 0 {
        value.clear();
        return;
    }

    if value.len() > keep {
        let drop_count = value.len() - keep;
        value.drain(0..drop_count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StateSlot;

    #[test]
    fn shared_trim_to_last_keeps_tail() {
        let mut v = vec![10, 20, 30, 40, 50];
        trim_to_last(&mut v, 3);
        assert_eq!(v, vec![30, 40, 50]);
    }

    #[test]
    fn shared_trim_to_last_zero_clears() {
        let mut v = vec![1, 2, 3];
        trim_to_last(&mut v, 0);
        assert!(v.is_empty());
    }

    #[test]
    fn shared_trim_to_last_noop_when_within_limit() {
        let mut v = vec![1, 2];
        trim_to_last(&mut v, 5);
        assert_eq!(v, vec![1, 2]);
    }

    #[test]
    fn effect_log_and_action_log_use_same_trim() {
        // Confirm both log slots route through the shared trim_to_last
        let mut effect_log: Vec<EffectLogEntry> = (0..5)
            .map(|i| EffectLogEntry {
                id: i,
                key: format!("e{i}"),
            })
            .collect();
        let mut action_log: Vec<ScheduledActionLogEntry> = (0..5)
            .map(|i| ScheduledActionLogEntry {
                id: i,
                phase: Phase::RunStart,
                key: format!("a{i}"),
            })
            .collect();

        // Apply via StateSlot::apply to prove both use shared trim
        EffectLog::apply(&mut effect_log, EffectLogUpdate::TrimToLast { keep: 2 });
        ScheduledActionLog::apply(
            &mut action_log,
            ScheduledActionLogUpdate::TrimToLast { keep: 2 },
        );

        assert_eq!(effect_log.len(), 2);
        assert_eq!(effect_log[0].id, 3);
        assert_eq!(action_log.len(), 2);
        assert_eq!(action_log[0].id, 3);
    }
}
