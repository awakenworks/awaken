//! A declarative, Claude-Code-compatible permission policy.
//!
//! This extension implements the runtime's [`PermissionPolicy`] port with rules
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
use awaken_runtime_contract::permission::{
    PermissionContext, PermissionDecision, PermissionPolicy,
};
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
    Ask,
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

/// Where a rule came from in the settings hierarchy (Claude Code scopes). Carried
/// for audit/precedence metadata; the decision uses pattern specificity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionRuleScope {
    User,
    #[default]
    Project,
    Local,
    Session,
}

/// One rule: a pattern and the behavior it grants.
#[derive(Debug, Clone)]
pub struct PermissionRule {
    pub pattern: ToolCallPattern,
    pub behavior: ToolPermissionBehavior,
    pub scope: PermissionRuleScope,
}

impl PermissionRule {
    pub fn new(pattern: ToolCallPattern, behavior: ToolPermissionBehavior) -> Self {
        Self {
            pattern,
            behavior,
            scope: PermissionRuleScope::default(),
        }
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
            default_behavior: ToolPermissionBehavior::Ask,
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
            if rule.behavior == ToolPermissionBehavior::Deny {
                return ToolPermissionBehavior::Deny; // deny is absolute
            }
            if best.is_none_or(|(best_spec, _)| spec > best_spec) {
                best = Some((spec, rule.behavior));
            }
        }
        if let Some((_, behavior)) = best {
            return behavior;
        }

        match self.mode {
            Mode::Plan => ToolPermissionBehavior::Deny,
            _ => self.default_behavior,
        }
    }
}

/// The runtime [`PermissionPolicy`] backed by a [`PermissionRuleset`].
pub struct RulePermissionPolicy {
    ruleset: PermissionRuleset,
}

impl RulePermissionPolicy {
    pub fn new(ruleset: PermissionRuleset) -> Self {
        Self { ruleset }
    }
}

#[async_trait]
impl PermissionPolicy for RulePermissionPolicy {
    async fn decide(&self, ctx: &PermissionContext) -> PermissionDecision {
        match self.ruleset.decide(&ctx.tool_id, &ctx.arguments) {
            ToolPermissionBehavior::Allow => PermissionDecision::Allow,
            ToolPermissionBehavior::Deny => PermissionDecision::Deny {
                reason: format!("tool {} denied by policy", ctx.tool_id),
            },
            // The ask ticket is correlated to this call, so the operator's later
            // decision resumes exactly this invocation (ADR-0030 D2).
            ToolPermissionBehavior::Ask => PermissionDecision::Ask {
                ticket_id: format!("perm-{}", ctx.call_id),
            },
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
    ToolPermissionBehavior::Ask
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
                            "description": "A glob tool-call pattern, e.g. Bash(*rm*) or mcp__github__*."
                        },
                        "behavior": { "type": "string", "enum": ["allow", "ask", "deny"] }
                    },
                    "required": ["pattern", "behavior"]
                }
            }
        }
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
        assert_eq!(rs.default_behavior, ToolPermissionBehavior::Ask);
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
            default_behavior: ToolPermissionBehavior::Ask,
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
                rule("Bash", ToolPermissionBehavior::Ask),
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
            ToolPermissionBehavior::Ask
        );
    }

    #[tokio::test]
    async fn policy_maps_behavior_to_decision() {
        use awaken_runtime_contract::permission::PermissionContext;
        let policy = RulePermissionPolicy::new(PermissionRuleset {
            default_behavior: ToolPermissionBehavior::Ask,
            mode: Mode::Default,
            rules: vec![
                rule("Read", ToolPermissionBehavior::Allow),
                rule("Bash(*rm*)", ToolPermissionBehavior::Deny),
            ],
        });
        let ctx = |tool: &str, args| PermissionContext {
            tool_id: tool.to_string(),
            call_id: "c1".to_string(),
            arguments: args,
        };
        assert!(matches!(
            policy.decide(&ctx("Read", json!({}))).await,
            PermissionDecision::Allow
        ));
        assert!(matches!(
            policy.decide(&ctx("Bash", json!({"c": "x rm y"}))).await,
            PermissionDecision::Deny { .. }
        ));
        match policy.decide(&ctx("Other", json!({}))).await {
            PermissionDecision::Ask { ticket_id } => assert_eq!(ticket_id, "perm-c1"),
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
            default_behavior: ToolPermissionBehavior::Ask,
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
            default_behavior: ToolPermissionBehavior::Ask,
            mode: Mode::Default,
            rules: base,
        };
        assert_eq!(
            default.decide("Bash", &unmatched()),
            ToolPermissionBehavior::Ask
        );
    }
}
