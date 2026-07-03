//! Behavioral integration tests for the Claude-Code-compatible permission policy.
//!
//! Ported (adapted) from the reference `awaken-ext-permission` suite. Only the
//! behaviors this crate's public API actually implements are covered here — the
//! reference's state-based policy/overrides merge, regex (`=~`) matching, and the
//! `actions::` mutation helpers are intentionally out of scope for this crate.

use awaken_ext_permission::{
    Mode, PermissionRule, PermissionRuleset, ToolCallPattern, ToolPermissionBehavior,
};
use serde_json::json;

fn rule(spec: &str, behavior: ToolPermissionBehavior) -> PermissionRule {
    PermissionRule::new(ToolCallPattern::parse(spec).unwrap(), behavior)
}

fn ruleset(default: ToolPermissionBehavior, rules: Vec<PermissionRule>) -> PermissionRuleset {
    PermissionRuleset {
        default_behavior: default,
        mode: Mode::Default,
        rules,
    }
}

// ---------------------------------------------------------------------------
// Matcher behaviors
// ---------------------------------------------------------------------------

#[test]
fn exact_tool_matches_any_args() {
    let p = ToolCallPattern::parse("Bash").unwrap();
    assert!(p.matches("Bash", &json!({"command": "ls"})).is_some());
    assert!(p.matches("Read", &json!({})).is_none());
}

#[test]
fn glob_tool_matches_namespace() {
    let p = ToolCallPattern::parse("mcp__*").unwrap();
    assert!(p.matches("mcp__github__issues", &json!({})).is_some());
    assert!(p.matches("read_file", &json!({})).is_none());
}

#[test]
fn glob_question_mark_matches_single_char() {
    // "?" matches exactly one character.
    let p = ToolCallPattern::parse("mcp_?_read").unwrap();
    assert!(p.matches("mcp_a_read", &json!({})).is_some());
    assert!(p.matches("mcp_ab_read", &json!({})).is_none());
}

#[test]
fn glob_char_class_matches_set() {
    // "[ab]" matches a single char from the class.
    let p = ToolCallPattern::parse("tool_[ab]").unwrap();
    assert!(p.matches("tool_a", &json!({})).is_some());
    assert!(p.matches("tool_b", &json!({})).is_some());
    assert!(p.matches("tool_c", &json!({})).is_none());
}

// ---------------------------------------------------------------------------
// Ruleset precedence behaviors
// ---------------------------------------------------------------------------

#[test]
fn pattern_deny_overrides_tool_allow() {
    let set = ruleset(
        ToolPermissionBehavior::Ask,
        vec![
            rule("Bash", ToolPermissionBehavior::Allow),
            rule("Bash(rm *)", ToolPermissionBehavior::Deny),
        ],
    );
    // rm → denied by the specific deny rule
    assert_eq!(
        set.decide("Bash", &json!({"command": "rm -rf /"})),
        ToolPermissionBehavior::Deny
    );
    // ls → only the broad allow matches
    assert_eq!(
        set.decide("Bash", &json!({"command": "ls -la"})),
        ToolPermissionBehavior::Allow
    );
}

#[test]
fn higher_specificity_allow_wins_within_same_tier() {
    let set = ruleset(
        ToolPermissionBehavior::Ask,
        vec![
            rule("Bash", ToolPermissionBehavior::Ask),
            rule("Bash(npm *)", ToolPermissionBehavior::Allow),
        ],
    );
    assert_eq!(
        set.decide("Bash", &json!({"command": "npm install"})),
        ToolPermissionBehavior::Allow
    );
    assert_eq!(
        set.decide("Bash", &json!({"command": "cargo build"})),
        ToolPermissionBehavior::Ask
    );
}

#[test]
fn deny_wins_amid_multiple_allow_rules() {
    // Ported from `multiple_rules_deny_wins_over_allow`: an allow-all, a specific
    // deny, and a specific allow together — deny is still absolute.
    let set = ruleset(
        ToolPermissionBehavior::Ask,
        vec![
            rule("Bash", ToolPermissionBehavior::Allow),
            rule("Bash(rm *)", ToolPermissionBehavior::Deny),
            rule("Bash(npm *)", ToolPermissionBehavior::Allow),
        ],
    );
    assert_eq!(
        set.decide("Bash", &json!({"command": "rm -rf /tmp"})),
        ToolPermissionBehavior::Deny
    );
    assert_eq!(
        set.decide("Bash", &json!({"command": "npm install"})),
        ToolPermissionBehavior::Allow
    );
}

// ---------------------------------------------------------------------------
// End-to-end parse → rule → decide
// ---------------------------------------------------------------------------

#[test]
fn mcp_glob_deny_and_default_fallthrough() {
    let set = ruleset(
        ToolPermissionBehavior::Ask,
        vec![rule("mcp__dangerous__*", ToolPermissionBehavior::Deny)],
    );
    assert_eq!(
        set.decide("mcp__dangerous__execute", &json!({})),
        ToolPermissionBehavior::Deny
    );
    // A non-matching tool falls to the default.
    assert_eq!(
        set.decide("mcp__safe__read", &json!({})),
        ToolPermissionBehavior::Ask
    );
}

#[test]
fn field_condition_deny_by_path() {
    let set = ruleset(
        ToolPermissionBehavior::Ask,
        vec![rule(
            "Edit(file_path ~ \"/etc/*\")",
            ToolPermissionBehavior::Deny,
        )],
    );
    assert_eq!(
        set.decide("Edit", &json!({"file_path": "/etc/passwd"})),
        ToolPermissionBehavior::Deny
    );
    assert_eq!(
        set.decide("Edit", &json!({"file_path": "src/main.rs"})),
        ToolPermissionBehavior::Ask
    );
}

#[test]
fn unsupported_regex_operator_is_rejected_not_silently_failing_open() {
    // The reference policy supported a `=~` regex operator; this crate is
    // glob-only. Parsing must reject `=~` rather than silently build a matcher
    // that matches nothing — otherwise a `Deny` rule ported from a regex config
    // would fail open. Regression guard for that fail-open.
    let err = ToolCallPattern::parse("Bash(command =~ \"rm\")").unwrap_err();
    assert!(
        err.contains("=~"),
        "the error should name the unsupported operator, got: {err}"
    );
    // A plain glob field match with `~` still parses.
    assert!(ToolCallPattern::parse("Bash(command ~ \"rm *\")").is_ok());
}

#[test]
fn ask_falls_through_when_no_rule_matches() {
    let set = ruleset(ToolPermissionBehavior::Ask, vec![]);
    assert_eq!(
        set.decide("Bash", &json!({"command": "echo hi"})),
        ToolPermissionBehavior::Ask
    );
}

#[test]
fn default_deny_blocks_all_unmatched_but_admits_specific_allow() {
    // Ported from `default_behavior_deny_blocks_all_unmatched`.
    let set = ruleset(
        ToolPermissionBehavior::Deny,
        vec![rule("Read", ToolPermissionBehavior::Allow)],
    );
    assert_eq!(
        set.decide("Read", &json!({})),
        ToolPermissionBehavior::Allow
    );
    assert_eq!(set.decide("Bash", &json!({})), ToolPermissionBehavior::Deny);
    assert_eq!(set.decide("Edit", &json!({})), ToolPermissionBehavior::Deny);
}
