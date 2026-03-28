//! State keys and reducers for permission policy.

use std::collections::HashMap;

use awaken_runtime::state::{KeyScope, MergeStrategy, StateKey};
use serde::{Deserialize, Serialize};

use crate::rules::{
    PermissionRule, PermissionRuleScope, PermissionRuleSource, PermissionRuleset,
    PermissionSubject, ToolPermissionBehavior, parse_pattern,
};

// ---------------------------------------------------------------------------
// Permission actions
// ---------------------------------------------------------------------------

/// Permission-domain action used by both [`PermissionPolicy`] and
/// [`PermissionOverrides`] reducers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PermissionAction {
    SetDefault {
        behavior: ToolPermissionBehavior,
    },
    SetTool {
        tool_id: String,
        behavior: ToolPermissionBehavior,
    },
    /// Set a pattern-based rule (function-call syntax string).
    SetRule {
        pattern: String,
        behavior: ToolPermissionBehavior,
    },
    RemoveTool {
        tool_id: String,
    },
    /// Remove a pattern-based rule by its canonical pattern string.
    RemoveRule {
        pattern: String,
    },
    ClearTools,
    /// Convenience: allow a tool (equivalent to `SetTool` with `Allow`).
    AllowTool {
        tool_id: String,
    },
    /// Convenience: deny a tool (equivalent to `SetTool` with `Deny`).
    DenyTool {
        tool_id: String,
    },
}

// ---------------------------------------------------------------------------
// PermissionPolicy — thread-scoped persisted state
// ---------------------------------------------------------------------------

/// Persisted permission rules.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionPolicy {
    pub default_behavior: ToolPermissionBehavior,
    pub rules: HashMap<String, PermissionRule>,
}

/// State key for the thread-scoped permission policy.
pub struct PermissionPolicyKey;

impl StateKey for PermissionPolicyKey {
    const KEY: &'static str = "permission_policy";
    const MERGE: MergeStrategy = MergeStrategy::Commutative;
    const SCOPE: KeyScope = KeyScope::Thread;

    type Value = PermissionPolicy;
    type Update = PermissionAction;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        value.reduce(update);
    }
}

// ---------------------------------------------------------------------------
// Shared reduce helpers
// ---------------------------------------------------------------------------

fn upsert_tool_rule(
    rules: &mut HashMap<String, PermissionRule>,
    tool_id: String,
    behavior: ToolPermissionBehavior,
    source: PermissionRuleSource,
) {
    let rule = PermissionRule::new_tool(tool_id, behavior)
        .with_scope(PermissionRuleScope::Thread)
        .with_source(source);
    rules.insert(rule.subject.key(), rule);
}

fn upsert_pattern_rule(
    rules: &mut HashMap<String, PermissionRule>,
    pattern_str: String,
    behavior: ToolPermissionBehavior,
    source: PermissionRuleSource,
) {
    if let Ok(pattern) = parse_pattern(&pattern_str) {
        let rule = PermissionRule::new_pattern(pattern, behavior)
            .with_scope(PermissionRuleScope::Thread)
            .with_source(source);
        rules.insert(rule.subject.key(), rule);
    }
}

/// Shared reducer for all [`PermissionAction`] variants except `SetDefault`,
/// which is a no-op in this function (only meaningful for [`PermissionPolicy`]).
fn reduce_permission_action(
    rules: &mut HashMap<String, PermissionRule>,
    action: PermissionAction,
    source: PermissionRuleSource,
) {
    match action {
        PermissionAction::SetTool { tool_id, behavior } => {
            upsert_tool_rule(rules, tool_id, behavior, source);
        }
        PermissionAction::SetRule { pattern, behavior } => {
            upsert_pattern_rule(rules, pattern, behavior, source);
        }
        PermissionAction::AllowTool { tool_id } => {
            upsert_tool_rule(rules, tool_id, ToolPermissionBehavior::Allow, source);
        }
        PermissionAction::DenyTool { tool_id } => {
            upsert_tool_rule(rules, tool_id, ToolPermissionBehavior::Deny, source);
        }
        PermissionAction::RemoveTool { tool_id } => {
            rules.remove(
                &PermissionRule::new_tool(tool_id, ToolPermissionBehavior::Ask)
                    .subject
                    .key(),
            );
        }
        PermissionAction::RemoveRule { pattern } => {
            if let Ok(parsed) = parse_pattern(&pattern) {
                rules.remove(&PermissionSubject::pattern(parsed).key());
            }
        }
        PermissionAction::ClearTools => rules.clear(),
        PermissionAction::SetDefault { .. } => {}
    }
}

impl PermissionPolicy {
    fn reduce(&mut self, action: PermissionAction) {
        if let PermissionAction::SetDefault { behavior } = action {
            self.default_behavior = behavior;
            return;
        }
        reduce_permission_action(&mut self.rules, action, PermissionRuleSource::Runtime);
    }
}

// ---------------------------------------------------------------------------
// PermissionOverrides — run-scoped temporary overrides
// ---------------------------------------------------------------------------

/// Run-scoped permission overrides applied on top of thread-level [`PermissionPolicy`].
///
/// Automatically cleaned up when the run ends. Used by skill activation to
/// grant temporary tool permissions that do not leak across runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionOverrides {
    pub rules: HashMap<String, PermissionRule>,
}

/// State key for the run-scoped permission overrides.
pub struct PermissionOverridesKey;

impl StateKey for PermissionOverridesKey {
    const KEY: &'static str = "permission_overrides";
    const MERGE: MergeStrategy = MergeStrategy::Commutative;
    const SCOPE: KeyScope = KeyScope::Run;

    type Value = PermissionOverrides;
    type Update = PermissionAction;

    fn apply(value: &mut Self::Value, update: Self::Update) {
        value.reduce(update);
    }
}

impl PermissionOverrides {
    fn reduce(&mut self, action: PermissionAction) {
        reduce_permission_action(&mut self.rules, action, PermissionRuleSource::Skill);
    }
}

// ---------------------------------------------------------------------------
// Loading from snapshot
// ---------------------------------------------------------------------------

/// Load resolved permission rules from a snapshot.
///
/// Merges two layers with descending priority:
/// 1. Run-scoped [`PermissionOverrides`] (highest — temporary skill grants)
/// 2. Thread-level [`PermissionPolicy`] (base rules)
#[must_use]
pub fn permission_rules_from_state(
    policy: Option<&PermissionPolicy>,
    overrides: Option<&PermissionOverrides>,
) -> PermissionRuleset {
    let mut ruleset = PermissionRuleset::default();

    if let Some(policy) = policy {
        ruleset.default_behavior = policy.default_behavior;
        ruleset.rules.extend(policy.rules.clone());
    }

    // Apply run-scoped overrides last — they take highest priority.
    if let Some(overrides) = overrides {
        for (key, rule) in &overrides.rules {
            ruleset.rules.insert(key.clone(), rule.clone());
        }
    }

    ruleset
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_set_default() {
        let mut policy = PermissionPolicy::default();
        PermissionPolicyKey::apply(
            &mut policy,
            PermissionAction::SetDefault {
                behavior: ToolPermissionBehavior::Deny,
            },
        );
        assert_eq!(policy.default_behavior, ToolPermissionBehavior::Deny);
    }

    #[test]
    fn policy_set_tool() {
        let mut policy = PermissionPolicy::default();
        PermissionPolicyKey::apply(
            &mut policy,
            PermissionAction::SetTool {
                tool_id: "Bash".into(),
                behavior: ToolPermissionBehavior::Allow,
            },
        );
        assert_eq!(policy.rules.len(), 1);
        let rule = policy.rules.get("tool:Bash").unwrap();
        assert_eq!(rule.behavior, ToolPermissionBehavior::Allow);
    }

    #[test]
    fn policy_allow_tool() {
        let mut policy = PermissionPolicy::default();
        PermissionPolicyKey::apply(
            &mut policy,
            PermissionAction::AllowTool {
                tool_id: "Read".into(),
            },
        );
        let rule = policy.rules.get("tool:Read").unwrap();
        assert_eq!(rule.behavior, ToolPermissionBehavior::Allow);
    }

    #[test]
    fn policy_deny_tool() {
        let mut policy = PermissionPolicy::default();
        PermissionPolicyKey::apply(
            &mut policy,
            PermissionAction::DenyTool {
                tool_id: "Delete".into(),
            },
        );
        let rule = policy.rules.get("tool:Delete").unwrap();
        assert_eq!(rule.behavior, ToolPermissionBehavior::Deny);
    }

    #[test]
    fn policy_remove_tool() {
        let mut policy = PermissionPolicy::default();
        PermissionPolicyKey::apply(
            &mut policy,
            PermissionAction::SetTool {
                tool_id: "Bash".into(),
                behavior: ToolPermissionBehavior::Allow,
            },
        );
        assert_eq!(policy.rules.len(), 1);
        PermissionPolicyKey::apply(
            &mut policy,
            PermissionAction::RemoveTool {
                tool_id: "Bash".into(),
            },
        );
        assert!(policy.rules.is_empty());
    }

    #[test]
    fn policy_set_rule_pattern() {
        let mut policy = PermissionPolicy::default();
        PermissionPolicyKey::apply(
            &mut policy,
            PermissionAction::SetRule {
                pattern: "Bash(npm *)".into(),
                behavior: ToolPermissionBehavior::Allow,
            },
        );
        assert_eq!(policy.rules.len(), 1);
        assert!(policy.rules.contains_key("pattern:Bash(npm *)"));
    }

    #[test]
    fn policy_remove_rule_pattern() {
        let mut policy = PermissionPolicy::default();
        PermissionPolicyKey::apply(
            &mut policy,
            PermissionAction::SetRule {
                pattern: "Bash(npm *)".into(),
                behavior: ToolPermissionBehavior::Allow,
            },
        );
        PermissionPolicyKey::apply(
            &mut policy,
            PermissionAction::RemoveRule {
                pattern: "Bash(npm *)".into(),
            },
        );
        assert!(policy.rules.is_empty());
    }

    #[test]
    fn policy_clear_tools() {
        let mut policy = PermissionPolicy::default();
        PermissionPolicyKey::apply(
            &mut policy,
            PermissionAction::SetTool {
                tool_id: "A".into(),
                behavior: ToolPermissionBehavior::Allow,
            },
        );
        PermissionPolicyKey::apply(
            &mut policy,
            PermissionAction::SetTool {
                tool_id: "B".into(),
                behavior: ToolPermissionBehavior::Deny,
            },
        );
        PermissionPolicyKey::apply(&mut policy, PermissionAction::ClearTools);
        assert!(policy.rules.is_empty());
    }

    #[test]
    fn overrides_set_tool() {
        let mut overrides = PermissionOverrides::default();
        PermissionOverridesKey::apply(
            &mut overrides,
            PermissionAction::SetTool {
                tool_id: "Bash".into(),
                behavior: ToolPermissionBehavior::Allow,
            },
        );
        assert_eq!(overrides.rules.len(), 1);
        let rule = overrides.rules.get("tool:Bash").unwrap();
        assert_eq!(rule.source, PermissionRuleSource::Skill);
    }

    #[test]
    fn overrides_ignore_set_default() {
        let mut overrides = PermissionOverrides::default();
        PermissionOverridesKey::apply(
            &mut overrides,
            PermissionAction::SetDefault {
                behavior: ToolPermissionBehavior::Deny,
            },
        );
        // Should be no-op
        assert!(overrides.rules.is_empty());
    }

    #[test]
    fn overrides_set_pattern_rule() {
        let mut overrides = PermissionOverrides::default();
        PermissionOverridesKey::apply(
            &mut overrides,
            PermissionAction::SetRule {
                pattern: r#"Edit(file_path ~ "src/**")"#.into(),
                behavior: ToolPermissionBehavior::Allow,
            },
        );
        assert_eq!(overrides.rules.len(), 1);
    }

    #[test]
    fn overrides_remove_tool() {
        let mut overrides = PermissionOverrides::default();
        PermissionOverridesKey::apply(
            &mut overrides,
            PermissionAction::AllowTool {
                tool_id: "Bash".into(),
            },
        );
        PermissionOverridesKey::apply(
            &mut overrides,
            PermissionAction::RemoveTool {
                tool_id: "Bash".into(),
            },
        );
        assert!(overrides.rules.is_empty());
    }

    #[test]
    fn overrides_clear_tools() {
        let mut overrides = PermissionOverrides::default();
        PermissionOverridesKey::apply(
            &mut overrides,
            PermissionAction::AllowTool {
                tool_id: "A".into(),
            },
        );
        PermissionOverridesKey::apply(
            &mut overrides,
            PermissionAction::DenyTool {
                tool_id: "B".into(),
            },
        );
        PermissionOverridesKey::apply(&mut overrides, PermissionAction::ClearTools);
        assert!(overrides.rules.is_empty());
    }

    #[test]
    fn merge_policy_and_overrides() {
        let mut policy = PermissionPolicy::default();
        policy.default_behavior = ToolPermissionBehavior::Ask;
        PermissionPolicyKey::apply(
            &mut policy,
            PermissionAction::SetTool {
                tool_id: "Bash".into(),
                behavior: ToolPermissionBehavior::Ask,
            },
        );

        let mut overrides = PermissionOverrides::default();
        PermissionOverridesKey::apply(
            &mut overrides,
            PermissionAction::SetTool {
                tool_id: "Bash".into(),
                behavior: ToolPermissionBehavior::Allow,
            },
        );

        let ruleset = permission_rules_from_state(Some(&policy), Some(&overrides));
        assert_eq!(ruleset.default_behavior, ToolPermissionBehavior::Ask);
        let rule = ruleset.rules.get("tool:Bash").unwrap();
        // Override should win
        assert_eq!(rule.behavior, ToolPermissionBehavior::Allow);
    }

    #[test]
    fn merge_empty_overrides() {
        let mut policy = PermissionPolicy::default();
        policy.default_behavior = ToolPermissionBehavior::Deny;
        PermissionPolicyKey::apply(
            &mut policy,
            PermissionAction::AllowTool {
                tool_id: "Bash".into(),
            },
        );

        let ruleset = permission_rules_from_state(Some(&policy), None);
        assert_eq!(ruleset.default_behavior, ToolPermissionBehavior::Deny);
        assert_eq!(ruleset.rules.len(), 1);
    }

    #[test]
    fn merge_empty_policy() {
        let mut overrides = PermissionOverrides::default();
        PermissionOverridesKey::apply(
            &mut overrides,
            PermissionAction::AllowTool {
                tool_id: "Bash".into(),
            },
        );

        let ruleset = permission_rules_from_state(None, Some(&overrides));
        assert_eq!(ruleset.default_behavior, ToolPermissionBehavior::Ask);
        assert_eq!(ruleset.rules.len(), 1);
    }

    #[test]
    fn policy_serde_roundtrip() {
        let mut policy = PermissionPolicy::default();
        policy.default_behavior = ToolPermissionBehavior::Deny;
        PermissionPolicyKey::apply(
            &mut policy,
            PermissionAction::SetTool {
                tool_id: "Bash".into(),
                behavior: ToolPermissionBehavior::Allow,
            },
        );
        PermissionPolicyKey::apply(
            &mut policy,
            PermissionAction::SetRule {
                pattern: "Bash(npm *)".into(),
                behavior: ToolPermissionBehavior::Allow,
            },
        );

        let json = serde_json::to_value(&policy).unwrap();
        let decoded: PermissionPolicy = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.default_behavior, ToolPermissionBehavior::Deny);
        assert_eq!(decoded.rules.len(), 2);
    }

    #[test]
    fn overrides_serde_roundtrip() {
        let mut overrides = PermissionOverrides::default();
        PermissionOverridesKey::apply(
            &mut overrides,
            PermissionAction::AllowTool {
                tool_id: "Bash".into(),
            },
        );

        let json = serde_json::to_value(&overrides).unwrap();
        let decoded: PermissionOverrides = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.rules.len(), 1);
    }
}
