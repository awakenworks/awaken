//! Skill visibility state, actions, and policy (ADR-0020).
//!
//! Follows the mechanism-policy separation pattern established by
//! `awaken-ext-permission` (PermissionPolicy + PermissionOverrides) and
//! `awaken-ext-deferred-tools` (`DeferralState` plus declarative config
//! classification via `resolve_mode`, not a pluggable policy trait). Here the
//! mechanism is `SkillVisibilityStateKey` + `SkillVisibilityAction`; the policy
//! is the declarative, metadata-derived `DefaultSkillVisibilityPolicy`.

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
    /// Skill ID → **explicit** visibility override.
    ///
    /// Absence does NOT mean `Visible`: it means "no explicit runtime override".
    /// Callers must resolve actual visibility through [`effective_visibility`],
    /// which falls back to the declarative metadata policy. Reading this map
    /// directly and treating absent as `Visible` re-introduces the fail-open bug.
    pub modes: HashMap<String, SkillVisibility>,
}

impl SkillVisibilityStateValue {
    /// Returns the EXPLICIT visibility entry for a skill, or `None` when the
    /// skill carries no recorded Show/Hide state.
    ///
    /// This deliberately does not fail open to `Visible`: this value records
    /// only explicit overrides. Resolving the visibility a skill should actually
    /// have — explicit override, else metadata policy — is the job of
    /// [`effective_visibility`], the single source of truth for the catalog.
    pub fn explicit(&self, skill_id: &str) -> Option<SkillVisibility> {
        self.modes.get(skill_id).copied()
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
    /// Batch-set visibility, overwriting any existing entry (last-write-wins).
    /// For explicit runtime control; not for run-start seeding (use `SeedBatch`).
    SetBatch {
        entries: Vec<(String, SkillVisibility)>,
    },
    /// Seed initial visibility at run start, **insert-if-absent**: an entry is
    /// written only when the skill has no existing state. This guarantees the
    /// seed never clobbers a runtime `Show`/`Hide` that already happened (e.g. on
    /// re-activation, handoff, resume, or sub-agent activation).
    SeedBatch {
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
    /// `Commutative` here means "parallel batches touching this key may merge
    /// without conflict" — each op is a per-skill-ID map insert, resolved
    /// last-write-wins, exactly like the `PermissionOverridesKey` reducer
    /// (`AllowTool`/`DenyTool` on the same tool). It is not strict mathematical
    /// commutativity: two parallel ops on the *same* ID resolve by concatenation
    /// order. `Exclusive` would be wrong — it would make two hooks that both
    /// adjust visibility hard-conflict, defeating additive promotion. The seed
    /// (run-start `on_activate`) never races runtime overrides: it is committed
    /// before any tool runs.
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
            SkillVisibilityAction::SeedBatch { entries } => {
                for (id, vis) in entries {
                    value.modes.entry(id).or_insert(vis);
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
// Default visibility policy
// ---------------------------------------------------------------------------

/// Default per-skill visibility decision (ADR-0020).
///
/// Visibility is **declarative** — derived from skill metadata, not a
/// user-pluggable strategy (mirroring `awaken-ext-permission`, where policy is
/// declarative rule data). A skill is `Hidden` when `model_invocable` is `false`
/// (frontmatter `disable-model-invocation: true`); otherwise `Visible`.
///
/// `paths` is **not** an input. The agentskills specification has no
/// path/glob-based conditional activation — skills are surfaced by their
/// `description` (progressive disclosure) and model-invoked. `paths` is a
/// non-standard awaken field, retained as parsed metadata but with no effect on
/// catalog visibility.
///
/// Used by [`effective_visibility`] as the fallback when a skill has no explicit
/// runtime `Show`/`Hide` override.
#[derive(Debug, Clone, Default)]
pub(crate) struct DefaultSkillVisibilityPolicy;

impl DefaultSkillVisibilityPolicy {
    /// Evaluate visibility for a single skill from its metadata.
    pub(crate) fn evaluate(&self, meta: &SkillMeta) -> SkillVisibility {
        if !meta.model_invocable {
            SkillVisibility::Hidden
        } else {
            SkillVisibility::Visible
        }
    }
}

// ---------------------------------------------------------------------------
// Effective visibility (single source of truth)
// ---------------------------------------------------------------------------

/// Resolve the visibility a skill should have in the catalog.
///
/// This is the one rule every read path must use: an explicit Show/Hide in the
/// run-scoped state ([`SkillVisibilityStateValue::explicit`]) wins; otherwise it
/// falls back to the declarative metadata policy ([`DefaultSkillVisibilityPolicy`])
/// rather than failing open. Because the policy type is crate-private, this
/// function is the public entry point for reproducing the catalog's decision.
pub fn effective_visibility(
    meta: &SkillMeta,
    state: Option<&SkillVisibilityStateValue>,
) -> SkillVisibility {
    state
        .and_then(|s| s.explicit(&meta.id))
        .unwrap_or_else(|| DefaultSkillVisibilityPolicy.evaluate(meta))
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
    fn explicit_returns_none_for_unknown_skill() {
        let mut state = SkillVisibilityStateValue::default();
        state.modes.insert("known".into(), SkillVisibility::Hidden);
        assert_eq!(state.explicit("unknown"), None);
        assert_eq!(state.explicit("known"), Some(SkillVisibility::Hidden));
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
        assert_eq!(state.explicit("s1"), Some(SkillVisibility::Hidden));

        SkillVisibilityStateKey::apply(
            &mut state,
            SkillVisibilityAction::Show {
                skill_id: "s1".into(),
            },
        );
        assert_eq!(state.explicit("s1"), Some(SkillVisibility::Visible));
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
        assert_eq!(state.explicit("s1"), Some(SkillVisibility::Hidden));
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
        assert_eq!(state.explicit("s1"), Some(SkillVisibility::Visible));
        assert_eq!(state.explicit("s2"), Some(SkillVisibility::Visible));
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
        assert_eq!(state.explicit("s1"), Some(SkillVisibility::Hidden));
        assert_eq!(state.explicit("s2"), Some(SkillVisibility::Visible));
        assert_eq!(state.explicit("s3"), Some(SkillVisibility::Hidden));
    }

    #[test]
    fn seed_batch_inserts_only_when_absent() {
        // SeedBatch must never clobber an existing explicit runtime override:
        // a prior Show/Hide wins; only skills with no entry get the seeded value.
        let mut state = SkillVisibilityStateValue::default();
        // Runtime override happened first (e.g. a tool promoted s1).
        SkillVisibilityStateKey::apply(
            &mut state,
            SkillVisibilityAction::Show {
                skill_id: "s1".into(),
            },
        );
        // Re-activation seeds s1=Hidden and s2=Hidden.
        SkillVisibilityStateKey::apply(
            &mut state,
            SkillVisibilityAction::SeedBatch {
                entries: vec![
                    ("s1".into(), SkillVisibility::Hidden),
                    ("s2".into(), SkillVisibility::Hidden),
                ],
            },
        );
        assert_eq!(
            state.explicit("s1"),
            Some(SkillVisibility::Visible),
            "seed must not overwrite an existing runtime override"
        );
        assert_eq!(
            state.explicit("s2"),
            Some(SkillVisibility::Hidden),
            "seed fills entries that are absent"
        );
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
        assert_eq!(parsed.explicit("s1"), Some(SkillVisibility::Hidden));
        assert_eq!(parsed.explicit("s2"), Some(SkillVisibility::Visible));
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
    fn default_policy_visible_when_only_paths_present() {
        // Path-conditional hiding is deferred until a file-match promote hook
        // exists (ADR-0020 D5, future). Until then a `paths`-only skill stays
        // Visible rather than vanishing from the catalog with no way back.
        let policy = DefaultSkillVisibilityPolicy;
        let mut meta = SkillMeta::new("s1", "s1", "desc", vec![]);
        meta.paths = vec!["*.tsx".into()];
        assert_eq!(policy.evaluate(&meta), SkillVisibility::Visible);
    }

    // --- Effective visibility (single source of truth) ---

    #[test]
    fn effective_visibility_explicit_state_wins_over_metadata() {
        // Explicit Show promotes a metadata-Hidden skill; explicit Hide suppresses
        // a metadata-Visible one.
        let mut blocked = SkillMeta::new("blocked", "blocked", "d", vec![]);
        blocked.model_invocable = false;
        let normal = SkillMeta::new("normal", "normal", "d", vec![]);

        let mut state = SkillVisibilityStateValue::default();
        state
            .modes
            .insert("blocked".into(), SkillVisibility::Visible);
        state.modes.insert("normal".into(), SkillVisibility::Hidden);

        assert_eq!(
            effective_visibility(&blocked, Some(&state)),
            SkillVisibility::Visible
        );
        assert_eq!(
            effective_visibility(&normal, Some(&state)),
            SkillVisibility::Hidden
        );
    }

    #[test]
    fn effective_visibility_falls_back_to_metadata_policy_when_absent() {
        let mut blocked = SkillMeta::new("blocked", "blocked", "d", vec![]);
        blocked.model_invocable = false;
        let normal = SkillMeta::new("normal", "normal", "d", vec![]);

        // No state at all, and a state that omits the skill: both fall back.
        assert_eq!(
            effective_visibility(&normal, None),
            SkillVisibility::Visible
        );
        assert_eq!(
            effective_visibility(&blocked, None),
            SkillVisibility::Hidden
        );

        let empty = SkillVisibilityStateValue::default();
        assert_eq!(
            effective_visibility(&blocked, Some(&empty)),
            SkillVisibility::Hidden
        );
    }

    #[test]
    fn default_policy_ignores_paths() {
        // `paths` is not in the agentskills spec and must not affect visibility.
        let policy = DefaultSkillVisibilityPolicy;
        let mut meta = SkillMeta::new("s1", "s1", "desc", vec![]);
        meta.paths = vec!["src/**/*.rs".into(), "*.tsx".into()];
        assert_eq!(policy.evaluate(&meta), SkillVisibility::Visible);
    }
}
