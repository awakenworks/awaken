//! Behavioral integration tests for the Claude-Code-compatible permission policy.
//!
//! Ported (adapted) from the reference `awaken-ext-permission` suite. Only the
//! behaviors this crate's public API actually implements are covered here — the
//! reference's state-based policy/overrides merge, regex (`=~`) matching, and the
//! `actions::` mutation helpers are intentionally out of scope for this crate.

use awaken_ext_permission::{
    AutoDecision, AutoPermissionEvaluator, ObservingToolEvaluator, PermissionRule,
    PermissionRuleset, RuleBasedToolPermissionPolicy, ToolCallPattern, ToolPermissionBehavior,
    parse_ruleset,
};
use awaken_runtime_contract::agent_bindings::ToolPermissionRequirement;
use awaken_runtime_contract::permission::{
    PermissionBasis, PermissionPolicy, PermissionReasonCode, RulingAuthority, ToolCall,
    ToolPermissionPolicy, ToolPermissionVerdict,
};
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
        .verdict
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
            .verdict
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
        policy
            .evaluate(&policy_ctx("Read", "c-1", json!({})))
            .await
            .verdict,
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
            .await
            .verdict;
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
                    .await
                    .verdict,
                ToolPermissionVerdict::Allow
            ),
            "the policy authored for \"a-b\" must not reach {neighbour:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// The `auto` policy: a fixed authored decision always outranks the evaluator,
// and the evaluator's own three outcomes each reach the caller intact.
// ---------------------------------------------------------------------------

/// Answers with one fixed decision, so a test states the evaluator's verdict
/// directly instead of depending on any classifier's judgement.
#[derive(Debug)]
struct FixedAuto(AutoDecision);
impl AutoPermissionEvaluator for FixedAuto {
    fn evaluate(&self, _call: &awaken_runtime_contract::permission::ToolCall) -> AutoDecision {
        self.0
    }
}

fn mcp_toolset(
    default: ToolPermissionRequirement,
) -> awaken_runtime_contract::agent_bindings::ToolsetPolicy {
    use awaken_runtime_contract::agent_bindings::{
        ToolExecutionPolicy, ToolsetPolicy, ToolsetSource,
    };
    ToolsetPolicy {
        source: ToolsetSource::Mcp {
            server_name: "context".into(),
        },
        default: ToolExecutionPolicy {
            enabled: true,
            permission: default,
        },
        overrides: Vec::new(),
    }
}

fn auto_call() -> ToolCall {
    ToolCall {
        tool_id: "mcp__context__attest".into(),
        call_id: "auto-1".into(),
        arguments: json!({}),
    }
}

#[tokio::test]
async fn each_auto_outcome_reaches_the_caller_unchanged() {
    // Cause/effect graph: C1 the resolved policy is `auto`; C2 the evaluator
    // returns allow/deny/indeterminate. E1 allow runs, E2 deny refuses the call
    // outright, E3 indeterminate falls through to a person — the same ask the
    // `always_ask` policy produces, correlated to this exact call.
    //
    // | Rule | C2 | Effect |
    // | A1 | allow | E1 Allow |
    // | A2 | deny | E2 Deny |
    // | A3 | indeterminate | E3 ask, correlated |
    for (rule, decision) in [
        ("A1", AutoDecision::Allow),
        ("A2", AutoDecision::Deny),
        ("A3", AutoDecision::Indeterminate),
    ] {
        let policy = RuleBasedToolPermissionPolicy::new(PermissionRuleset::default())
            .with_mcp_toolsets(&[mcp_toolset(ToolPermissionRequirement::Auto)])
            .with_auto_evaluator(std::sync::Arc::new(FixedAuto(decision)));
        match (decision, policy.evaluate(&auto_call()).await.verdict) {
            (AutoDecision::Allow, ToolPermissionVerdict::Allow) => {}
            (AutoDecision::Deny, ToolPermissionVerdict::Deny { .. }) => {}
            (
                AutoDecision::Indeterminate,
                ToolPermissionVerdict::RequireConfirmation { correlation_id },
            ) => assert_eq!(correlation_id, "perm-auto-1", "{rule}"),
            (decision, verdict) => panic!("{rule}: {decision:?} produced {verdict:?}"),
        }
    }
}

#[tokio::test]
async fn a_fixed_authored_policy_outranks_the_evaluator() {
    // The authored decision is the author's own instruction; letting an
    // evaluator overrule it would replace a stated intent with a judgement.
    // Both fixed policies must hold even against an evaluator that would answer
    // the opposite way for every call.
    //
    // | Rule | authored | evaluator would say | Effect |
    // | F1 | always_allow | deny | allow (the author decided) |
    // | F2 | always_ask | allow | ask (the author decided) |
    let allow_policy = RuleBasedToolPermissionPolicy::new(PermissionRuleset::default())
        .with_mcp_toolsets(&[mcp_toolset(ToolPermissionRequirement::AlwaysAllow)])
        .with_auto_evaluator(std::sync::Arc::new(FixedAuto(AutoDecision::Deny)));
    assert!(
        matches!(
            allow_policy.evaluate(&auto_call()).await.verdict,
            ToolPermissionVerdict::Allow
        ),
        "F1"
    );

    let ask_policy = RuleBasedToolPermissionPolicy::new(PermissionRuleset::default())
        .with_mcp_toolsets(&[mcp_toolset(ToolPermissionRequirement::AlwaysAsk)])
        .with_auto_evaluator(std::sync::Arc::new(FixedAuto(AutoDecision::Allow)));
    assert!(
        matches!(
            ask_policy.evaluate(&auto_call()).await.verdict,
            ToolPermissionVerdict::RequireConfirmation { .. }
        ),
        "F2"
    );
}

#[tokio::test]
async fn an_authored_deny_rule_still_outranks_an_auto_allow() {
    // `deny` is absolute across the whole rule scan. An evaluator that would
    // allow the call cannot resurrect it — the same guarantee the retired
    // bypass posture used to break.
    let policy = RuleBasedToolPermissionPolicy::new(ruleset(
        ToolPermissionBehavior::RequireConfirmation,
        vec![rule("mcp__context__attest", ToolPermissionBehavior::Deny)],
    ))
    .with_mcp_toolsets(&[mcp_toolset(ToolPermissionRequirement::Auto)])
    .with_auto_evaluator(std::sync::Arc::new(FixedAuto(AutoDecision::Allow)));
    assert!(matches!(
        policy.evaluate(&auto_call()).await.verdict,
        ToolPermissionVerdict::Deny { .. }
    ));
}

#[test]
fn the_default_evaluator_allows_only_observing_tools_and_never_denies() {
    // The shipped classifier judges by tool identity alone, so it may conclude
    // "safe to observe" but never "too risky for anyone to approve": an
    // unknown tool goes to a person, not to a refusal.
    let evaluator = ObservingToolEvaluator::new(["read".to_string(), "grep".to_string()]);
    let call = |tool: &str| ToolCall {
        tool_id: tool.into(),
        call_id: "c".into(),
        arguments: json!({}),
    };
    assert_eq!(evaluator.evaluate(&call("read")), AutoDecision::Allow);
    assert_eq!(
        evaluator.evaluate(&call("bash")),
        AutoDecision::Indeterminate
    );
    assert_eq!(
        ObservingToolEvaluator::default().evaluate(&call("read")),
        AutoDecision::Indeterminate,
        "an unconfigured evaluator names no safe tool"
    );
}

#[tokio::test]
async fn a_disabled_tool_is_denied_before_the_evaluator_is_ever_consulted() {
    // The one gap that pointed at fail-open. "Disabled" and "auto" are judged in
    // two places — the host decides which members become rules, and this policy
    // decides which calls reach the evaluator — so nothing but a test keeps the
    // two agreeing. If either drifts, a tool the author switched OFF becomes a
    // tool the platform may switch back on.
    //
    // Cause/effect graph: C1 the resolved policy is disabled; C2 its permission
    // is `auto`; C3 the evaluator would allow. E1 Deny, and the evaluator is
    // never asked — proved by an evaluator that panics if consulted.
    //
    // | Rule | C1 | C2 | C3 | Effect |
    // |---|---|---|---|---|
    // | X1 | T | T | T | E1 Deny, evaluator untouched |
    // | X2 | T | F(always_allow) | * | E1 Deny — disabled outranks allow too |
    #[derive(Debug)]
    struct NeverAsk;
    impl AutoPermissionEvaluator for NeverAsk {
        fn evaluate(&self, call: &ToolCall) -> AutoDecision {
            panic!(
                "the evaluator must not be consulted for the disabled tool {}",
                call.tool_id
            );
        }
    }

    for (rule, permission) in [
        ("X1", ToolPermissionRequirement::Auto),
        ("X2", ToolPermissionRequirement::AlwaysAllow),
    ] {
        let policy = RuleBasedToolPermissionPolicy::new(PermissionRuleset::default())
            .with_mcp_toolsets(&[awaken_runtime_contract::agent_bindings::ToolsetPolicy {
                source: awaken_runtime_contract::agent_bindings::ToolsetSource::Mcp {
                    server_name: "context".into(),
                },
                default: awaken_runtime_contract::agent_bindings::ToolExecutionPolicy {
                    enabled: false,
                    permission,
                },
                overrides: Vec::new(),
            }])
            .with_auto_evaluator(std::sync::Arc::new(NeverAsk));
        assert!(
            matches!(
                policy.evaluate(&auto_call()).await.verdict,
                ToolPermissionVerdict::Deny { .. }
            ),
            "{rule}: a disabled tool is denied without consulting the evaluator"
        );
    }
}

#[tokio::test]
async fn a_closed_toolset_deferral_and_an_open_toolset_auto_behave_identically() {
    // The two deferral paths (named closed members vs per-call open resolution)
    // must produce the same decision for the same evaluator verdict, or the
    // authored meaning of `auto` would depend on which toolset a tool came from.
    //
    // | Rule | path | evaluator | Effect |
    // |---|---|---|---|
    // | P1 | closed (named member) | allow | allow |
    // | P2 | open (resolved per call) | allow | allow |
    // | P3 | closed (named member) | indeterminate | ask |
    // | P4 | open (resolved per call) | indeterminate | ask |
    for (decision, expect_allow) in [
        (AutoDecision::Allow, true),
        (AutoDecision::Indeterminate, false),
    ] {
        let closed = RuleBasedToolPermissionPolicy::new(PermissionRuleset::default())
            .with_deferred_members(["bash".to_string()])
            .with_auto_evaluator(std::sync::Arc::new(FixedAuto(decision)));
        let closed_call = ToolCall {
            tool_id: "bash".into(),
            call_id: "c".into(),
            arguments: json!({}),
        };
        let open = RuleBasedToolPermissionPolicy::new(PermissionRuleset::default())
            .with_mcp_toolsets(&[mcp_toolset(ToolPermissionRequirement::Auto)])
            .with_auto_evaluator(std::sync::Arc::new(FixedAuto(decision)));

        for (label, verdict) in [
            ("closed", closed.evaluate(&closed_call).await.verdict),
            ("open", open.evaluate(&auto_call()).await.verdict),
        ] {
            if expect_allow {
                assert!(
                    matches!(verdict, ToolPermissionVerdict::Allow),
                    "{label} path under {decision:?}"
                );
            } else {
                assert!(
                    matches!(verdict, ToolPermissionVerdict::RequireConfirmation { .. }),
                    "{label} path under {decision:?}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The basis every ruling states (ADR-0030 D4)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_ruling_names_the_policy_that_decided_and_the_auto_grounds() {
    // Cause/effect graph: C1 the resolved toolset requirement fixed the answer;
    // C2 the requirement is `auto` and the evaluator allowed / refused / could
    // not judge; C3 an authored pattern rule matched; C4 nothing matched and
    // the ruleset default decided; C5 the toolset disabled the tool. Effects:
    // E1 the ruling's basis names the requirement; E2 an `auto` refusal carries
    // `high_risk` and an `auto` ask carries `indeterminate`, an `auto`
    // allow carries none; E3 an authored rule, the authored default and a
    // disablement all name `rule` — an instruction outside the toolset policy
    // vocabulary.
    //
    // | Rule | source | evaluator | verdict | policy | reason_code |
    // |---|---|---|---|---|---|
    // | B1 | always_allow | (ignored) | Allow | always_allow | - |
    // | B2 | always_ask | (ignored) | Ask | always_ask | - |
    // | B3 | auto | allow | Allow | auto | - |
    // | B4 | auto | deny | Deny | auto | high_risk |
    // | B5 | auto | indeterminate | Ask | auto | indeterminate |
    // | B6 | authored `Read` allow rule | - | Allow | rule | - |
    // | B7 | ruleset default deny | - | Deny | rule | - |
    // | B8 | toolset disabled | - | Deny | rule | - |
    let expect = |rule: &str,
                  ruling: awaken_runtime_contract::permission::ToolPermissionRuling,
                  verdict: fn(&ToolPermissionVerdict) -> bool,
                  basis: PermissionBasis| {
        assert!(verdict(&ruling.verdict), "{rule}: {:?}", ruling.verdict);
        assert_eq!(ruling.authority, RulingAuthority::Policy(basis), "{rule}");
    };
    let is_allow = |v: &ToolPermissionVerdict| matches!(v, ToolPermissionVerdict::Allow);
    let is_ask =
        |v: &ToolPermissionVerdict| matches!(v, ToolPermissionVerdict::RequireConfirmation { .. });
    let is_deny = |v: &ToolPermissionVerdict| matches!(v, ToolPermissionVerdict::Deny { .. });

    let fixed = |requirement| {
        RuleBasedToolPermissionPolicy::new(PermissionRuleset::default())
            .with_mcp_toolsets(&[mcp_toolset(requirement)])
            .with_auto_evaluator(std::sync::Arc::new(FixedAuto(AutoDecision::Deny)))
    };
    expect(
        "B1",
        fixed(ToolPermissionRequirement::AlwaysAllow)
            .evaluate(&auto_call())
            .await,
        is_allow,
        PermissionBasis::fixed(PermissionPolicy::AlwaysAllow),
    );
    expect(
        "B2",
        fixed(ToolPermissionRequirement::AlwaysAsk)
            .evaluate(&auto_call())
            .await,
        is_ask,
        PermissionBasis::fixed(PermissionPolicy::AlwaysAsk),
    );

    let auto = |decision| {
        RuleBasedToolPermissionPolicy::new(PermissionRuleset::default())
            .with_mcp_toolsets(&[mcp_toolset(ToolPermissionRequirement::Auto)])
            .with_auto_evaluator(std::sync::Arc::new(FixedAuto(decision)))
    };
    expect(
        "B3",
        auto(AutoDecision::Allow).evaluate(&auto_call()).await,
        is_allow,
        PermissionBasis::auto(None),
    );
    expect(
        "B4",
        auto(AutoDecision::Deny).evaluate(&auto_call()).await,
        is_deny,
        PermissionBasis::auto(Some(PermissionReasonCode::HIGH_RISK)),
    );
    expect(
        "B5",
        auto(AutoDecision::Indeterminate)
            .evaluate(&auto_call())
            .await,
        is_ask,
        PermissionBasis::auto(Some(PermissionReasonCode::INDETERMINATE)),
    );

    let authored = RuleBasedToolPermissionPolicy::new(ruleset(
        ToolPermissionBehavior::Deny,
        vec![rule("Read", ToolPermissionBehavior::Allow)],
    ));
    let call = |tool_id: &str| ToolCall {
        tool_id: tool_id.into(),
        call_id: "c".into(),
        arguments: json!({}),
    };
    expect(
        "B6",
        authored.evaluate(&call("Read")).await,
        is_allow,
        PermissionBasis::fixed(PermissionPolicy::Rule),
    );
    expect(
        "B7",
        authored.evaluate(&call("Write")).await,
        is_deny,
        PermissionBasis::fixed(PermissionPolicy::Rule),
    );

    let mut disabled = mcp_toolset(ToolPermissionRequirement::AlwaysAllow);
    disabled.default.enabled = false;
    let disabled = RuleBasedToolPermissionPolicy::new(PermissionRuleset::default())
        .with_mcp_toolsets(&[disabled]);
    expect(
        "B8",
        disabled.evaluate(&auto_call()).await,
        is_deny,
        PermissionBasis::fixed(PermissionPolicy::Rule),
    );
}

#[test]
fn a_fixed_requirement_compiles_into_a_rule_that_still_names_it() {
    // The host expands a closed toolset's fixed requirements into rules. The
    // rule keeps naming its requirement so the audit can report
    // `always_allow`/`always_ask` rather than an anonymous rule; `auto` compiles
    // into no rule at all because a deferral is not a decision.
    //
    // | Rule | enabled | requirement | behavior | policy |
    // |---|---|---|---|---|
    // | C1 | T | always_allow | allow | always_allow |
    // | C2 | T | always_ask | ask | always_ask |
    // | C3 | T | auto | (no rule) | - |
    // | C4 | F | always_allow | deny | rule |
    use awaken_runtime_contract::agent_bindings::ToolExecutionPolicy;
    let pattern = || ToolCallPattern::parse("read").unwrap();
    let policy = |enabled, permission| ToolExecutionPolicy {
        enabled,
        permission,
    };
    let c1 = PermissionRule::from_fixed_policy(
        pattern(),
        policy(true, ToolPermissionRequirement::AlwaysAllow),
    )
    .unwrap();
    assert_eq!(
        (c1.behavior, c1.policy),
        (ToolPermissionBehavior::Allow, PermissionPolicy::AlwaysAllow),
        "C1"
    );
    let c2 = PermissionRule::from_fixed_policy(
        pattern(),
        policy(true, ToolPermissionRequirement::AlwaysAsk),
    )
    .unwrap();
    assert_eq!(
        (c2.behavior, c2.policy),
        (
            ToolPermissionBehavior::RequireConfirmation,
            PermissionPolicy::AlwaysAsk
        ),
        "C2"
    );
    assert!(
        PermissionRule::from_fixed_policy(pattern(), policy(true, ToolPermissionRequirement::Auto))
            .is_none(),
        "C3"
    );
    let c4 = PermissionRule::from_fixed_policy(
        pattern(),
        policy(false, ToolPermissionRequirement::AlwaysAllow),
    )
    .unwrap();
    assert_eq!(
        (c4.behavior, c4.policy),
        (ToolPermissionBehavior::Deny, PermissionPolicy::Rule),
        "C4"
    );
    assert_eq!(
        rule("Read", ToolPermissionBehavior::Allow).policy,
        PermissionPolicy::Rule,
        "an authored rule names `rule`"
    );
}
