//! Tests for skill-triggered permission elevation.
//!
//! These tests verify the intended flow: when a skill declares `allowed-tools`,
//! activating that skill should grant temporary permission overrides via
//! `PermissionOverridesKey`. Currently, this flow is broken because:
//!
//! 1. `SkillActivateTool` parses `allowed_tools` but discards them
//! 2. No `AfterToolExecute` hook bridges tool result → permission state
//! 3. `grant_tool_override()` and `grant_rule_override()` exist but are never called
//!
//! These tests document the correct behavior and the current broken state.

use awaken_ext_permission::actions;
use awaken_ext_permission::rules::ToolPermissionBehavior;
use awaken_ext_permission::state::{
    PermissionAction, PermissionOverrides, PermissionOverridesKey, PermissionPolicy,
    PermissionPolicyKey, permission_rules_from_state,
};
use awaken_ext_permission::{PermissionRuleSource, evaluate_tool_permission};
use awaken_runtime::state::MutationBatch;
use awaken_runtime_contract::state::StateKey;

// ═══════════════════════════════════════════════════════════════════
// Permission Overrides State Tests
// ═══════════════════════════════════════════════════════════════════

/// Verify that PermissionOverridesKey uses `Skill` source when applying actions.
#[test]
fn overrides_use_skill_source() {
    let mut overrides = PermissionOverrides::default();
    PermissionOverridesKey::apply(
        &mut overrides,
        PermissionAction::AllowTool {
            tool_id: "Bash".into(),
        },
    );

    let rule = overrides.rules.get("tool:Bash").unwrap();
    assert_eq!(rule.source, PermissionRuleSource::Skill);
    assert_eq!(rule.behavior, ToolPermissionBehavior::Allow);
}

/// Verify overrides take priority over base policy in merged ruleset.
#[test]
fn overrides_take_priority_over_policy() {
    // Base policy denies Bash
    let mut policy = PermissionPolicy::default();
    PermissionPolicyKey::apply(
        &mut policy,
        PermissionAction::DenyTool {
            tool_id: "Bash".into(),
        },
    );

    // Override allows Bash (simulating skill elevation)
    let mut overrides = PermissionOverrides::default();
    PermissionOverridesKey::apply(
        &mut overrides,
        PermissionAction::AllowTool {
            tool_id: "Bash".into(),
        },
    );

    let ruleset = permission_rules_from_state(Some(&policy), Some(&overrides));
    let eval = evaluate_tool_permission(&ruleset, "Bash", &serde_json::json!({}));
    assert_eq!(
        eval.behavior,
        ToolPermissionBehavior::Allow,
        "Override should allow Bash even when policy denies it"
    );
}

/// Verify that without overrides, policy denial stands.
#[test]
fn policy_denial_without_override() {
    let mut policy = PermissionPolicy::default();
    PermissionPolicyKey::apply(
        &mut policy,
        PermissionAction::DenyTool {
            tool_id: "Bash".into(),
        },
    );

    let ruleset = permission_rules_from_state(Some(&policy), None);
    let eval = evaluate_tool_permission(&ruleset, "Bash", &serde_json::json!({}));
    assert_eq!(eval.behavior, ToolPermissionBehavior::Deny);
}

// ═══════════════════════════════════════════════════════════════════
// Action helpers for skill elevation
// ═══════════════════════════════════════════════════════════════════

/// Verify `grant_tool_override()` creates a non-empty mutation batch.
#[test]
fn grant_tool_override_creates_override_mutation() {
    let mut batch = MutationBatch::new();
    actions::grant_tool_override(&mut batch, "Bash");
    assert!(!batch.is_empty());
}

/// Verify `grant_rule_override()` creates a non-empty mutation batch.
#[test]
fn grant_rule_override_creates_override_mutation() {
    let mut batch = MutationBatch::new();
    actions::grant_rule_override(&mut batch, "mcp__*", ToolPermissionBehavior::Allow);
    assert!(!batch.is_empty());
}

/// Verify that SetDefault via overrides is ignored (only policy can set default).
#[test]
fn overrides_ignore_set_default() {
    let mut overrides = PermissionOverrides::default();
    PermissionOverridesKey::apply(
        &mut overrides,
        PermissionAction::SetDefault {
            behavior: ToolPermissionBehavior::Allow,
        },
    );
    // Should be empty — overrides don't support SetDefault
    assert!(overrides.rules.is_empty());
}

// ═══════════════════════════════════════════════════════════════════
// Skill elevation simulation tests
// ═══════════════════════════════════════════════════════════════════

/// Simulate what SHOULD happen when a skill with `allowed-tools: Bash Read Edit`
/// is activated. This is the intended flow that is currently broken.
#[test]
fn simulated_skill_elevation_flow() {
    // Step 1: Base policy denies everything
    let mut policy = PermissionPolicy::default();
    PermissionPolicyKey::apply(
        &mut policy,
        PermissionAction::SetDefault {
            behavior: ToolPermissionBehavior::Deny,
        },
    );

    // Step 2: Skill activation should grant allowed_tools via overrides
    // This is what an AfterToolExecute hook SHOULD do:
    let mut overrides = PermissionOverrides::default();
    let allowed_tools = vec!["Bash", "Read", "Edit"];
    for tool_id in &allowed_tools {
        PermissionOverridesKey::apply(
            &mut overrides,
            PermissionAction::AllowTool {
                tool_id: tool_id.to_string(),
            },
        );
    }

    // Step 3: Verify all declared tools are now allowed
    let ruleset = permission_rules_from_state(Some(&policy), Some(&overrides));

    for tool_id in &allowed_tools {
        let eval = evaluate_tool_permission(&ruleset, tool_id, &serde_json::json!({}));
        assert_eq!(
            eval.behavior,
            ToolPermissionBehavior::Allow,
            "Tool '{}' should be allowed after skill elevation",
            tool_id
        );
    }

    // Step 4: Tools NOT in the allowed list should still be denied
    let eval = evaluate_tool_permission(&ruleset, "DeleteFile", &serde_json::json!({}));
    assert_eq!(
        eval.behavior,
        ToolPermissionBehavior::Deny,
        "Tool 'DeleteFile' should still be denied"
    );
}

/// Simulate pattern-based skill elevation (e.g., `mcp__*`).
#[test]
fn simulated_pattern_skill_elevation() {
    let mut policy = PermissionPolicy::default();
    PermissionPolicyKey::apply(
        &mut policy,
        PermissionAction::SetDefault {
            behavior: ToolPermissionBehavior::Deny,
        },
    );

    let mut overrides = PermissionOverrides::default();
    PermissionOverridesKey::apply(
        &mut overrides,
        PermissionAction::SetRule {
            pattern: "mcp__*".into(),
            behavior: ToolPermissionBehavior::Allow,
        },
    );

    let ruleset = permission_rules_from_state(Some(&policy), Some(&overrides));

    // MCP tools should be allowed
    let eval = evaluate_tool_permission(&ruleset, "mcp__server__tool", &serde_json::json!({}));
    assert_eq!(eval.behavior, ToolPermissionBehavior::Allow);

    // Non-MCP tools should still be denied
    let eval = evaluate_tool_permission(&ruleset, "Bash", &serde_json::json!({}));
    assert_eq!(eval.behavior, ToolPermissionBehavior::Deny);
}

/// Multiple skill activations should accumulate overrides (commutative merge).
#[test]
fn multiple_skill_activations_accumulate_overrides() {
    let mut overrides = PermissionOverrides::default();

    // First skill grants Bash
    PermissionOverridesKey::apply(
        &mut overrides,
        PermissionAction::AllowTool {
            tool_id: "Bash".into(),
        },
    );

    // Second skill grants Read
    PermissionOverridesKey::apply(
        &mut overrides,
        PermissionAction::AllowTool {
            tool_id: "Read".into(),
        },
    );

    assert!(overrides.rules.contains_key("tool:Bash"));
    assert!(overrides.rules.contains_key("tool:Read"));
    assert_eq!(overrides.rules.len(), 2);
}

/// ClearTools on overrides should remove all skill-granted permissions.
#[test]
fn clear_tools_removes_all_skill_permissions() {
    let mut overrides = PermissionOverrides::default();
    PermissionOverridesKey::apply(
        &mut overrides,
        PermissionAction::AllowTool {
            tool_id: "Bash".into(),
        },
    );
    PermissionOverridesKey::apply(
        &mut overrides,
        PermissionAction::AllowTool {
            tool_id: "Read".into(),
        },
    );
    assert_eq!(overrides.rules.len(), 2);

    PermissionOverridesKey::apply(&mut overrides, PermissionAction::ClearTools);
    assert!(overrides.rules.is_empty());
}

// ═══════════════════════════════════════════════════════════════════
// MutationBatch integration
// ═══════════════════════════════════════════════════════════════════

/// Verify that batch actions can be used to grant skill permissions.
#[test]
fn batch_grant_and_apply() {
    let mut batch = MutationBatch::new();
    actions::grant_tool_override(&mut batch, "Bash");
    actions::grant_tool_override(&mut batch, "Read");
    actions::grant_rule_override(&mut batch, "mcp__*", ToolPermissionBehavior::Allow);

    // Three mutations should be queued
    assert!(!batch.is_empty());
}
