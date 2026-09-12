//! Behavioral integration tests for the Claude-Code-compatible permission policy.
//!
//! Ported (adapted) from the reference `awaken-ext-permission` suite. Only the
//! behaviors this crate's public API actually implements are covered here — the
//! reference's state-based policy/overrides merge, regex (`=~`) matching, and the
//! `actions::` mutation helpers are intentionally out of scope for this crate.

use awaken_ext_permission::{
    PermissionRule, PermissionRuleset, RuleBasedToolPermissionPolicy, ToolCallPattern,
    ToolPermissionBehavior, parse_ruleset,
};
use awaken_runtime_contract::permission::{ToolCall, ToolPermissionPolicy, ToolPermissionVerdict};
use serde_json::json;

fn rule(spec: &str, behavior: ToolPermissionBehavior) -> PermissionRule {
    PermissionRule::new(ToolCallPattern::parse(spec).unwrap(), behavior)
}

fn ruleset(default: ToolPermissionBehavior, rules: Vec<PermissionRule>) -> PermissionRuleset {
    PermissionRuleset {
        default_behavior: default,
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
        ToolPermissionBehavior::RequireConfirmation,
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
        ToolPermissionBehavior::RequireConfirmation,
        vec![
            rule("Bash", ToolPermissionBehavior::RequireConfirmation),
            rule("Bash(npm *)", ToolPermissionBehavior::Allow),
        ],
    );
    assert_eq!(
        set.decide("Bash", &json!({"command": "npm install"})),
        ToolPermissionBehavior::Allow
    );
    assert_eq!(
        set.decide("Bash", &json!({"command": "cargo build"})),
        ToolPermissionBehavior::RequireConfirmation
    );
}

#[test]
fn deny_wins_amid_multiple_allow_rules() {
    // Ported from `multiple_rules_deny_wins_over_allow`: an allow-all, a specific
    // deny, and a specific allow together — deny is still absolute.
    let set = ruleset(
        ToolPermissionBehavior::RequireConfirmation,
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
        ToolPermissionBehavior::RequireConfirmation,
        vec![rule("mcp__dangerous__*", ToolPermissionBehavior::Deny)],
    );
    assert_eq!(
        set.decide("mcp__dangerous__execute", &json!({})),
        ToolPermissionBehavior::Deny
    );
    // A non-matching tool falls to the default.
    assert_eq!(
        set.decide("mcp__safe__read", &json!({})),
        ToolPermissionBehavior::RequireConfirmation
    );
}

#[test]
fn field_condition_deny_by_path() {
    let set = ruleset(
        ToolPermissionBehavior::RequireConfirmation,
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
        ToolPermissionBehavior::RequireConfirmation
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
    let set = ruleset(ToolPermissionBehavior::RequireConfirmation, vec![]);
    assert_eq!(
        set.decide("Bash", &json!({"command": "echo hi"})),
        ToolPermissionBehavior::RequireConfirmation
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

// ---------------------------------------------------------------------------
// Decision cross-product: deny-is-absolute regardless of specificity, mode
// defaults, and fail-closed edges. These are the security-critical rows —
// a wrong answer here is a fail-open (a side-effecting call escapes a deny).
// ---------------------------------------------------------------------------

#[test]
fn low_specificity_deny_beats_high_specificity_allow() {
    // The core security invariant: deny is absolute, so a *broad* deny must
    // still win over a *more specific* allow that also matches. If deny were
    // ranked by specificity instead of short-circuiting, the specific allow
    // would fail open here.
    let set = ruleset(
        ToolPermissionBehavior::RequireConfirmation,
        vec![
            // High specificity (tool + primary arg) allow.
            rule("Bash(npm *)", ToolPermissionBehavior::Allow),
            // Low specificity (bare tool) deny.
            rule("Bash", ToolPermissionBehavior::Deny),
        ],
    );
    assert_eq!(
        set.decide("Bash", &json!({"command": "npm install"})),
        ToolPermissionBehavior::Deny,
        "a broad deny must override a more specific allow"
    );
}

#[test]
fn deny_wins_irrespective_of_rule_order() {
    // Deny short-circuits regardless of whether it appears before or after the
    // allow it overrides — order must not flip a deny into an allow.
    let deny_first = ruleset(
        ToolPermissionBehavior::RequireConfirmation,
        vec![
            rule("Bash", ToolPermissionBehavior::Deny),
            rule("Bash(npm *)", ToolPermissionBehavior::Allow),
        ],
    );
    let allow_first = ruleset(
        ToolPermissionBehavior::RequireConfirmation,
        vec![
            rule("Bash(npm *)", ToolPermissionBehavior::Allow),
            rule("Bash", ToolPermissionBehavior::Deny),
        ],
    );
    let call = json!({"command": "npm install"});
    assert_eq!(
        deny_first.decide("Bash", &call),
        ToolPermissionBehavior::Deny
    );
    assert_eq!(
        allow_first.decide("Bash", &call),
        ToolPermissionBehavior::Deny
    );
}

#[test]
fn a_retired_bypass_section_can_no_longer_disarm_a_deny_rule() {
    // The retired `bypassPermissions` posture returned Allow *before* the rules
    // were scanned, so an explicit deny never fired. Compiling it into an
    // unmatched default keeps its permissive intent for unmatched calls while
    // restoring deny's absolute authority. That direction is a narrowing, which
    // is the safety win of removing the axis.
    let set = parse_ruleset(&json!({
        "mode": "bypassPermissions",
        "default_behavior": "deny",
        "rules": [ { "pattern": "bash(command ~ \"rm *\")", "behavior": "deny" } ]
    }))
    .unwrap();
    assert_eq!(
        set.decide("bash", &json!({"command": "rm -rf /"})),
        ToolPermissionBehavior::Deny,
        "the matching deny now decides"
    );
    assert_eq!(
        set.decide("write", &json!({})),
        ToolPermissionBehavior::Allow,
        "an unmatched call keeps the retired posture's allow"
    );
}

#[test]
fn a_retired_accept_edits_section_keeps_the_authored_default() {
    // AcceptEdits never had an edit-tool side-effect class, so it always behaved
    // as `default`. Compiling it must not invent one: an unmatched call takes the
    // authored default (NOT a plan-style deny).
    let set = parse_ruleset(&json!({
        "mode": "acceptEdits",
        "default_behavior": "ask",
        "rules": [ { "pattern": "read", "behavior": "allow" } ]
    }))
    .unwrap();
    assert_eq!(
        set.decide("edit", &json!({"file_path": "src/x.rs"})),
        ToolPermissionBehavior::RequireConfirmation
    );
    assert_eq!(
        set.decide("read", &json!({})),
        ToolPermissionBehavior::Allow
    );
}

#[test]
fn default_allow_admits_unmatched_calls() {
    // A permissive authored config (default_behavior = allow) is honored for an
    // unmatched call. Documents that the only "silent allow" is an explicit one.
    let set = ruleset(
        ToolPermissionBehavior::Allow,
        vec![rule("Bash(rm *)", ToolPermissionBehavior::Deny)],
    );
    assert_eq!(
        set.decide("WebFetch", &json!({})),
        ToolPermissionBehavior::Allow
    );
    // The deny still fires for its pattern.
    assert_eq!(
        set.decide("Bash", &json!({"command": "rm -rf /"})),
        ToolPermissionBehavior::Deny
    );
}

// ---------------------------------------------------------------------------
// Parse-time rejection of fail-open operators (whole-config path).
// ---------------------------------------------------------------------------

#[test]
fn parse_ruleset_rejects_regex_tool_name() {
    // A `/regex/` tool name is a regex matcher; the glob-only DSL rejects it at
    // parse time so a deny authored as a regex can't silently match nothing.
    let err = parse_ruleset(&json!({
        "rules": [ { "pattern": "/mcp__.*/", "behavior": "deny" } ]
    }))
    .unwrap_err();
    assert!(
        err.contains("regex tool names"),
        "error explains the regex tool-name rejection: {err}"
    );
}

#[test]
fn parse_ruleset_rejects_negated_regex_operator() {
    // `!=~` (negated regex) is equally unsupported and must be rejected, naming
    // the offending pattern.
    let err = parse_ruleset(&json!({
        "rules": [ { "pattern": "Bash(command !=~ \"rm\")", "behavior": "deny" } ]
    }))
    .unwrap_err();
    assert!(
        err.contains("Bash(command") && err.contains("!=~"),
        "error names the pattern and the unsupported operator: {err}"
    );
}

#[test]
fn parse_ruleset_rejects_malformed_pattern() {
    // A structurally broken pattern (unterminated arg group) is an error, never
    // silently dropped — a malformed deny must surface, not vanish.
    let err = parse_ruleset(&json!({
        "rules": [ { "pattern": "Bash(command ~ \"rm", "behavior": "deny" } ]
    }))
    .unwrap_err();
    assert!(
        err.contains("Bash(command"),
        "error names the malformed pattern: {err}"
    );
}

#[test]
fn negated_glob_and_exact_operators_are_accepted() {
    // Glob-only still admits the *negated non-regex* operators `!~` and `!=`.
    assert!(ToolCallPattern::parse("Bash(command !~ \"rm *\")").is_ok());
    assert!(ToolCallPattern::parse("Bash(command != \"rm\")").is_ok());
}

#[test]
fn negated_deny_matches_present_field_but_not_a_missing_one() {
    // Documents a fail-closed-to-*non-firing* edge: a `!~` deny ("deny anything
    // that is not `ls*`") fires when the field is present and non-matching, but
    // a call that OMITS the field resolves to no value → the condition is false
    // → the deny does not fire and the call falls to the default. See the
    // report's design note: negated deny rules do not catch missing fields.
    let set = ruleset(
        ToolPermissionBehavior::RequireConfirmation,
        vec![rule(
            "Bash(command !~ \"ls*\")",
            ToolPermissionBehavior::Deny,
        )],
    );
    // Present + not ls* → deny fires.
    assert_eq!(
        set.decide("Bash", &json!({"command": "rm -rf /"})),
        ToolPermissionBehavior::Deny
    );
    // Present + ls* → not denied, falls to default ask.
    assert_eq!(
        set.decide("Bash", &json!({"command": "ls -la"})),
        ToolPermissionBehavior::RequireConfirmation
    );
    // Missing field → deny does NOT fire; falls to default ask (documented gap).
    assert_eq!(
        set.decide("Bash", &json!({})),
        ToolPermissionBehavior::RequireConfirmation,
        "a negated deny does not catch a call that omits the field"
    );
}

#[test]
fn equal_specificity_first_rule_wins_allow_vs_ask() {
    // Documents the tie-break: two non-deny rules of identical specificity are
    // resolved by *order* (strictly-greater specificity replaces, equal does
    // not), so the first-listed rule wins. See report's design recommendation
    // to prefer the more restrictive behavior on ties.
    let allow_first = ruleset(
        ToolPermissionBehavior::Deny,
        vec![
            rule("Bash(npm *)", ToolPermissionBehavior::Allow),
            rule("Bash(npm *)", ToolPermissionBehavior::RequireConfirmation),
        ],
    );
    let ask_first = ruleset(
        ToolPermissionBehavior::Deny,
        vec![
            rule("Bash(npm *)", ToolPermissionBehavior::RequireConfirmation),
            rule("Bash(npm *)", ToolPermissionBehavior::Allow),
        ],
    );
    let call = json!({"command": "npm install"});
    assert_eq!(
        allow_first.decide("Bash", &call),
        ToolPermissionBehavior::Allow
    );
    assert_eq!(
        ask_first.decide("Bash", &call),
        ToolPermissionBehavior::RequireConfirmation
    );
}

// ---------------------------------------------------------------------------
// Serde/wire fail-open: mode/behavior tag parsing. A silent fall-through of an
// UNKNOWN tag to `Default`/`Ask` would be a fail-open (a typo'd `bypassPermissions`
// or `deny` quietly changing the policy's default). These pin that the wire form
// is exactly the serde renames and that anything else is REJECTED at parse.
// ---------------------------------------------------------------------------

#[test]
fn every_retired_mode_camel_case_tag_still_parses_and_compiles() {
    // A historical section must keep parsing after the axis was removed, and each
    // retired tag must compile into exactly one unmatched default.
    for (tag, expected) in [
        ("default", ToolPermissionBehavior::RequireConfirmation),
        ("acceptEdits", ToolPermissionBehavior::RequireConfirmation),
        ("plan", ToolPermissionBehavior::Deny),
        ("bypassPermissions", ToolPermissionBehavior::Allow),
    ] {
        let set = parse_ruleset(&json!({ "mode": tag, "default_behavior": "ask" })).unwrap();
        assert_eq!(
            set.decide("bash", &json!({"command": "rm -rf /"})),
            expected,
            "tag `{tag}` compiles into its documented unmatched default"
        );
    }
}

#[test]
fn an_unknown_mode_tag_is_rejected_not_silently_defaulted() {
    // A typo (`bypasspermissions`, wrong case) must be a hard parse error, never a
    // silent fall to the default posture. A `plan` typo that silently fell through
    // would DROP a read-only lock on a replayed historical section: a fail-open.
    let err = parse_ruleset(&json!({ "mode": "bypasspermissions" })).unwrap_err();
    assert!(
        err.contains("invalid permission config"),
        "unknown mode is a surfaced config error: {err}"
    );
}

#[test]
fn an_unknown_default_behavior_tag_is_rejected_not_silently_defaulted() {
    // A misspelled behavior (`deni`) must not silently fall to the `ask` default;
    // a dropped `deny` default would be a fail-open. Parse must reject it.
    let err = parse_ruleset(&json!({ "default_behavior": "deni" })).unwrap_err();
    assert!(
        err.contains("invalid permission config"),
        "unknown behavior is a surfaced config error: {err}"
    );
    // A misspelled rule behavior is equally rejected.
    let err = parse_ruleset(&json!({
        "rules": [ { "pattern": "Bash", "behavior": "denyy" } ]
    }))
    .unwrap_err();
    assert!(
        err.contains("invalid permission config"),
        "unknown rule behavior is rejected: {err}"
    );
}

// ---------------------------------------------------------------------------
// The async port impl (`RuleBasedToolPermissionPolicy`): Deny carries a reason naming the
// tool, and an Ask ticket is correlated to the call id so the operator's later
// decision resumes exactly this invocation (ADR-0030 D2).
// ---------------------------------------------------------------------------

fn policy_ctx(tool: &str, call_id: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        tool_id: tool.to_string(),
        call_id: call_id.to_string(),
        arguments: args,
    }
}

#[tokio::test]
async fn port_deny_reason_names_the_tool() {
    let policy = RuleBasedToolPermissionPolicy::new(ruleset(
        ToolPermissionBehavior::RequireConfirmation,
        vec![rule("Bash(rm *)", ToolPermissionBehavior::Deny)],
    ));
    match policy
        .evaluate(&policy_ctx("Bash", "c-9", json!({"command": "rm -rf /"})))
        .await
    {
        ToolPermissionVerdict::Deny { reason } => {
            assert!(
                reason.contains("Bash"),
                "deny reason names the tool: {reason}"
            );
            assert!(reason.contains("denied by policy"), "reason: {reason}");
        }
        other => panic!("expected deny, got {other:?}"),
    }
}

#[tokio::test]
async fn port_ask_ticket_is_correlated_to_the_call_id() {
    // An unmatched call under the default `ask` yields a ticket keyed to THIS
    // call id, so the resumed decision targets exactly this invocation.
    let policy = RuleBasedToolPermissionPolicy::new(ruleset(
        ToolPermissionBehavior::RequireConfirmation,
        vec![],
    ));
    for call_id in ["c-1", "call-42"] {
        match policy
            .evaluate(&policy_ctx("WebFetch", call_id, json!({})))
            .await
        {
            ToolPermissionVerdict::RequireConfirmation { correlation_id } => {
                assert_eq!(correlation_id, format!("perm-{call_id}"));
            }
            other => panic!("expected ask, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn port_allow_maps_to_allow_decision() {
    let policy = RuleBasedToolPermissionPolicy::new(ruleset(
        ToolPermissionBehavior::Deny,
        vec![rule("Read", ToolPermissionBehavior::Allow)],
    ));
    assert!(matches!(
        policy.evaluate(&policy_ctx("Read", "c-1", json!({}))).await,
        ToolPermissionVerdict::Allow
    ));
}

// ---------------------------------------------------------------------------
// MCP toolset identity: a disabled toolset must reach the decision
// ---------------------------------------------------------------------------

/// A permission ruleset whose unmatched behavior is the common, permissive one.
/// This is the substrate the bypass rode on: when the MCP toolset lookup
/// returned `None`, the call matched no rule and fell straight to `Allow`.
fn allow_when_unmatched() -> PermissionRuleset {
    PermissionRuleset {
        default_behavior: ToolPermissionBehavior::Allow,
        rules: Vec::new(),
    }
}

fn disabled_mcp_toolset(
    server_name: &str,
) -> awaken_runtime_contract::agent_bindings::ToolsetPolicy {
    awaken_runtime_contract::agent_bindings::ToolsetPolicy {
        source: awaken_runtime_contract::agent_bindings::ToolsetSource::Mcp {
            server_name: server_name.into(),
        },
        default: awaken_runtime_contract::agent_bindings::ToolExecutionPolicy {
            enabled: false,
            permission:
                awaken_runtime_contract::agent_bindings::ToolPermissionRequirement::AlwaysAsk,
        },
        overrides: Vec::new(),
    }
}

#[tokio::test]
async fn a_disabled_mcp_toolset_denies_however_its_server_name_is_punctuated() {
    // The operator disabled the toolset. Whatever punctuation the server name
    // carries, the tool the producer emits for it must reach a Deny — never the
    // permissive unmatched default.
    for server_name in ["github-mcp", "githubmcp", "github.mcp", "api.example.com"] {
        let policy = RuleBasedToolPermissionPolicy::new(allow_when_unmatched())
            .with_mcp_toolsets(&[disabled_mcp_toolset(server_name)]);
        let tool_id = awaken_agent_contract::McpToolId::new(server_name, "search")
            .expect("canonical id")
            .as_str()
            .to_string();
        let verdict = policy
            .evaluate(&ToolCall {
                call_id: "bypass".into(),
                tool_id: tool_id.clone(),
                arguments: json!({}),
            })
            .await;
        assert!(
            matches!(verdict, ToolPermissionVerdict::Deny { .. }),
            "disabled toolset on {server_name:?} must deny {tool_id}, got {verdict:?}"
        );
    }
}

#[tokio::test]
async fn a_toolset_never_governs_a_differently_punctuated_neighbour() {
    let policy = RuleBasedToolPermissionPolicy::new(allow_when_unmatched())
        .with_mcp_toolsets(&[disabled_mcp_toolset("a-b")]);
    for neighbour in ["a.b", "a_b"] {
        let tool_id = awaken_agent_contract::McpToolId::new(neighbour, "search")
            .expect("canonical id")
            .as_str()
            .to_string();
        assert!(
            matches!(
                policy
                    .evaluate(&ToolCall {
                        call_id: "neighbour".into(),
                        tool_id,
                        arguments: json!({}),
                    })
                    .await,
                ToolPermissionVerdict::Allow
            ),
            "the policy authored for \"a-b\" must not reach {neighbour:?}"
        );
    }
}
