use std::collections::{BTreeMap, HashSet};

use awaken_runtime_contract::state::{KeyScope, MergeStrategy, StateKey};
use serde::{Deserialize, Serialize};

/// Persisted skill state tracking which skills are active.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillStateValue {
    /// Activated skill IDs.
    #[serde(default)]
    pub active: HashSet<String>,
    /// Rendered activation instructions captured at activation time.
    ///
    /// Older persisted state only contains `active`; the active-instructions
    /// plugin falls back to reading the registry for those legacy entries.
    #[serde(
        default,
        deserialize_with = "deserialize_activations",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub activations: BTreeMap<String, SkillRenderedActivation>,
}

/// Rendered instructions for one skill activation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillRenderedActivation {
    pub skill_id: String,
    pub instructions: String,
    /// Optional fingerprint of the skill spec/content used to render
    /// `instructions`. When present, active-instructions rendering drops stale
    /// activations if the live skill no longer reports the same fingerprint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

/// Update type for skill state.
#[derive(Debug)]
pub enum SkillStateUpdate {
    /// Mark a skill as activated (insert into the set).
    Activate(String),
    /// Remove a skill from the active set and drop any rendered activation.
    Deactivate(String),
    /// Mark a skill as activated and persist its rendered instructions.
    ActivateRendered {
        skill_id: String,
        instructions: String,
        fingerprint: Option<String>,
    },
}

/// State key for tracking active skills.
pub struct SkillState;

impl StateKey for SkillState {
    const KEY: &'static str = "skills";
    const MERGE: MergeStrategy = MergeStrategy::Exclusive;
    const SCOPE: KeyScope = KeyScope::Run;

    type Value = SkillStateValue;
    type Update = SkillStateUpdate;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        match update {
            SkillStateUpdate::Activate(id) => {
                value.active.insert(id);
            }
            SkillStateUpdate::Deactivate(id) => {
                value.active.remove(&id);
                value.activations.remove(&id);
            }
            SkillStateUpdate::ActivateRendered {
                skill_id,
                instructions,
                fingerprint,
            } => {
                value.active.insert(skill_id.clone());
                value.activations.insert(
                    skill_id.clone(),
                    SkillRenderedActivation {
                        skill_id,
                        instructions,
                        fingerprint,
                    },
                );
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ActivationsWire {
    Map(BTreeMap<String, SkillRenderedActivation>),
    List(Vec<SkillRenderedActivation>),
}

fn deserialize_activations<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, SkillRenderedActivation>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let Some(wire) = Option::<ActivationsWire>::deserialize(deserializer)? else {
        return Ok(BTreeMap::new());
    };

    let mut out = BTreeMap::new();
    match wire {
        ActivationsWire::Map(map) => {
            for (_key, activation) in map {
                out.insert(activation.skill_id.clone(), activation);
            }
        }
        ActivationsWire::List(list) => {
            for activation in list {
                out.insert(activation.skill_id.clone(), activation);
            }
        }
    }
    Ok(out)
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
    fn activate_rendered_records_instructions() {
        let mut state = SkillStateValue::default();
        SkillState::apply(
            &mut state,
            SkillStateUpdate::ActivateRendered {
                skill_id: "s1".to_string(),
                instructions: "Use postgres syntax.".to_string(),
                fingerprint: Some("fp-1".to_string()),
            },
        );
        assert!(state.active.contains("s1"));
        assert_eq!(
            state.activations.get("s1"),
            Some(&SkillRenderedActivation {
                skill_id: "s1".to_string(),
                instructions: "Use postgres syntax.".to_string(),
                fingerprint: Some("fp-1".to_string()),
            })
        );
    }

    #[test]
    fn activate_rendered_replaces_existing_activation_for_same_skill() {
        let mut state = SkillStateValue::default();
        SkillState::apply(
            &mut state,
            SkillStateUpdate::ActivateRendered {
                skill_id: "s1".to_string(),
                instructions: "Use postgres syntax.".to_string(),
                fingerprint: Some("fp-1".to_string()),
            },
        );
        SkillState::apply(
            &mut state,
            SkillStateUpdate::ActivateRendered {
                skill_id: "s1".to_string(),
                instructions: "Use mysql syntax.".to_string(),
                fingerprint: Some("fp-2".to_string()),
            },
        );

        assert_eq!(state.activations.len(), 1);
        assert_eq!(
            state.activations.get("s1"),
            Some(&SkillRenderedActivation {
                skill_id: "s1".to_string(),
                instructions: "Use mysql syntax.".to_string(),
                fingerprint: Some("fp-2".to_string()),
            })
        );
    }

    #[test]
    fn deactivate_removes_active_and_rendered_activation() {
        let mut state = SkillStateValue::default();
        SkillState::apply(
            &mut state,
            SkillStateUpdate::ActivateRendered {
                skill_id: "s1".to_string(),
                instructions: "Use postgres syntax.".to_string(),
                fingerprint: Some("fp-1".to_string()),
            },
        );

        SkillState::apply(&mut state, SkillStateUpdate::Deactivate("s1".to_string()));

        assert!(!state.active.contains("s1"));
        assert!(!state.activations.contains_key("s1"));
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
        assert_eq!(SkillState::MERGE, MergeStrategy::Exclusive);
        assert_eq!(SkillState::SCOPE, KeyScope::Run);
    }

    #[test]
    fn sequential_legacy_activations_form_union() {
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
    fn parallel_rendered_activation_batches_conflict_under_exclusive_strategy() {
        use awaken_runtime_contract::StateError;
        use awaken_runtime_contract::state::MutationBatch;

        let mut left = MutationBatch::new();
        left.update::<SkillState>(SkillStateUpdate::ActivateRendered {
            skill_id: "db".to_string(),
            instructions: "Use postgres syntax.".to_string(),
            fingerprint: Some("fp".to_string()),
        });

        let mut right = MutationBatch::new();
        right.update::<SkillState>(SkillStateUpdate::ActivateRendered {
            skill_id: "db".to_string(),
            instructions: "Use mysql syntax.".to_string(),
            fingerprint: Some("fp".to_string()),
        });

        let err = left
            .merge_parallel(right, |key| {
                if key == SkillState::KEY {
                    SkillState::MERGE
                } else {
                    MergeStrategy::Exclusive
                }
            })
            .expect_err("parallel rendered activations must not be merged as commutative");

        assert!(matches!(
            err,
            StateError::ParallelMergeConflict { ref key } if key == SkillState::KEY
        ));
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
        state.activations.insert(
            "s1".to_string(),
            SkillRenderedActivation {
                skill_id: "s1".to_string(),
                instructions: "Rendered".to_string(),
                fingerprint: Some("fp-1".to_string()),
            },
        );
        let json = serde_json::to_value(&state).unwrap();
        let parsed: SkillStateValue = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.active, state.active);
        assert_eq!(parsed.activations, state.activations);
    }

    #[test]
    fn state_serde_accepts_legacy_active_only_payload() {
        let parsed: SkillStateValue =
            serde_json::from_value(serde_json::json!({"active": ["s1"]})).unwrap();
        assert!(parsed.active.contains("s1"));
        assert!(parsed.activations.is_empty());
    }

    #[test]
    fn state_serde_accepts_legacy_activation_list_payload() {
        let parsed: SkillStateValue = serde_json::from_value(serde_json::json!({
            "active": ["s1"],
            "activations": [
                {"skill_id": "s1", "instructions": "old"},
                {"skill_id": "s1", "instructions": "new"}
            ]
        }))
        .unwrap();

        assert_eq!(parsed.activations.len(), 1);
        assert_eq!(
            parsed.activations.get("s1").unwrap().instructions.as_str(),
            "new"
        );
    }
}
