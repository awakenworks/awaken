//! Skill visibility state, actions, and policy (ADR-0020).
//!
//! Follows the mechanism-policy separation pattern established by
//! `awaken-ext-permission` (PermissionPolicy + PermissionOverrides)
//! and `awaken-ext-deferred-tools` (DeferralState + DeferralPolicy).

use std::collections::HashMap;

use awaken_contract::state::{KeyScope, MergeStrategy, StateKey};
use serde::{Deserialize, Serialize};

use crate::skill::SkillMeta;

// ---------------------------------------------------------------------------
// Visibility decision
// ---------------------------------------------------------------------------

/// Whether a skill should appear in the LLM catalog.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillVisibility {
    #[default]
    Visible,
    Hidden,
}

// ---------------------------------------------------------------------------
// State value
// ---------------------------------------------------------------------------

/// Per-skill visibility state (run-scoped).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillVisibilityStateValue {
    /// Skill ID → visibility.  Skills absent from the map are treated as `Visible`.
    pub modes: HashMap<String, SkillVisibility>,
}

impl SkillVisibilityStateValue {
    /// Returns the effective visibility for the given skill.
    pub fn visibility_of(&self, skill_id: &str) -> SkillVisibility {
        self.modes
            .get(skill_id)
            .copied()
            .unwrap_or(SkillVisibility::Visible)
    }

    /// Returns an iterator over all hidden skill IDs.
    pub fn hidden_ids(&self) -> impl Iterator<Item = &str> {
        self.modes
            .iter()
            .filter(|(_, v)| **v == SkillVisibility::Hidden)
            .map(|(k, _)| k.as_str())
    }
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

/// Action for mutating skill visibility state.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SkillVisibilityAction {
    /// Make a single skill visible.
    Show { skill_id: String },
    /// Hide a single skill from the catalog.
    Hide { skill_id: String },
    /// Make multiple skills visible at once.
    ShowBatch { skill_ids: Vec<String> },
    /// Batch-set initial visibility (typically used at run start).
    SetBatch {
        entries: Vec<(String, SkillVisibility)>,
    },
}

// ---------------------------------------------------------------------------
// State key
// ---------------------------------------------------------------------------

/// Run-scoped state key for skill visibility.
///
/// Scoped to `Run` so visibility decisions do not leak across runs, mirroring
/// `PermissionOverridesKey`.
pub struct SkillVisibilityStateKey;

impl StateKey for SkillVisibilityStateKey {
    const KEY: &'static str = "skills.visibility";
    const MERGE: MergeStrategy = MergeStrategy::Commutative;
    const SCOPE: KeyScope = KeyScope::Run;

    type Value = SkillVisibilityStateValue;
    type Update = SkillVisibilityAction;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        match update {
            SkillVisibilityAction::Show { skill_id } => {
                value.modes.insert(skill_id, SkillVisibility::Visible);
            }
            SkillVisibilityAction::Hide { skill_id } => {
                value.modes.insert(skill_id, SkillVisibility::Hidden);
            }
            SkillVisibilityAction::ShowBatch { skill_ids } => {
                for id in skill_ids {
                    value.modes.insert(id, SkillVisibility::Visible);
                }
            }
            SkillVisibilityAction::SetBatch { entries } => {
                for (id, vis) in entries {
                    value.modes.insert(id, vis);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Policy trait
// ---------------------------------------------------------------------------

/// Policy that decides the initial visibility for each skill at run start.
///
/// Implementations can incorporate config rules, feature flags, or dynamic
/// criteria.  The default implementation uses `SkillMeta` fields only.
pub trait SkillVisibilityPolicy: Send + Sync {
    /// Evaluate visibility for a single skill.
    fn evaluate(&self, meta: &SkillMeta) -> SkillVisibility;

    /// Batch-evaluate all skills and return actions to seed the visibility state.
    fn seed(&self, metas: &[&SkillMeta]) -> Vec<(String, SkillVisibility)> {
        metas
            .iter()
            .map(|m| (m.id.clone(), self.evaluate(m)))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Default policy
// ---------------------------------------------------------------------------

/// Default policy based on `SkillMeta` fields.
///
/// A skill is `Hidden` if:
/// - `model_invocable` is `false` (frontmatter `disable-model-invocation: true`), OR
/// - `paths` is non-empty (conditional skill — starts hidden until file match).
///
/// Otherwise it is `Visible`.
#[derive(Debug, Clone, Default)]
pub struct DefaultSkillVisibilityPolicy;

impl SkillVisibilityPolicy for DefaultSkillVisibilityPolicy {
    fn evaluate(&self, meta: &SkillMeta) -> SkillVisibility {
        if !meta.model_invocable || !meta.paths.is_empty() {
            SkillVisibility::Hidden
        } else {
            SkillVisibility::Visible
        }
    }
}

// ---------------------------------------------------------------------------
// Convenience action constructors
// ---------------------------------------------------------------------------

/// Schedule a `Show` action for the given skill.
pub fn show_skill(batch: &mut awaken_runtime::state::MutationBatch, skill_id: impl Into<String>) {
    batch.update::<SkillVisibilityStateKey>(SkillVisibilityAction::Show {
        skill_id: skill_id.into(),
    });
}

/// Schedule a `Hide` action for the given skill.
pub fn hide_skill(batch: &mut awaken_runtime::state::MutationBatch, skill_id: impl Into<String>) {
    batch.update::<SkillVisibilityStateKey>(SkillVisibilityAction::Hide {
        skill_id: skill_id.into(),
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_visibility_is_visible() {
        assert_eq!(SkillVisibility::default(), SkillVisibility::Visible);
    }

    #[test]
    fn state_value_default_visibility_of_unknown_skill() {
        let state = SkillVisibilityStateValue::default();
        assert_eq!(state.visibility_of("unknown"), SkillVisibility::Visible);
    }

    #[test]
    fn show_action_sets_visible() {
        let mut state = SkillVisibilityStateValue::default();
        SkillVisibilityStateKey::apply(
            &mut state,
            SkillVisibilityAction::Hide {
                skill_id: "s1".into(),
            },
        );
        assert_eq!(state.visibility_of("s1"), SkillVisibility::Hidden);

        SkillVisibilityStateKey::apply(
            &mut state,
            SkillVisibilityAction::Show {
                skill_id: "s1".into(),
            },
        );
        assert_eq!(state.visibility_of("s1"), SkillVisibility::Visible);
    }

    #[test]
    fn hide_action_sets_hidden() {
        let mut state = SkillVisibilityStateValue::default();
        SkillVisibilityStateKey::apply(
            &mut state,
            SkillVisibilityAction::Hide {
                skill_id: "s1".into(),
            },
        );
        assert_eq!(state.visibility_of("s1"), SkillVisibility::Hidden);
    }

    #[test]
    fn show_batch_action() {
        let mut state = SkillVisibilityStateValue::default();
        SkillVisibilityStateKey::apply(
            &mut state,
            SkillVisibilityAction::Hide {
                skill_id: "s1".into(),
            },
        );
        SkillVisibilityStateKey::apply(
            &mut state,
            SkillVisibilityAction::Hide {
                skill_id: "s2".into(),
            },
        );
        SkillVisibilityStateKey::apply(
            &mut state,
            SkillVisibilityAction::ShowBatch {
                skill_ids: vec!["s1".into(), "s2".into()],
            },
        );
        assert_eq!(state.visibility_of("s1"), SkillVisibility::Visible);
        assert_eq!(state.visibility_of("s2"), SkillVisibility::Visible);
    }

    #[test]
    fn set_batch_action() {
        let mut state = SkillVisibilityStateValue::default();
        SkillVisibilityStateKey::apply(
            &mut state,
            SkillVisibilityAction::SetBatch {
                entries: vec![
                    ("s1".into(), SkillVisibility::Hidden),
                    ("s2".into(), SkillVisibility::Visible),
                    ("s3".into(), SkillVisibility::Hidden),
                ],
            },
        );
        assert_eq!(state.visibility_of("s1"), SkillVisibility::Hidden);
        assert_eq!(state.visibility_of("s2"), SkillVisibility::Visible);
        assert_eq!(state.visibility_of("s3"), SkillVisibility::Hidden);
    }

    #[test]
    fn hidden_ids_iterator() {
        let mut state = SkillVisibilityStateValue::default();
        SkillVisibilityStateKey::apply(
            &mut state,
            SkillVisibilityAction::SetBatch {
                entries: vec![
                    ("a".into(), SkillVisibility::Hidden),
                    ("b".into(), SkillVisibility::Visible),
                    ("c".into(), SkillVisibility::Hidden),
                ],
            },
        );
        let mut hidden: Vec<&str> = state.hidden_ids().collect();
        hidden.sort();
        assert_eq!(hidden, vec!["a", "c"]);
    }

    #[test]
    fn state_key_constants() {
        assert_eq!(SkillVisibilityStateKey::KEY, "skills.visibility");
        assert_eq!(SkillVisibilityStateKey::MERGE, MergeStrategy::Commutative);
        assert_eq!(SkillVisibilityStateKey::SCOPE, KeyScope::Run);
    }

    #[test]
    fn serde_roundtrip() {
        let mut state = SkillVisibilityStateValue::default();
        state.modes.insert("s1".into(), SkillVisibility::Hidden);
        state.modes.insert("s2".into(), SkillVisibility::Visible);
        let json = serde_json::to_value(&state).unwrap();
        let parsed: SkillVisibilityStateValue = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.visibility_of("s1"), SkillVisibility::Hidden);
        assert_eq!(parsed.visibility_of("s2"), SkillVisibility::Visible);
    }

    // --- Default policy tests ---

    #[test]
    fn default_policy_visible_for_normal_skill() {
        let policy = DefaultSkillVisibilityPolicy;
        let meta = SkillMeta::new("s1", "s1", "desc", vec![]);
        assert_eq!(policy.evaluate(&meta), SkillVisibility::Visible);
    }

    #[test]
    fn default_policy_hidden_when_model_invocable_false() {
        let policy = DefaultSkillVisibilityPolicy;
        let mut meta = SkillMeta::new("s1", "s1", "desc", vec![]);
        meta.model_invocable = false;
        assert_eq!(policy.evaluate(&meta), SkillVisibility::Hidden);
    }

    #[test]
    fn default_policy_hidden_when_paths_present() {
        let policy = DefaultSkillVisibilityPolicy;
        let mut meta = SkillMeta::new("s1", "s1", "desc", vec![]);
        meta.paths = vec!["*.tsx".into()];
        assert_eq!(policy.evaluate(&meta), SkillVisibility::Hidden);
    }

    #[test]
    fn default_policy_seed_batch() {
        let policy = DefaultSkillVisibilityPolicy;
        let m1 = SkillMeta::new("visible", "visible", "ok", vec![]);
        let mut m2 = SkillMeta::new("hidden", "hidden", "ok", vec![]);
        m2.model_invocable = false;
        let metas: Vec<&SkillMeta> = vec![&m1, &m2];
        let result = policy.seed(&metas);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], ("visible".into(), SkillVisibility::Visible));
        assert_eq!(result[1], ("hidden".into(), SkillVisibility::Hidden));
    }
}
