use crate::state::StateKey;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Per-key throttle entry: tracks when a context message was last injected.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThrottleEntry {
    /// Step number when this key was last injected.
    pub last_step: usize,
    /// Hash of the content at last injection (re-inject if content changes).
    pub content_hash: u64,
}

/// Throttle state for context message injection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextThrottleMap {
    pub entries: HashMap<String, ThrottleEntry>,
}

/// Update for the context throttle state.
pub enum ContextThrottleUpdate {
    /// Record that a key was injected at a given step with a content hash.
    Injected {
        key: String,
        step: usize,
        content_hash: u64,
    },
}

/// State key for context message throttle tracking.
///
/// Tracks per-key injection history so the loop runner can enforce cooldown rules.
pub struct ContextThrottleState;

impl StateKey for ContextThrottleState {
    const KEY: &'static str = "__runtime.context_throttle";

    type Value = ContextThrottleMap;
    type Update = ContextThrottleUpdate;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        match update {
            ContextThrottleUpdate::Injected {
                key,
                step,
                content_hash,
            } => {
                value.entries.insert(
                    key,
                    ThrottleEntry {
                        last_step: step,
                        content_hash,
                    },
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttle_map_default_is_empty() {
        let map = ContextThrottleMap::default();
        assert!(map.entries.is_empty());
    }

    #[test]
    fn injected_creates_entry() {
        let mut map = ContextThrottleMap::default();
        ContextThrottleState::apply(
            &mut map,
            ContextThrottleUpdate::Injected {
                key: "reminder".into(),
                step: 5,
                content_hash: 12345,
            },
        );
        assert_eq!(map.entries.len(), 1);
        let entry = &map.entries["reminder"];
        assert_eq!(entry.last_step, 5);
        assert_eq!(entry.content_hash, 12345);
    }

    #[test]
    fn injected_updates_existing_entry() {
        let mut map = ContextThrottleMap::default();
        ContextThrottleState::apply(
            &mut map,
            ContextThrottleUpdate::Injected {
                key: "reminder".into(),
                step: 1,
                content_hash: 111,
            },
        );
        ContextThrottleState::apply(
            &mut map,
            ContextThrottleUpdate::Injected {
                key: "reminder".into(),
                step: 5,
                content_hash: 222,
            },
        );
        assert_eq!(map.entries.len(), 1);
        let entry = &map.entries["reminder"];
        assert_eq!(entry.last_step, 5);
        assert_eq!(entry.content_hash, 222);
    }

    #[test]
    fn injected_multiple_keys_independent() {
        let mut map = ContextThrottleMap::default();
        ContextThrottleState::apply(
            &mut map,
            ContextThrottleUpdate::Injected {
                key: "a".into(),
                step: 1,
                content_hash: 100,
            },
        );
        ContextThrottleState::apply(
            &mut map,
            ContextThrottleUpdate::Injected {
                key: "b".into(),
                step: 2,
                content_hash: 200,
            },
        );
        assert_eq!(map.entries.len(), 2);
        assert_eq!(map.entries["a"].last_step, 1);
        assert_eq!(map.entries["b"].last_step, 2);
    }

    #[test]
    fn throttle_entry_serde_roundtrip() {
        let entry = ThrottleEntry {
            last_step: 42,
            content_hash: 987654321,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: ThrottleEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, entry);
    }

    #[test]
    fn throttle_map_serde_roundtrip() {
        let mut map = ContextThrottleMap::default();
        ContextThrottleState::apply(
            &mut map,
            ContextThrottleUpdate::Injected {
                key: "k1".into(),
                step: 3,
                content_hash: 456,
            },
        );
        let json = serde_json::to_string(&map).unwrap();
        let parsed: ContextThrottleMap = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, map);
    }

    #[test]
    fn content_hash_change_detection() {
        let mut map = ContextThrottleMap::default();

        // First injection
        ContextThrottleState::apply(
            &mut map,
            ContextThrottleUpdate::Injected {
                key: "reminder".into(),
                step: 1,
                content_hash: 111,
            },
        );

        // Check if content changed (simulating throttle logic)
        let entry = &map.entries["reminder"];
        let new_hash: u64 = 222;
        let content_changed = entry.content_hash != new_hash;
        assert!(content_changed, "different hash should indicate change");

        // Same hash
        let same_hash: u64 = 111;
        let content_same = entry.content_hash == same_hash;
        assert!(content_same, "same hash should indicate no change");
    }

    #[test]
    fn cooldown_check_logic() {
        let mut map = ContextThrottleMap::default();

        // Inject at step 1 with cooldown of 3
        ContextThrottleState::apply(
            &mut map,
            ContextThrottleUpdate::Injected {
                key: "reminder".into(),
                step: 1,
                content_hash: 100,
            },
        );

        let entry = &map.entries["reminder"];
        let cooldown = 3usize;

        // Step 2: within cooldown (2 - 1 = 1 < 3)
        assert!(2usize.saturating_sub(entry.last_step) < cooldown);

        // Step 3: within cooldown (3 - 1 = 2 < 3)
        assert!(3usize.saturating_sub(entry.last_step) < cooldown);

        // Step 4: cooldown expired (4 - 1 = 3 >= 3)
        assert!(4usize.saturating_sub(entry.last_step) >= cooldown);
    }
}
