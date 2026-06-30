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
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

/// How precisely a pattern matched — higher wins when resolving allow vs ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Specificity(u32);

/// Matches a tool id: exact, or a glob (`mcp__github__*`).
#[derive(Debug, Clone)]
enum ToolMatcher {
    Exact(String),
    Glob(glob::Pattern),
}

impl ToolMatcher {
    fn matches(&self, tool_id: &str) -> bool {
        match self {
            ToolMatcher::Exact(id) => id == tool_id,
            ToolMatcher::Glob(p) => p.matches(tool_id),
        }
    }
    fn specificity(&self) -> u32 {
        match self {
            ToolMatcher::Exact(_) => 100,
            ToolMatcher::Glob(_) => 10,
        }
    }
}

/// Matches the call's arguments.
#[derive(Debug, Clone)]
enum ArgMatcher {
    /// A named string field matches a glob (`Edit(file_path ~ "src/**")`).
    Field { field: String, glob: glob::Pattern },
    /// Any top-level string value matches a glob (`Bash(npm *)`).
    Primary { glob: glob::Pattern },
}

impl ArgMatcher {
    fn matches(&self, args: &Value) -> bool {
        match self {
            ArgMatcher::Field { field, glob } => args
                .get(field)
                .and_then(Value::as_str)
                .is_some_and(|v| glob.matches(v)),
            ArgMatcher::Primary { glob } => match args {
                Value::String(s) => glob.matches(s),
                Value::Object(map) => map
                    .values()
                    .filter_map(Value::as_str)
                    .any(|v| glob.matches(v)),
                _ => false,
            },
        }
    }
    fn specificity(&self) -> u32 {
        match self {
            ArgMatcher::Field { .. } => 50,
            ArgMatcher::Primary { .. } => 30,
        }
    }
}

/// A tool-call pattern: a tool matcher and an optional argument matcher.
#[derive(Debug, Clone)]
pub struct ToolCallPattern {
    tool: ToolMatcher,
    arg: Option<ArgMatcher>,
}

impl ToolCallPattern {
    /// Parse a Claude-Code-style specifier (see the crate docs). Glob syntax is
    /// `*`/`**`/`?`/`[..]`; an invalid glob is an error.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        let (tool_spec, arg_spec) = match spec.split_once('(') {
            Some((tool, rest)) => {
                let inner = rest
                    .strip_suffix(')')
                    .ok_or_else(|| format!("unterminated '(' in pattern {spec:?}"))?;
                (tool.trim(), Some(inner.trim()))
            }
            None => (spec, None),
        };

        let tool = if tool_spec.contains(['*', '?', '[']) {
            ToolMatcher::Glob(glob_of(tool_spec)?)
        } else {
            ToolMatcher::Exact(tool_spec.to_string())
        };

        let arg = match arg_spec {
            None | Some("") => None,
            Some(inner) => Some(parse_arg(inner)?),
        };
        Ok(Self { tool, arg })
    }

    /// Match a tool call, returning the pattern's specificity if it matches.
    pub fn matches(&self, tool_id: &str, args: &Value) -> Option<Specificity> {
        if !self.tool.matches(tool_id) {
            return None;
        }
        if let Some(arg) = &self.arg
            && !arg.matches(args)
        {
            return None;
        }
        let score = self.tool.specificity() + self.arg.as_ref().map_or(0, ArgMatcher::specificity);
        Some(Specificity(score))
    }
}

fn glob_of(pattern: &str) -> Result<glob::Pattern, String> {
    glob::Pattern::new(pattern).map_err(|err| format!("bad glob {pattern:?}: {err}"))
}

fn parse_arg(inner: &str) -> Result<ArgMatcher, String> {
    // `field ~ "glob"` is a named-field glob; anything else is a primary glob.
    if let Some((field, rest)) = inner.split_once('~') {
        let field = field.trim().to_string();
        let glob = unquote(rest.trim());
        Ok(ArgMatcher::Field {
            field,
            glob: glob_of(glob)?,
        })
    } else {
        Ok(ArgMatcher::Primary {
            glob: glob_of(unquote(inner))?,
        })
    }
}

fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(spec: &str, behavior: ToolPermissionBehavior) -> PermissionRule {
        PermissionRule::new(ToolCallPattern::parse(spec).unwrap(), behavior)
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
                rule("Bash(* rm *)", ToolPermissionBehavior::Deny),
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
                rule("Bash(* rm *)", ToolPermissionBehavior::Deny),
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
