use std::collections::HashSet;

use awaken_contract::state::{KeyScope, MergeStrategy, StateKey};
use serde::{Deserialize, Serialize};

/// Persisted skill state tracking which skills are active.
///
/// Uses a grow-only set (commutative merge) so parallel tool executions
/// that activate different skills can merge without conflict.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillStateValue {
    /// Activated skill IDs.
    #[serde(default)]
    pub active: HashSet<String>,
}

/// Update type for skill state.
#[derive(Debug)]
pub enum SkillStateUpdate {
    /// Mark a skill as activated (insert into the set).
    Activate(String),
}

/// State key for tracking active skills.
pub struct SkillState;

impl StateKey for SkillState {
    const KEY: &'static str = "skills";
    const MERGE: MergeStrategy = MergeStrategy::Commutative;
    const SCOPE: KeyScope = KeyScope::Thread;

    type Value = SkillStateValue;
    type Update = SkillStateUpdate;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        match update {
            SkillStateUpdate::Activate(id) => {
                value.active.insert(id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activate_adds_skill_to_active_set() {
        let mut state = SkillStateValue::default();
        SkillState::apply(&mut state, SkillStateUpdate::Activate("s1".to_string()));
        assert!(state.active.contains("s1"));
    }

    #[test]
    fn activate_is_idempotent() {
        let mut state = SkillStateValue::default();
        SkillState::apply(&mut state, SkillStateUpdate::Activate("s1".to_string()));
        SkillState::apply(&mut state, SkillStateUpdate::Activate("s1".to_string()));
        assert_eq!(state.active.len(), 1);
    }

    #[test]
    fn multiple_activations_grow_set() {
        let mut state = SkillStateValue::default();
        SkillState::apply(&mut state, SkillStateUpdate::Activate("s1".to_string()));
        SkillState::apply(&mut state, SkillStateUpdate::Activate("s2".to_string()));
        assert_eq!(state.active.len(), 2);
        assert!(state.active.contains("s1"));
        assert!(state.active.contains("s2"));
    }

    #[test]
    fn state_key_constants() {
        assert_eq!(SkillState::KEY, "skills");
        assert_eq!(SkillState::MERGE, MergeStrategy::Commutative);
        assert_eq!(SkillState::SCOPE, KeyScope::Thread);
    }

    #[test]
    fn merge_strategy_is_union() {
        // When two runs on the same thread both activate skills,
        // the commutative (grow-only set) merge produces a union.
        let mut state = SkillStateValue::default();
        SkillState::apply(&mut state, SkillStateUpdate::Activate("s1".to_string()));

        let mut other = SkillStateValue::default();
        SkillState::apply(&mut other, SkillStateUpdate::Activate("s2".to_string()));

        // Simulating merge: apply other's activations to state
        for id in other.active {
            SkillState::apply(&mut state, SkillStateUpdate::Activate(id));
        }
        assert_eq!(state.active.len(), 2);
        assert!(state.active.contains("s1"));
        assert!(state.active.contains("s2"));
    }

    #[test]
    fn default_state_has_empty_active() {
        let state = SkillStateValue::default();
        assert!(state.active.is_empty());
    }

    #[test]
    fn state_serde_roundtrip() {
        let mut state = SkillStateValue::default();
        state.active.insert("s1".to_string());
        state.active.insert("s2".to_string());
        let json = serde_json::to_value(&state).unwrap();
        let parsed: SkillStateValue = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.active, state.active);
    }
}
