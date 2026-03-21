use crate::model::{
    PermissionRule, PermissionRuleScope, PermissionRuleSource, PermissionRuleset,
    PermissionSubject, ToolPermissionBehavior,
};
use crate::parser::parse_pattern;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tirea_state::State;

/// Permission-domain action used by both [`PermissionPolicy`] and
/// [`PermissionOverrides`] reducers. Scope and source metadata are determined
/// by the target reducer, not by fields on the action itself.
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

/// Persisted permission rules.
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[serde(default)]
#[tirea(
    path = "permission_policy",
    action = "PermissionAction",
    scope = "thread"
)]
pub struct PermissionPolicy {
    pub default_behavior: ToolPermissionBehavior,
    pub rules: HashMap<String, PermissionRule>,
}

/// Run-scoped permission overrides applied on top of thread-level [`PermissionPolicy`].
///
/// Automatically cleaned up by `prepare_run()` when the run ends. Used by skill
/// activation to grant temporary tool permissions that do not leak across runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, State)]
#[serde(default)]
#[tirea(
    path = "permission_overrides",
    action = "PermissionAction",
    scope = "run"
)]
pub struct PermissionOverrides {
    pub rules: HashMap<String, PermissionRule>,
}

impl PermissionOverrides {
    fn upsert_tool_rule(&mut self, tool_id: String, behavior: ToolPermissionBehavior) {
        let rule = PermissionRule::new_tool(tool_id, behavior)
            .with_scope(PermissionRuleScope::Thread)
            .with_source(PermissionRuleSource::Skill);
        self.rules.insert(rule.subject.key(), rule);
    }

    fn upsert_pattern_rule(&mut self, pattern_str: String, behavior: ToolPermissionBehavior) {
        if let Ok(pattern) = parse_pattern(&pattern_str) {
            let rule = PermissionRule::new_pattern(pattern, behavior)
                .with_scope(PermissionRuleScope::Thread)
                .with_source(PermissionRuleSource::Skill);
            self.rules.insert(rule.subject.key(), rule);
        }
    }

    pub(super) fn reduce(&mut self, action: PermissionAction) {
        match action {
            PermissionAction::SetTool { tool_id, behavior } => {
                self.upsert_tool_rule(tool_id, behavior)
            }
            PermissionAction::SetRule { pattern, behavior } => {
                self.upsert_pattern_rule(pattern, behavior)
            }
            PermissionAction::AllowTool { tool_id } => {
                self.upsert_tool_rule(tool_id, ToolPermissionBehavior::Allow)
            }
            PermissionAction::DenyTool { tool_id } => {
                self.upsert_tool_rule(tool_id, ToolPermissionBehavior::Deny)
            }
            PermissionAction::RemoveTool { tool_id } => {
                self.rules.remove(
                    &PermissionRule::new_tool(tool_id, ToolPermissionBehavior::Ask)
                        .subject
                        .key(),
                );
            }
            PermissionAction::RemoveRule { pattern } => {
                if let Ok(parsed) = parse_pattern(&pattern) {
                    self.rules.remove(&PermissionSubject::pattern(parsed).key());
                }
            }
            PermissionAction::ClearTools => self.rules.clear(),
            // SetDefault has no run-scoped equivalent; callers should route
            // this variant to PermissionPolicy instead.
            PermissionAction::SetDefault { .. } => {}
        }
    }
}

impl PermissionPolicy {
    fn upsert_tool_rule(&mut self, tool_id: String, behavior: ToolPermissionBehavior) {
        let rule = PermissionRule::new_tool(tool_id, behavior)
            .with_scope(PermissionRuleScope::Thread)
            .with_source(PermissionRuleSource::Runtime);
        self.rules.insert(rule.subject.key(), rule);
    }

    fn upsert_pattern_rule(&mut self, pattern_str: String, behavior: ToolPermissionBehavior) {
        if let Ok(pattern) = parse_pattern(&pattern_str) {
            let rule = PermissionRule::new_pattern(pattern, behavior)
                .with_scope(PermissionRuleScope::Thread)
                .with_source(PermissionRuleSource::Runtime);
            self.rules.insert(rule.subject.key(), rule);
        }
    }

    pub(super) fn reduce(&mut self, action: PermissionAction) {
        match action {
            PermissionAction::SetDefault { behavior } => self.default_behavior = behavior,
            PermissionAction::SetTool { tool_id, behavior } => {
                self.upsert_tool_rule(tool_id, behavior)
            }
            PermissionAction::SetRule { pattern, behavior } => {
                self.upsert_pattern_rule(pattern, behavior)
            }
            PermissionAction::AllowTool { tool_id } => {
                self.upsert_tool_rule(tool_id, ToolPermissionBehavior::Allow)
            }
            PermissionAction::DenyTool { tool_id } => {
                self.upsert_tool_rule(tool_id, ToolPermissionBehavior::Deny)
            }
            PermissionAction::RemoveTool { tool_id } => {
                self.rules.remove(
                    &PermissionRule::new_tool(tool_id, ToolPermissionBehavior::Ask)
                        .subject
                        .key(),
                );
            }
            PermissionAction::RemoveRule { pattern } => {
                if let Ok(parsed) = parse_pattern(&pattern) {
                    self.rules.remove(&PermissionSubject::pattern(parsed).key());
                }
            }
            PermissionAction::ClearTools => self.rules.clear(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct LegacyPermissionOverrides {
    pub default_behavior: ToolPermissionBehavior,
    pub tools: HashMap<String, ToolPermissionBehavior>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct LegacyPermissionPolicy {
    pub default_behavior: ToolPermissionBehavior,
    pub allowed_tools: Vec<String>,
    pub denied_tools: Vec<String>,
}

/// Concrete [`tirea_contract::runtime::tool_call::ToolAccessGranter`] that creates
/// run-scoped permission overrides. Injected into skill tools via DI.
pub struct PermissionOverrideGranter;

impl tirea_contract::runtime::tool_call::ToolAccessGranter for PermissionOverrideGranter {
    fn grant_tool_override(&self, tool_id: &str) -> tirea_contract::runtime::state::AnyStateAction {
        permission_override_action(PermissionAction::SetTool {
            tool_id: tool_id.to_string(),
            behavior: ToolPermissionBehavior::Allow,
        })
    }
}

/// Target state for a permission rule change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDestination {
    /// Thread-scoped [`PermissionPolicy`] state (persistent across runs).
    Policy,
    /// Run-scoped [`PermissionOverrides`] state (auto-cleared at run end).
    Override,
}

/// Unified dispatch: routes a [`PermissionAction`] to the specified destination.
///
/// Replaces direct calls to [`permission_state_action()`] / [`permission_override_action()`].
pub fn permission_update(
    action: PermissionAction,
    destination: PermissionDestination,
) -> tirea_contract::runtime::state::AnyStateAction {
    match destination {
        PermissionDestination::Policy => permission_state_action(action),
        PermissionDestination::Override => permission_override_action(action),
    }
}

/// Route a [`PermissionAction`] to the canonical [`PermissionPolicy`] state.
pub fn permission_state_action(
    action: PermissionAction,
) -> tirea_contract::runtime::state::AnyStateAction {
    tirea_contract::runtime::state::AnyStateAction::new::<PermissionPolicy>(action)
}

/// Route a [`PermissionAction`] to the run-scoped [`PermissionOverrides`] state.
///
/// Use this instead of [`permission_state_action`] when the permission change
/// should be temporary and automatically cleaned up at the end of the current run
/// (e.g., skill-granted tool permissions).
///
/// `SetDefault` has no run-scoped equivalent; it is redirected to thread-level
/// [`PermissionPolicy`] instead.
pub fn permission_override_action(
    action: PermissionAction,
) -> tirea_contract::runtime::state::AnyStateAction {
    if matches!(action, PermissionAction::SetDefault { .. }) {
        return permission_state_action(action);
    }
    tirea_contract::runtime::state::AnyStateAction::new::<PermissionOverrides>(action)
}

/// Load resolved permission rules from a runtime snapshot.
///
/// Merges three layers with descending priority:
/// 1. Run-scoped [`PermissionOverrides`] (highest — temporary skill grants)
/// 2. Thread-level [`PermissionPolicy`] (base rules)
/// 3. Legacy `permissions` snapshot (lowest — backward compat)
#[must_use]
pub fn permission_rules_from_snapshot(snapshot: &serde_json::Value) -> PermissionRuleset {
    let mut ruleset = PermissionRuleset::default();
    let mut default_from_new_state = false;

    if let Some(policy_value) = snapshot.get(PermissionPolicy::PATH) {
        let prefers_legacy_shape = policy_value.get("allowed_tools").is_some()
            || policy_value.get("denied_tools").is_some();
        if prefers_legacy_shape {
            if let Ok(legacy_policy) =
                serde_json::from_value::<LegacyPermissionPolicy>(policy_value.clone())
            {
                default_from_new_state = true;
                ruleset.default_behavior = legacy_policy.default_behavior;
                for tool_id in legacy_policy.allowed_tools {
                    let rule = PermissionRule::new_tool(tool_id, ToolPermissionBehavior::Allow)
                        .with_source(PermissionRuleSource::Runtime);
                    ruleset.rules.entry(rule.subject.key()).or_insert(rule);
                }
                for tool_id in legacy_policy.denied_tools {
                    let rule = PermissionRule::new_tool(tool_id, ToolPermissionBehavior::Deny)
                        .with_source(PermissionRuleSource::Runtime);
                    ruleset.rules.insert(rule.subject.key(), rule);
                }
            }
        } else if let Ok(policy) = PermissionPolicy::from_value(policy_value) {
            default_from_new_state = true;
            ruleset.default_behavior = policy.default_behavior;
            ruleset.rules.extend(policy.rules);
        }
    }

    if let Some(legacy_value) = snapshot.get("permissions") {
        if let Ok(legacy) =
            serde_json::from_value::<LegacyPermissionOverrides>(legacy_value.clone())
        {
            if !default_from_new_state {
                ruleset.default_behavior = legacy.default_behavior;
            }
            for (tool_id, behavior) in legacy.tools {
                let rule = PermissionRule::new_tool(tool_id, behavior)
                    .with_source(PermissionRuleSource::Runtime);
                ruleset.rules.entry(rule.subject.key()).or_insert(rule);
            }
        }
    }

    // Apply run-scoped overrides last — they take highest priority.
    if let Some(overrides_value) = snapshot.get(PermissionOverrides::PATH) {
        if let Ok(overrides) = PermissionOverrides::from_value(overrides_value) {
            for (key, rule) in overrides.rules {
                ruleset.rules.insert(key, rule);
            }
        }
    }

    ruleset
}
