//! Integration tests for awaken-ext-permission.

use awaken_ext_permission::*;
use awaken_runtime::state::{MutationBatch, StateKey};
use serde_json::json;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Matcher integration tests
// ---------------------------------------------------------------------------

#[test]
fn matcher_exact_tool_any_args() {
    let p = ToolCallPattern::tool("Bash");
    let result = matcher::pattern_matches(&p, "Bash", &json!({"command": "ls"}));
    assert!(result.is_match());
}

#[test]
fn matcher_glob_tool_any_args() {
    let p = ToolCallPattern::tool_glob("mcp__*");
    let result = matcher::pattern_matches(&p, "mcp__github__issues", &json!({}));
    assert!(result.is_match());
    let result2 = matcher::pattern_matches(&p, "read_file", &json!({}));
    assert!(!result2.is_match());
}

#[test]
fn matcher_primary_with_exact_op() {
    let p = ToolCallPattern {
        tool: ToolMatcher::Exact("Bash".into()),
        args: ArgMatcher::Primary {
            op: MatchOp::Exact,
            value: "git status".into(),
        },
    };
    assert!(matcher::pattern_matches(&p, "Bash", &json!({"command": "git status"})).is_match());
    assert!(
        !matcher::pattern_matches(&p, "Bash", &json!({"command": "git status --short"})).is_match()
    );
}

#[test]
fn matcher_primary_with_regex_op() {
    let p = ToolCallPattern {
        tool: ToolMatcher::Exact("Bash".into()),
        args: ArgMatcher::Primary {
            op: MatchOp::Regex,
            value: "^(npm|yarn) ".into(),
        },
    };
    assert!(matcher::pattern_matches(&p, "Bash", &json!({"command": "npm install"})).is_match());
    assert!(matcher::pattern_matches(&p, "Bash", &json!({"command": "yarn add foo"})).is_match());
    assert!(!matcher::pattern_matches(&p, "Bash", &json!({"command": "cargo build"})).is_match());
}

// ---------------------------------------------------------------------------
// Ruleset evaluation integration tests
// ---------------------------------------------------------------------------

#[test]
fn ruleset_pattern_deny_overrides_tool_allow() {
    let mut ruleset = PermissionRuleset::default();
    ruleset.rules.insert(
        "tool:Bash".into(),
        PermissionRule::new_tool("Bash", ToolPermissionBehavior::Allow),
    );
    ruleset.rules.insert(
        "pattern:Bash(rm *)".into(),
        PermissionRule::new_pattern(
            ToolCallPattern::tool_with_primary("Bash", "rm *"),
            ToolPermissionBehavior::Deny,
        ),
    );

    // rm command → denied
    let eval = evaluate_tool_permission(&ruleset, "Bash", &json!({"command": "rm -rf /"}));
    assert_eq!(eval.behavior, ToolPermissionBehavior::Deny);

    // ls command → allowed (only tool:Bash matches)
    let eval2 = evaluate_tool_permission(&ruleset, "Bash", &json!({"command": "ls -la"}));
    assert_eq!(eval2.behavior, ToolPermissionBehavior::Allow);
}

#[test]
fn ruleset_higher_specificity_wins_within_same_tier() {
    let mut ruleset = PermissionRuleset::default();
    // Glob pattern: allow all Bash
    ruleset.rules.insert(
        "pattern:Bash(*)".into(),
        PermissionRule::new_pattern(ToolCallPattern::tool("Bash"), ToolPermissionBehavior::Allow),
    );
    // More specific: allow Bash with npm commands
    ruleset.rules.insert(
        "pattern:Bash(npm *)".into(),
        PermissionRule::new_pattern(
            ToolCallPattern::tool_with_primary("Bash", "npm *"),
            ToolPermissionBehavior::Allow,
        ),
    );

    let eval = evaluate_tool_permission(&ruleset, "Bash", &json!({"command": "npm install"}));
    assert_eq!(eval.behavior, ToolPermissionBehavior::Allow);
    // Matched rule should be the more specific one
    assert!(eval.matched_rule.is_some());
}

#[test]
fn ruleset_glob_tool_deny() {
    let mut ruleset = PermissionRuleset::default();
    ruleset.rules.insert(
        "pattern:mcp__*".into(),
        PermissionRule::new_pattern(
            ToolCallPattern::tool_glob("mcp__*"),
            ToolPermissionBehavior::Deny,
        ),
    );

    let eval = evaluate_tool_permission(&ruleset, "mcp__github__create_issue", &json!({}));
    assert_eq!(eval.behavior, ToolPermissionBehavior::Deny);

    let eval2 = evaluate_tool_permission(&ruleset, "read_file", &json!({}));
    assert_eq!(eval2.behavior, ToolPermissionBehavior::Ask); // default
}

// ---------------------------------------------------------------------------
// State integration tests
// ---------------------------------------------------------------------------

#[test]
fn state_policy_and_overrides_merge_correctly() {
    let mut policy = PermissionPolicy::default();
    policy.default_behavior = ToolPermissionBehavior::Ask;

    // Policy: allow Bash
    PermissionPolicyKey::apply(
        &mut policy,
        PermissionAction::AllowTool {
            tool_id: "Bash".into(),
        },
    );

    // Policy: deny rm tool
    PermissionPolicyKey::apply(
        &mut policy,
        PermissionAction::DenyTool {
            tool_id: "rm".into(),
        },
    );

    let mut overrides = PermissionOverrides::default();
    // Override: allow rm (overrides policy deny)
    PermissionOverridesKey::apply(
        &mut overrides,
        PermissionAction::AllowTool {
            tool_id: "rm".into(),
        },
    );

    let ruleset = permission_rules_from_state(Some(&policy), Some(&overrides));
    assert_eq!(ruleset.default_behavior, ToolPermissionBehavior::Ask);

    // Bash is still allowed from policy
    let bash_rule = ruleset.rules.get("tool:Bash").unwrap();
    assert_eq!(bash_rule.behavior, ToolPermissionBehavior::Allow);

    // rm is now allowed because override wins
    let rm_rule = ruleset.rules.get("tool:rm").unwrap();
    assert_eq!(rm_rule.behavior, ToolPermissionBehavior::Allow);
}

#[test]
fn state_overrides_dont_leak_default_behavior() {
    let mut policy = PermissionPolicy::default();
    policy.default_behavior = ToolPermissionBehavior::Deny;

    let mut overrides = PermissionOverrides::default();
    // SetDefault is ignored in overrides
    PermissionOverridesKey::apply(
        &mut overrides,
        PermissionAction::SetDefault {
            behavior: ToolPermissionBehavior::Allow,
        },
    );

    let ruleset = permission_rules_from_state(Some(&policy), Some(&overrides));
    // Default should still be Deny from policy
    assert_eq!(ruleset.default_behavior, ToolPermissionBehavior::Deny);
}

// ---------------------------------------------------------------------------
// Actions integration tests
// ---------------------------------------------------------------------------

#[test]
fn actions_set_default_behavior() {
    let mut batch = MutationBatch::new();
    actions::set_default_behavior(&mut batch, ToolPermissionBehavior::Deny);
    assert!(!batch.is_empty());
}

#[test]
fn actions_remove_tool() {
    let mut batch = MutationBatch::new();
    actions::remove_tool(&mut batch, "Bash");
    assert!(!batch.is_empty());
}

#[test]
fn actions_remove_rule() {
    let mut batch = MutationBatch::new();
    actions::remove_rule(&mut batch, "Bash(npm *)");
    assert!(!batch.is_empty());
}

#[test]
fn actions_grant_rule_override() {
    let mut batch = MutationBatch::new();
    actions::grant_rule_override(
        &mut batch,
        r#"Edit(file_path ~ "src/**")"#,
        ToolPermissionBehavior::Allow,
    );
    assert!(!batch.is_empty());
}

// ---------------------------------------------------------------------------
// End-to-end: parse → rule → evaluate
// ---------------------------------------------------------------------------

#[test]
fn end_to_end_parse_and_evaluate() {
    let pattern = parse_pattern(r#"Bash(command ~ "npm *")"#).unwrap();
    let rule = PermissionRule::new_pattern(pattern, ToolPermissionBehavior::Allow);

    let mut ruleset = PermissionRuleset {
        default_behavior: ToolPermissionBehavior::Ask,
        rules: HashMap::new(),
    };
    ruleset.rules.insert(rule.subject.key(), rule);

    let eval = evaluate_tool_permission(&ruleset, "Bash", &json!({"command": "npm install"}));
    assert_eq!(eval.behavior, ToolPermissionBehavior::Allow);

    let eval2 = evaluate_tool_permission(&ruleset, "Bash", &json!({"command": "cargo build"}));
    assert_eq!(eval2.behavior, ToolPermissionBehavior::Ask);
}

#[test]
fn end_to_end_deny_dangerous_commands() {
    let deny_rm = parse_pattern(r#"Bash(command ~ "rm *")"#).unwrap();
    let deny_eval = parse_pattern(r#"Bash(command =~ "(?i)eval|exec")"#).unwrap();
    let allow_bash = ToolCallPattern::tool("Bash");

    let mut ruleset = PermissionRuleset {
        default_behavior: ToolPermissionBehavior::Ask,
        rules: HashMap::new(),
    };

    let r1 = PermissionRule::new_pattern(deny_rm, ToolPermissionBehavior::Deny);
    ruleset.rules.insert(r1.subject.key(), r1);

    let r2 = PermissionRule::new_pattern(deny_eval, ToolPermissionBehavior::Deny);
    ruleset.rules.insert(r2.subject.key(), r2);

    let r3 = PermissionRule::new_pattern(allow_bash, ToolPermissionBehavior::Allow);
    ruleset.rules.insert(r3.subject.key(), r3);

    // rm → denied
    let eval = evaluate_tool_permission(&ruleset, "Bash", &json!({"command": "rm -rf /"}));
    assert_eq!(eval.behavior, ToolPermissionBehavior::Deny);

    // eval → denied
    let eval2 = evaluate_tool_permission(&ruleset, "Bash", &json!({"command": "eval malicious"}));
    assert_eq!(eval2.behavior, ToolPermissionBehavior::Deny);

    // ls → allowed (matches allow_bash, no deny matches)
    let eval3 = evaluate_tool_permission(&ruleset, "Bash", &json!({"command": "ls -la"}));
    assert_eq!(eval3.behavior, ToolPermissionBehavior::Allow);
}

#[test]
fn end_to_end_mcp_tool_glob_deny() {
    let pattern = parse_pattern("mcp__dangerous__*").unwrap();
    let rule = PermissionRule::new_pattern(pattern, ToolPermissionBehavior::Deny);

    let mut ruleset = PermissionRuleset::default();
    ruleset.rules.insert(rule.subject.key(), rule);

    let eval = evaluate_tool_permission(&ruleset, "mcp__dangerous__execute", &json!({}));
    assert_eq!(eval.behavior, ToolPermissionBehavior::Deny);

    let eval2 = evaluate_tool_permission(&ruleset, "mcp__safe__read", &json!({}));
    assert_eq!(eval2.behavior, ToolPermissionBehavior::Ask);
}

#[test]
fn end_to_end_field_condition_deny() {
    let pattern = parse_pattern(r#"Edit(file_path ~ "/etc/*")"#).unwrap();
    let rule = PermissionRule::new_pattern(pattern, ToolPermissionBehavior::Deny);

    let mut ruleset = PermissionRuleset::default();
    ruleset.rules.insert(rule.subject.key(), rule);

    let eval = evaluate_tool_permission(&ruleset, "Edit", &json!({"file_path": "/etc/passwd"}));
    assert_eq!(eval.behavior, ToolPermissionBehavior::Deny);

    let eval2 = evaluate_tool_permission(&ruleset, "Edit", &json!({"file_path": "src/main.rs"}));
    assert_eq!(eval2.behavior, ToolPermissionBehavior::Ask);
}

// ---------------------------------------------------------------------------
// Serde integration
// ---------------------------------------------------------------------------

#[test]
fn permission_rule_serde_roundtrip() {
    let rule = PermissionRule::new_pattern(
        parse_pattern(r#"Bash(command ~ "npm *")"#).unwrap(),
        ToolPermissionBehavior::Allow,
    )
    .with_scope(PermissionRuleScope::Session)
    .with_source(PermissionRuleSource::User);

    let json = serde_json::to_value(&rule).unwrap();
    let decoded: PermissionRule = serde_json::from_value(json).unwrap();

    assert_eq!(decoded.behavior, ToolPermissionBehavior::Allow);
    assert_eq!(decoded.scope, PermissionRuleScope::Session);
    assert_eq!(decoded.source, PermissionRuleSource::User);
}

#[test]
fn permission_action_serde_roundtrip() {
    let action = PermissionAction::SetRule {
        pattern: "Bash(npm *)".into(),
        behavior: ToolPermissionBehavior::Allow,
    };
    let json = serde_json::to_value(&action).unwrap();
    let decoded: PermissionAction = serde_json::from_value(json).unwrap();
    assert_eq!(decoded, action);
}

#[test]
fn permission_action_deny_tool_serde() {
    let action = PermissionAction::DenyTool {
        tool_id: "rm".into(),
    };
    let json = serde_json::to_value(&action).unwrap();
    let decoded: PermissionAction = serde_json::from_value(json).unwrap();
    assert_eq!(decoded, action);
}
