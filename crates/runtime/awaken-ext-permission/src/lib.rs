//! A declarative, Claude-Code-compatible permission policy.
//!
//! This extension implements the runtime's [`ToolPermissionPolicy`] port with rules
//! that match a tool call by name and arguments, returning allow / ask / deny
//! (ADR-0030). The runtime owns the *axis* (the gate, the decision ticket, the
//! audit); this crate owns the concrete *policy*.
//!
//! Pattern syntax mirrors Claude Code tool specifiers:
//! ```text
//! Bash                         exact tool, any args
//! Bash(npm *)                  primary-arg glob
//! Edit(file_path ~ "src/**")   named-field glob
//! mcp__github__*               glob tool name
//! ```
//!
//! Precedence is **deny > (most-specific allow/ask) > mode default**: a deny rule
//! always wins; otherwise the most specific matching allow/ask rule decides; an
//! unmatched call falls to the mode's default. This matches Claude Code, where a
//! deny is absolute and a more specific allow overrides a broader ask.

use async_trait::async_trait;
use awaken_runtime_contract::permission::{ToolCall, ToolPermissionPolicy, ToolPermissionVerdict};
use awaken_tool_pattern::{
    ArgMatcher, MatchOp, MatchResult, Specificity, ToolMatcher, parse_pattern, pattern_matches,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A glob-only tool-call pattern for permission rules.
///
/// Wraps the shared [`awaken_tool_pattern`] engine but restricts it to the
/// glob/exact operators (`~`, `=`, `!~`, `!=`). This crate is deliberately
/// *glob-only*: it rejects the regex operators (`=~`, `!=~`) at parse time so a
/// `Deny` rule can never be silently reinterpreted into one that matches
/// nothing and fails open (see [`ToolCallPattern::parse`]).
#[derive(Debug, Clone)]
pub struct ToolCallPattern(awaken_tool_pattern::ToolCallPattern);

impl ToolCallPattern {
    /// Parse a Claude-Code-style specifier, rejecting regex operators.
    ///
    /// Glob tool names (`mcp__github__*`), primary globs (`Bash(npm *)`), and
    /// named-field glob/exact conditions (`Edit(file_path ~ "src/**")`) are
    /// accepted. A regex operator (`=~` / `!=~`) or a `/regex/` tool name is an
    /// error whose message names the unsupported operator.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let pattern = parse_pattern(spec).map_err(|err| err.to_string())?;
        ensure_glob_only(&pattern)?;
        Ok(Self(pattern))
    }

    /// Match a tool call, returning the pattern's specificity if it matches.
    pub fn matches(&self, tool_id: &str, args: &Value) -> Option<Specificity> {
        match pattern_matches(&self.0, tool_id, args) {
            MatchResult::Match { specificity } => Some(specificity),
            MatchResult::NoMatch => None,
        }
    }
}

/// Reject the regex operators the permission DSL does not support. The error
/// names the offending operator (`=~` / `!=~`) so a config author sees exactly
/// what to change.
fn ensure_glob_only(pattern: &awaken_tool_pattern::ToolCallPattern) -> Result<(), String> {
    fn reject_op(op: MatchOp) -> Result<(), String> {
        if matches!(op, MatchOp::Regex | MatchOp::NotRegex) {
            return Err(format!(
                "regex operator '{op}' is not supported in permission patterns; use a glob with '~'"
            ));
        }
        Ok(())
    }
    if matches!(pattern.tool, ToolMatcher::Regex(_)) {
        return Err("regex tool names ('/.../') are not supported in permission patterns".into());
    }
    match &pattern.args {
        ArgMatcher::Any => {}
        ArgMatcher::Primary { op, .. } => reject_op(*op)?,
        ArgMatcher::Fields(conditions) => {
            for cond in conditions {
                reject_op(cond.op)?;
            }
        }
    }
    Ok(())
}

/// What a matched rule does — the Claude Code behaviors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolPermissionBehavior {
    Allow,
    #[serde(rename = "ask")]
    RequireConfirmation,
    Deny,
}

/// The Claude Code permission mode, shaping the default for unmatched calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    /// Use the rules; an unmatched call falls to `default_behavior`.
    #[default]
    Default,
    /// Like `Default`. Selective auto-accept of *edit* tools needs a tool
    /// side-effect class (a follow-on); without it this behaves as `Default`.
    AcceptEdits,
    /// Read-only planning: an unmatched call is denied, so nothing with a side
    /// effect runs unless an explicit allow rule matches.
    Plan,
    /// Bypass: every call is allowed (the gate is effectively off).
    BypassPermissions,
}

impl Mode {
    /// Resolve an unmatched call without consulting any ambient default.
    /// Planning fails closed, bypass is the one explicit allow-all posture,
    /// and the ordinary modes preserve the authored default exactly.
    #[must_use]
    const fn unmatched_behavior(
        self,
        authored_default: ToolPermissionBehavior,
    ) -> ToolPermissionBehavior {
        match self {
            Self::Plan => ToolPermissionBehavior::Deny,
            Self::BypassPermissions => ToolPermissionBehavior::Allow,
            Self::Default | Self::AcceptEdits => authored_default,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchedRuleSelection {
    Deny,
    Continue(Option<(Specificity, ToolPermissionBehavior)>),
}

/// Fold one matching rule into the current non-deny winner. A deny is an
/// absorbing result; otherwise only a strictly more-specific candidate may
/// replace the winner, preserving deterministic first-wins behavior on ties.
fn select_matched_rule(
    best: Option<(Specificity, ToolPermissionBehavior)>,
    specificity: Specificity,
    behavior: ToolPermissionBehavior,
) -> MatchedRuleSelection {
    if behavior == ToolPermissionBehavior::Deny {
        return MatchedRuleSelection::Deny;
    }
    if best.is_none_or(|(best_specificity, _)| specificity > best_specificity) {
        MatchedRuleSelection::Continue(Some((specificity, behavior)))
    } else {
        MatchedRuleSelection::Continue(best)
    }
}

/// One rule: a pattern and the behavior it grants.
#[derive(Debug, Clone)]
pub struct PermissionRule {
    pub pattern: ToolCallPattern,
    pub behavior: ToolPermissionBehavior,
}

impl PermissionRule {
    pub fn new(pattern: ToolCallPattern, behavior: ToolPermissionBehavior) -> Self {
        Self { pattern, behavior }
    }
}

/// A set of rules plus the mode and default for unmatched calls.
#[derive(Debug, Clone)]
pub struct PermissionRuleset {
    pub default_behavior: ToolPermissionBehavior,
    pub mode: Mode,
    pub rules: Vec<PermissionRule>,
}

impl Default for PermissionRuleset {
    fn default() -> Self {
        Self {
            default_behavior: ToolPermissionBehavior::RequireConfirmation,
            mode: Mode::Default,
            rules: Vec::new(),
        }
    }
}

impl PermissionRuleset {
    /// Decide one tool call. Deny is absolute; otherwise the most specific
    /// matching allow/ask wins; an unmatched call falls to the mode default.
    pub fn decide(&self, tool_id: &str, args: &Value) -> ToolPermissionBehavior {
        if self.mode == Mode::BypassPermissions {
            return ToolPermissionBehavior::Allow;
        }

        let mut best: Option<(Specificity, ToolPermissionBehavior)> = None;
        for rule in &self.rules {
            let Some(spec) = rule.pattern.matches(tool_id, args) else {
                continue;
            };
            match select_matched_rule(best, spec, rule.behavior) {
                MatchedRuleSelection::Deny => return ToolPermissionBehavior::Deny,
                MatchedRuleSelection::Continue(selected) => best = selected,
            }
        }
        if let Some((_, behavior)) = best {
            return behavior;
        }

        self.mode.unmatched_behavior(self.default_behavior)
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::{
        MatchedRuleSelection, Mode, Specificity, ToolPermissionBehavior, select_matched_rule,
    };

    fn arbitrary_behavior() -> ToolPermissionBehavior {
        match kani::any::<u8>() % 3 {
            0 => ToolPermissionBehavior::Allow,
            1 => ToolPermissionBehavior::RequireConfirmation,
            _ => ToolPermissionBehavior::Deny,
        }
    }

    fn arbitrary_specificity() -> Specificity {
        Specificity {
            tool_kind: kani::any(),
            has_args: kani::any(),
            field_count: kani::any(),
            field_precision: kani::any(),
        }
    }

    #[kani::proof]
    fn unmatched_permission_mode_is_exact_and_plan_fails_closed() {
        let mode = match kani::any::<u8>() % 4 {
            0 => Mode::Default,
            1 => Mode::AcceptEdits,
            2 => Mode::Plan,
            _ => Mode::BypassPermissions,
        };
        let authored = arbitrary_behavior();
        let selected = mode.unmatched_behavior(authored);
        let expected = match mode {
            Mode::Plan => ToolPermissionBehavior::Deny,
            Mode::BypassPermissions => ToolPermissionBehavior::Allow,
            Mode::Default | Mode::AcceptEdits => authored,
        };
        assert_eq!(selected, expected);
        if mode == Mode::Plan {
            assert_eq!(selected, ToolPermissionBehavior::Deny);
        }
    }

    #[kani::proof]
    fn matched_deny_is_absolute_and_only_stricter_non_deny_replaces_authority() {
        let existing = if kani::any() {
            Some((arbitrary_specificity(), arbitrary_behavior()))
        } else {
            None
        };
        let candidate_specificity = arbitrary_specificity();
        let candidate_behavior = arbitrary_behavior();
        let selected = select_matched_rule(existing, candidate_specificity, candidate_behavior);

        if candidate_behavior == ToolPermissionBehavior::Deny {
            assert_eq!(selected, MatchedRuleSelection::Deny);
            return;
        }
        let expected =
            if existing.is_none_or(|(specificity, _)| candidate_specificity > specificity) {
                Some((candidate_specificity, candidate_behavior))
            } else {
                existing
            };
        assert_eq!(selected, MatchedRuleSelection::Continue(expected));
    }
}

/// The runtime [`ToolPermissionPolicy`] backed by a [`PermissionRuleset`].
pub struct RuleBasedToolPermissionPolicy {
    ruleset: PermissionRuleset,
}

impl RuleBasedToolPermissionPolicy {
    pub fn new(ruleset: PermissionRuleset) -> Self {
        Self { ruleset }
    }
}

#[async_trait]
impl ToolPermissionPolicy for RuleBasedToolPermissionPolicy {
    async fn evaluate(&self, call: &ToolCall) -> ToolPermissionVerdict {
        match self.ruleset.decide(&call.tool_id, &call.arguments) {
            ToolPermissionBehavior::Allow => ToolPermissionVerdict::Allow,
            ToolPermissionBehavior::Deny => ToolPermissionVerdict::Deny {
                reason: format!("tool {} denied by policy", call.tool_id),
            },
            // The ask ticket is correlated to this call, so the operator's later
            // decision resumes exactly this invocation (ADR-0030 D2).
            ToolPermissionBehavior::RequireConfirmation => {
                ToolPermissionVerdict::RequireConfirmation {
                    correlation_id: format!("perm-{}", call.call_id),
                }
            }
        }
    }
}

/// The JSON wire shape a config author writes for an agent's `permission`
/// section: a default behavior plus an ordered rule list of `{ pattern, behavior }`.
/// Deliberately flat and stringly-patterned so a console form maps to it 1:1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesConfig {
    /// Behavior for a call no rule matches (defaults to `ask`).
    #[serde(default = "default_ask")]
    pub default_behavior: ToolPermissionBehavior,
    /// The Claude Code permission mode (defaults to `default`).
    #[serde(default)]
    pub mode: Mode,
    /// Ordered rules; `deny` is absolute, otherwise the most specific match wins.
    #[serde(default)]
    pub rules: Vec<RuleConfig>,
}

/// One authored rule: a glob pattern string and the behavior it grants.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleConfig {
    pub pattern: String,
    pub behavior: ToolPermissionBehavior,
}

fn default_ask() -> ToolPermissionBehavior {
    ToolPermissionBehavior::RequireConfirmation
}

/// Parse an agent's `permission` config section into a [`PermissionRuleset`].
///
/// Every pattern is validated through [`ToolCallPattern::parse`], so a fail-open
/// regex operator is rejected here rather than silently accepted. Returns an error
/// (whose message names the offending pattern/field) so a malformed policy is
/// surfaced, never silently reinterpreted.
pub fn parse_ruleset(value: &Value) -> Result<PermissionRuleset, String> {
    let config: RulesConfig = serde_json::from_value(value.clone())
        .map_err(|e| format!("invalid permission config: {e}"))?;
    let mut rules = Vec::with_capacity(config.rules.len());
    for r in config.rules {
        let pattern = ToolCallPattern::parse(&r.pattern)
            .map_err(|e| format!("rule pattern '{}': {e}", r.pattern))?;
        rules.push(PermissionRule::new(pattern, r.behavior));
    }
    Ok(PermissionRuleset {
        default_behavior: config.default_behavior,
        mode: config.mode,
        rules,
    })
}

/// The JSON Schema for the `permission` config section, advertised on
/// `/v1/capabilities` so the console can discover and author the policy. Hand-authored
/// (not schemars) to keep this crate dependency-light; it mirrors [`RulesConfig`].
pub fn permission_config_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "title": "Permission policy",
        "description": PERMISSION_AUTHORING_GUIDE,
        "examples": [PERMISSION_EXAMPLE_DENY_DESTRUCTIVE()],
        "properties": {
            "default_behavior": {
                "type": "string",
                "enum": ["allow", "ask", "deny"],
                "default": "ask",
                "description": "Behavior for a tool call no rule matches."
            },
            "mode": {
                "type": "string",
                "enum": ["default", "acceptEdits", "plan", "bypassPermissions"],
                "default": "default",
                "description": "Permission mode; 'plan' denies unmatched side-effecting calls."
            },
            "rules": {
                "type": "array",
                "description": "Ordered rules; deny is absolute, else the most specific match wins.",
                "items": {
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "A tool-call pattern: `bash` matches that tool, \
    `bash(command ~ \"*rm *\")` also matches on an argument, `mcp__github__*` globs a name. \
    Tool ids are CASE-SENSITIVE and lowercase (`bash`/`read`/`write`, NOT `Bash`)."
                        },
                        "behavior": { "type": "string", "enum": ["allow", "ask", "deny"] }
                    },
                    "required": ["pattern", "behavior"]
                }
            }
        }
    })
}

/// The rule-pattern DSL and precedence rules the raw schema can't express (patterns
/// live inside strings), so an author — human form or LLM — otherwise guesses them.
/// Same tool-pattern grammar as the state machine's `on`; same case-sensitivity trap.
const PERMISSION_AUTHORING_GUIDE: &str = "\
An ordered permission policy over tool calls. Authoring rules:\n\
- `rules[].pattern` is a TOOL PATTERN: `<tool_id>` matches that tool, \
`<tool_id>(<arg> ~ \"<glob>\")` also matches on an argument, and `mcp__<server>__*` globs \
a name. The `<tool_id>` and `<arg>` are the EXACT ids the tools use — they are \
case-sensitive: the built-in tools are `bash` (arg `command`), `read`/`write` (arg \
`path`), NOT `Bash`/`Read`/`file_path`. A pattern with the wrong case parses but never \
matches, silently disabling the rule.\n\
- `behavior` is `allow` | `ask` | `deny`. `deny` is ABSOLUTE (any matching deny wins); \
otherwise the MOST SPECIFIC matching rule wins, and `default_behavior` applies if none match.\n\
- `mode`: `default` honors the rules; `plan` denies unmatched side-effecting calls; \
`bypassPermissions` allows everything (use only to let another gate be authoritative).";

/// A canonical policy: allow reads, ask by default, hard-deny destructive shell calls.
#[allow(non_snake_case)]
fn PERMISSION_EXAMPLE_DENY_DESTRUCTIVE() -> Value {
    serde_json::json!({
        "default_behavior": "ask",
        "mode": "default",
        "rules": [
            { "pattern": "read", "behavior": "allow" },
            { "pattern": "bash(command ~ \"*rm -rf*\")", "behavior": "deny" }
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(spec: &str, behavior: ToolPermissionBehavior) -> PermissionRule {
        PermissionRule::new(ToolCallPattern::parse(spec).unwrap(), behavior)
    }

    #[test]
    fn parse_ruleset_builds_rules_and_default() {
        let rs = parse_ruleset(&json!({
            "default_behavior": "deny",
            "rules": [
                { "pattern": "read", "behavior": "allow" },
                { "pattern": "Bash(*rm*)", "behavior": "deny" }
            ]
        }))
        .unwrap();
        assert_eq!(rs.default_behavior, ToolPermissionBehavior::Deny);
        assert_eq!(rs.rules.len(), 2);
        // The parsed ruleset enforces as authored.
        assert_eq!(rs.decide("read", &json!({})), ToolPermissionBehavior::Allow);
        assert_eq!(
            rs.decide("Bash", &json!({ "command": "rm -rf" })),
            ToolPermissionBehavior::Deny
        );
        assert_eq!(rs.decide("write", &json!({})), ToolPermissionBehavior::Deny); // unmatched → default
    }

    #[test]
    fn parse_ruleset_defaults_to_ask_when_unspecified() {
        let rs = parse_ruleset(&json!({})).unwrap();
        assert_eq!(
            rs.default_behavior,
            ToolPermissionBehavior::RequireConfirmation
        );
        assert!(rs.rules.is_empty());
    }

    #[test]
    fn parse_ruleset_rejects_fail_open_regex_pattern() {
        // A regex operator would be a fail-open footgun; parse must reject it, naming the pattern.
        let err = parse_ruleset(&json!({
            "rules": [ { "pattern": "Bash(command =~ \"rm\")", "behavior": "deny" } ]
        }))
        .unwrap_err();
        assert!(
            err.contains("Bash(command"),
            "error names the offending pattern: {err}"
        );
    }

    #[test]
    fn name_and_primary_glob_match() {
        let p = ToolCallPattern::parse("Bash(npm *)").unwrap();
        assert!(
            p.matches("Bash", &json!({"command": "npm install"}))
                .is_some()
        );
        assert!(p.matches("Bash", &json!({"command": "rm -rf"})).is_none());
        assert!(p.matches("Read", &json!({"command": "npm x"})).is_none());
    }

    #[test]
    fn named_field_and_glob_tool_match() {
        let edit = ToolCallPattern::parse("Edit(file_path ~ \"src/**\")").unwrap();
        assert!(
            edit.matches("Edit", &json!({"file_path": "src/a/b.rs"}))
                .is_some()
        );
        assert!(
            edit.matches("Edit", &json!({"file_path": "docs/x"}))
                .is_none()
        );

        let mcp = ToolCallPattern::parse("mcp__github__*").unwrap();
        assert!(mcp.matches("mcp__github__issues", &json!({})).is_some());
        assert!(mcp.matches("mcp__gitlab__x", &json!({})).is_none());
    }

    #[test]
    fn deny_is_absolute_over_allow() {
        let set = PermissionRuleset {
            default_behavior: ToolPermissionBehavior::RequireConfirmation,
            mode: Mode::Default,
            rules: vec![
                rule("Bash", ToolPermissionBehavior::Allow),
                rule("Bash(*rm*)", ToolPermissionBehavior::Deny),
            ],
        };
        assert_eq!(
            set.decide("Bash", &json!({"c": "ls"})),
            ToolPermissionBehavior::Allow
        );
        assert_eq!(
            set.decide("Bash", &json!({"c": "x rm y"})),
            ToolPermissionBehavior::Deny,
            "a deny match overrides a broad allow"
        );
    }

    #[test]
    fn more_specific_allow_overrides_broad_ask() {
        let set = PermissionRuleset {
            default_behavior: ToolPermissionBehavior::Deny,
            mode: Mode::Default,
            rules: vec![
                rule("Bash", ToolPermissionBehavior::RequireConfirmation),
                rule("Bash(npm test*)", ToolPermissionBehavior::Allow),
            ],
        };
        assert_eq!(
            set.decide("Bash", &json!({"command": "npm test"})),
            ToolPermissionBehavior::Allow,
            "the specific allow beats the broad ask"
        );
        assert_eq!(
            set.decide("Bash", &json!({"command": "ls"})),
            ToolPermissionBehavior::RequireConfirmation
        );
    }

    #[tokio::test]
    async fn policy_maps_behavior_to_decision() {
        use awaken_runtime_contract::permission::ToolCall;
        let policy = RuleBasedToolPermissionPolicy::new(PermissionRuleset {
            default_behavior: ToolPermissionBehavior::RequireConfirmation,
            mode: Mode::Default,
            rules: vec![
                rule("Read", ToolPermissionBehavior::Allow),
                rule("Bash(*rm*)", ToolPermissionBehavior::Deny),
            ],
        });
        let ctx = |tool: &str, args| ToolCall {
            tool_id: tool.to_string(),
            call_id: "c1".to_string(),
            arguments: args,
        };
        assert!(matches!(
            policy.evaluate(&ctx("Read", json!({}))).await,
            ToolPermissionVerdict::Allow
        ));
        assert!(matches!(
            policy.evaluate(&ctx("Bash", json!({"c": "x rm y"}))).await,
            ToolPermissionVerdict::Deny { .. }
        ));
        match policy.evaluate(&ctx("Other", json!({}))).await {
            ToolPermissionVerdict::RequireConfirmation { correlation_id } => {
                assert_eq!(correlation_id, "perm-c1")
            }
            other => panic!("expected ask, got {other:?}"),
        }
    }

    #[test]
    fn modes_shape_the_default() {
        let base = vec![rule("Read", ToolPermissionBehavior::Allow)];
        let unmatched = || json!({});

        let bypass = PermissionRuleset {
            default_behavior: ToolPermissionBehavior::Deny,
            mode: Mode::BypassPermissions,
            rules: base.clone(),
        };
        assert_eq!(
            bypass.decide("Bash", &unmatched()),
            ToolPermissionBehavior::Allow
        );

        let plan = PermissionRuleset {
            default_behavior: ToolPermissionBehavior::RequireConfirmation,
            mode: Mode::Plan,
            rules: base.clone(),
        };
        assert_eq!(
            plan.decide("Bash", &unmatched()),
            ToolPermissionBehavior::Deny
        );
        assert_eq!(
            plan.decide("Read", &unmatched()),
            ToolPermissionBehavior::Allow
        );

        let default = PermissionRuleset {
            default_behavior: ToolPermissionBehavior::RequireConfirmation,
            mode: Mode::Default,
            rules: base,
        };
        assert_eq!(
            default.decide("Bash", &unmatched()),
            ToolPermissionBehavior::RequireConfirmation
        );
    }

    #[test]
    fn config_schema_carries_authoring_guidance() {
        let schema = permission_config_schema();
        // The DSL grammar + case-sensitivity trap must ride on the schema so the assistant
        // (and the console form) that author from it don't guess.
        let desc = schema["description"].as_str().unwrap();
        assert!(desc.contains("case-sensitive"), "guide names the case trap");
        assert!(schema["examples"].is_array());
    }

    #[test]
    fn shipped_example_actually_enforces_against_the_real_bash_tool() {
        // The example's deny rule must match the REAL tool id `bash` (lowercase). This
        // guards the exact bug we're fixing: a `Bash(...)` example parses but never matches
        // the runtime tool, silently disabling the rule.
        let rs = parse_ruleset(&PERMISSION_EXAMPLE_DENY_DESTRUCTIVE()).unwrap();
        assert_eq!(
            rs.decide("bash", &json!({ "command": "sudo rm -rf /" })),
            ToolPermissionBehavior::Deny,
            "the example denies a destructive lowercase `bash` call"
        );
        assert_eq!(
            rs.decide("read", &json!({ "path": "/etc/hosts" })),
            ToolPermissionBehavior::Allow
        );
        assert_eq!(
            rs.decide("bash", &json!({ "command": "ls" })),
            ToolPermissionBehavior::RequireConfirmation,
            "a benign bash call falls through to the ask default"
        );
    }
}
