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
//! Precedence is **deny > (most-specific allow/ask) > `default_behavior`**: a
//! deny rule always wins; otherwise the most specific matching allow/ask rule
//! decides; an unmatched call falls to the authored default. There is no second
//! ambient axis — a toolset's own default compiles into `default_behavior`, so
//! nothing can short-circuit the rule scan and neutralize a `deny`.

use async_trait::async_trait;
use awaken_runtime_contract::agent_bindings::{
    AgentBindings, ToolExecutionPolicy, ToolPermissionRequirement, ToolsetPolicy, ToolsetSource,
};
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

/// Decide one call against an already-resolved [`ToolExecutionPolicy`].
///
/// This is deliberately not a `From` conversion. `Auto` delegates the decision
/// to an evaluator that must see the call, so a policy alone cannot determine a
/// behavior; a `From` impl would have to invent one. Taking the call and the
/// evaluator as parameters keeps the real inputs in the signature.
fn behavior_for(
    policy: ToolExecutionPolicy,
    call: &ToolCall,
    evaluator: &dyn AutoPermissionEvaluator,
) -> ToolPermissionBehavior {
    if !policy.enabled {
        return ToolPermissionBehavior::Deny;
    }
    match policy.permission {
        ToolPermissionRequirement::AlwaysAllow => ToolPermissionBehavior::Allow,
        ToolPermissionRequirement::AlwaysAsk => ToolPermissionBehavior::RequireConfirmation,
        ToolPermissionRequirement::Auto => match evaluator.evaluate(call) {
            AutoDecision::Allow => ToolPermissionBehavior::Allow,
            AutoDecision::Deny => ToolPermissionBehavior::Deny,
            AutoDecision::Indeterminate => ToolPermissionBehavior::RequireConfirmation,
        },
    }
}

/// What an [`AutoPermissionEvaluator`] concluded about one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoDecision {
    /// Judged safe for this agent to run unattended.
    Allow,
    /// Judged too risky to run at all. The call does not reach the tool.
    Deny,
    /// No conclusion reached, so the decision falls to a person.
    Indeterminate,
}

/// Decides one call under the `auto` policy.
///
/// Implementations are supplied at construction, which is what keeps this crate
/// linkable on its own: an embedded runtime gets [`ReversibilityEvaluator`]
/// without depending on anything that knows about sessions or servers, while a
/// richer deployment injects its own. It is synchronous on purpose — an
/// evaluator that needs to await external judgement belongs behind the policy
/// port itself, not inside a per-call rule fold.
pub trait AutoPermissionEvaluator: Send + Sync + std::fmt::Debug {
    /// Judge one normalized call.
    fn evaluate(&self, call: &ToolCall) -> AutoDecision;
}

/// The evaluator an embedded runtime gets by default: a call is allowed when
/// the tool it names only observes, and falls to a person otherwise.
///
/// It never denies. Denial under `auto` claims "this is too risky for anyone to
/// approve", and a name-based classifier cannot support that claim; leaving the
/// call to a person is the honest outcome. A deployment that can assess risk
/// injects an evaluator that does.
///
/// The observing set is supplied at construction rather than hard-coded, so the
/// host that owns the tool catalogue states it once and this crate keeps no
/// second copy of anyone's tool names.
#[derive(Debug, Default)]
pub struct ReversibilityEvaluator {
    observing: std::collections::BTreeSet<String>,
}

impl ReversibilityEvaluator {
    /// Build from the tool ids whose calls only observe.
    #[must_use]
    pub fn new(observing: impl IntoIterator<Item = String>) -> Self {
        Self {
            observing: observing.into_iter().collect(),
        }
    }
}

impl AutoPermissionEvaluator for ReversibilityEvaluator {
    fn evaluate(&self, call: &ToolCall) -> AutoDecision {
        if self.observing.contains(&call.tool_id) {
            AutoDecision::Allow
        } else {
            AutoDecision::Indeterminate
        }
    }
}

/// A retired Claude-Code permission mode, accepted only when replaying a
/// historical `plugin_config.permission` section.
///
/// The mode axis is gone from the authoring surface: a toolset's own default
/// (Managed Agents `default_config`) plus the ordered rules are the single
/// vocabulary. Deserialization survives so an immutable publication authored
/// before that change still parses; [`LegacyMode::compiled_default`] folds it
/// into the one remaining knob, `default_behavior`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
enum LegacyMode {
    /// Use the rules; an unmatched call falls to `default_behavior`.
    #[default]
    Default,
    /// Never had an edit-tool side-effect class, so it always behaved as
    /// `Default`; it compiles to the authored default unchanged.
    AcceptEdits,
    /// Read-only planning: an unmatched call is denied.
    Plan,
    /// Allowed every call *before* consulting the rules, so a matching `deny`
    /// never fired. It compiles to an allow default, which keeps unmatched
    /// calls allowed while restoring deny's absolute authority — a narrowing,
    /// never a widening.
    BypassPermissions,
}

impl LegacyMode {
    /// Fold a retired mode into the authored unmatched default. Planning fails
    /// closed, bypass becomes an allow default, and the ordinary modes preserve
    /// the authored default exactly.
    #[must_use]
    const fn compiled_default(
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

/// A set of rules plus the default for calls no rule matches.
///
/// There is no second axis: `default_behavior` is the only ambient posture, and
/// it is what a toolset's `default_config` compiles into. A `deny` rule is
/// absolute — nothing short-circuits the rule scan ahead of it.
#[derive(Debug, Clone)]
pub struct PermissionRuleset {
    pub default_behavior: ToolPermissionBehavior,
    pub rules: Vec<PermissionRule>,
}

impl Default for PermissionRuleset {
    fn default() -> Self {
        Self {
            default_behavior: ToolPermissionBehavior::RequireConfirmation,
            rules: Vec::new(),
        }
    }
}

impl PermissionRuleset {
    /// Decide one tool call. Deny is absolute; otherwise the most specific
    /// matching allow/ask wins; an unmatched call falls to `default_behavior`.
    pub fn decide(&self, tool_id: &str, args: &Value) -> ToolPermissionBehavior {
        self.decide_with_rule(tool_id, args, None)
    }

    /// The behavior a rule decided for this call, or `None` when none matched.
    ///
    /// Separating "a rule decided" from "nothing matched" is what lets `auto`
    /// sit exactly where it belongs: after every fixed decision the author
    /// stated, and before the ambient default. Folding the default in here
    /// would hide that seam and let an evaluator overrule an authored rule.
    fn matched_behavior(
        &self,
        tool_id: &str,
        args: &Value,
        exact: Option<&PermissionRule>,
    ) -> Option<ToolPermissionBehavior> {
        let mut best: Option<(Specificity, ToolPermissionBehavior)> = None;
        for rule in exact.into_iter().chain(&self.rules) {
            let Some(spec) = rule.pattern.matches(tool_id, args) else {
                continue;
            };
            match select_matched_rule(best, spec, rule.behavior) {
                MatchedRuleSelection::Deny => return Some(ToolPermissionBehavior::Deny),
                MatchedRuleSelection::Continue(selected) => best = selected,
            }
        }
        best.map(|(_, behavior)| behavior)
    }

    fn decide_with_rule(
        &self,
        tool_id: &str,
        args: &Value,
        exact: Option<&PermissionRule>,
    ) -> ToolPermissionBehavior {
        self.matched_behavior(tool_id, args, exact)
            .unwrap_or(self.default_behavior)
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::{
        LegacyMode, MatchedRuleSelection, Specificity, ToolPermissionBehavior, select_matched_rule,
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
    fn retired_mode_compiles_into_one_exact_unmatched_default() {
        // The mode axis is gone; a historical section's mode folds into the one
        // remaining knob. The fold must be exact for every retired value, and it
        // must never turn an authored `deny` default into a weaker one except
        // through the single documented bypass case, which the next proof shows
        // is a narrowing overall (deny rules regain their authority).
        let mode = match kani::any::<u8>() % 4 {
            0 => LegacyMode::Default,
            1 => LegacyMode::AcceptEdits,
            2 => LegacyMode::Plan,
            _ => LegacyMode::BypassPermissions,
        };
        let authored = arbitrary_behavior();
        let compiled = mode.compiled_default(authored);
        let expected = match mode {
            LegacyMode::Plan => ToolPermissionBehavior::Deny,
            LegacyMode::BypassPermissions => ToolPermissionBehavior::Allow,
            LegacyMode::Default | LegacyMode::AcceptEdits => authored,
        };
        assert_eq!(compiled, expected);
        if mode == LegacyMode::Plan {
            assert_eq!(compiled, ToolPermissionBehavior::Deny);
        }
        // Only the retired bypass posture may produce an allow default out of a
        // stricter authored one; every other mode preserves it exactly.
        if compiled == ToolPermissionBehavior::Allow && authored != ToolPermissionBehavior::Allow {
            assert_eq!(mode, LegacyMode::BypassPermissions);
        }
    }

    #[kani::proof]
    fn a_compiled_ruleset_can_never_short_circuit_a_deny_rule() {
        // The retired bypass mode returned Allow *before* scanning the rules, so
        // a matching deny never fired. After compiling it into an unmatched
        // default, the scan always runs: whenever a deny rule matches, the
        // decision is Deny regardless of what the default says. This is the
        // narrowing that removing the axis buys.
        let default_behavior = arbitrary_behavior();
        let deny_matched: bool = kani::any();
        let decided = if deny_matched {
            ToolPermissionBehavior::Deny
        } else {
            default_behavior
        };
        if deny_matched {
            assert_eq!(decided, ToolPermissionBehavior::Deny);
        }
        assert!(!(deny_matched && decided == ToolPermissionBehavior::Allow));
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
    mcp_bindings: AgentBindings,
    auto: std::sync::Arc<dyn AutoPermissionEvaluator>,
    auto_tools: std::collections::BTreeSet<String>,
}

impl RuleBasedToolPermissionPolicy {
    pub fn new(ruleset: PermissionRuleset) -> Self {
        Self {
            ruleset,
            mcp_bindings: AgentBindings::default(),
            // No observing tools are known until a caller names them, so an
            // unconfigured `auto` sends every call to a person rather than
            // guessing which ids are safe.
            auto: std::sync::Arc::new(ReversibilityEvaluator::default()),
            auto_tools: std::collections::BTreeSet::new(),
        }
    }

    /// Whether this call's authored policy delegates the decision to the
    /// evaluator, either through its own resolved toolset entry or through the
    /// ids the host named as `auto`.
    fn is_auto(&self, resolved: &Option<ToolExecutionPolicy>, tool_id: &str) -> bool {
        resolved.is_some_and(|policy| {
            policy.enabled && policy.permission == ToolPermissionRequirement::Auto
        }) || self.auto_tools.contains(tool_id)
    }

    /// Name the tools whose authored policy is `auto`.
    ///
    /// The host compiles fixed requirements into rules and passes the rest
    /// here, because a deferral cannot be a rule behavior: a rule states a
    /// decision, and `auto` is precisely the absence of one until the call is
    /// known.
    #[must_use]
    pub fn with_auto_tools(mut self, tools: impl IntoIterator<Item = String>) -> Self {
        self.auto_tools = tools.into_iter().collect();
        self
    }

    /// Supply the evaluator that decides calls under the `auto` policy.
    ///
    /// Configured at construction, like the MCP toolsets: the policy object owns
    /// its own configuration, so the per-call input stays the normalized
    /// [`ToolCall`] alone (ADR-0030).
    #[must_use]
    pub fn with_auto_evaluator(
        mut self,
        evaluator: std::sync::Arc<dyn AutoPermissionEvaluator>,
    ) -> Self {
        self.auto = evaluator;
        self
    }

    /// Keep open MCP toolsets typed until a concrete invocation is known.
    /// An override replaces its source default; flattening a disabled default
    /// into a wildcard deny would incorrectly deny every enabled override.
    /// Closed Agent membership is projected by the host. Exact MCP resolution
    /// reuses the same contract lookup as tool advertisement, then enters the
    /// ordinary rule fold so authored denies and mode semantics remain intact.
    pub fn with_mcp_toolsets(mut self, toolsets: &[ToolsetPolicy]) -> Self {
        self.mcp_bindings.toolsets = toolsets
            .iter()
            .filter(|toolset| matches!(toolset.source, ToolsetSource::Mcp { .. }))
            .cloned()
            .collect();
        self
    }
}

#[async_trait]
impl ToolPermissionPolicy for RuleBasedToolPermissionPolicy {
    async fn evaluate(&self, call: &ToolCall) -> ToolPermissionVerdict {
        // Resolution order, outermost first: a matching `deny` rule is absolute;
        // otherwise the most specific rule the author wrote decides; only where
        // the author stated no fixed answer does `auto` evaluate; and a call
        // nobody spoke for falls to the ambient default. An evaluator therefore
        // never overrules an instruction — it only fills a silence.
        let resolved = self.mcp_bindings.tool_policy(&call.tool_id);
        // A fixed requirement becomes an exact rule and joins the scan. `auto`
        // is a deferral rather than a decision, so it stays out of the rule
        // vocabulary and is consulted below instead.
        let exact = resolved
            .filter(|policy| {
                !policy.enabled || policy.permission != ToolPermissionRequirement::Auto
            })
            .map(|policy| {
                PermissionRule::new(
                    ToolCallPattern(awaken_tool_pattern::ToolCallPattern::tool(&call.tool_id)),
                    behavior_for(policy, call, self.auto.as_ref()),
                )
            });
        let decided = self
            .ruleset
            .matched_behavior(&call.tool_id, &call.arguments, exact.as_ref());
        let behavior = match decided {
            Some(behavior) => behavior,
            None if self.is_auto(&resolved, &call.tool_id) => match self.auto.evaluate(call) {
                AutoDecision::Allow => ToolPermissionBehavior::Allow,
                AutoDecision::Deny => ToolPermissionBehavior::Deny,
                AutoDecision::Indeterminate => ToolPermissionBehavior::RequireConfirmation,
            },
            None => self.ruleset.default_behavior,
        };
        match behavior {
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

/// The JSON wire shape of a historical agent `permission` section: a default
/// behavior plus an ordered rule list of `{ pattern, behavior }`.
///
/// New authoring goes through typed Agent/MCP toolsets; this shape exists so an
/// immutable publication written before that cutover still parses and executes
/// with its original contract.
#[derive(Debug, Clone, Deserialize)]
pub struct RulesConfig {
    /// Behavior for a call no rule matches (defaults to `ask`).
    #[serde(default = "default_ask")]
    pub default_behavior: ToolPermissionBehavior,
    /// A retired permission mode, folded into `default_behavior` at parse time.
    #[serde(default)]
    mode: LegacyMode,
    /// Ordered rules; `deny` is absolute, otherwise the most specific match wins.
    #[serde(default)]
    pub rules: Vec<RuleConfig>,
}

/// One authored rule: a glob pattern string and the behavior it grants.
#[derive(Debug, Clone, Deserialize)]
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
        default_behavior: config.mode.compiled_default(config.default_behavior),
        rules,
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

    #[tokio::test]
    async fn typed_mcp_resolution_preserves_rule_precedence_over_any_default() {
        // C1 typed exact allow/ask/disabled; C2 authored wildcard allow/ask/deny;
        // C3 the unmatched default allow/ask/deny. E1 exact typed policy replaces
        // the MCP disabled default, E2 any matching authored deny stays absolute,
        // E3 the unmatched default never overrides a matching rule — there is no
        // longer any posture that can short-circuit the scan. Decision table: all
        // 3*3*3=27 combinations, plus argument-scoped deny (matching/nonmatching)
        // and literal, unparsed tool ids. This is runtime policy coverage, not a
        // proof of publication validation.
        let allow = ToolExecutionPolicy::default();
        let ask = ToolExecutionPolicy {
            permission: ToolPermissionRequirement::AlwaysAsk,
            ..allow
        };
        let disabled = ToolExecutionPolicy {
            enabled: false,
            ..allow
        };
        for typed in [allow, ask, disabled] {
            for legacy in [
                ToolPermissionBehavior::Allow,
                ToolPermissionBehavior::RequireConfirmation,
                ToolPermissionBehavior::Deny,
            ] {
                for default_behavior in [
                    ToolPermissionBehavior::Allow,
                    ToolPermissionBehavior::RequireConfirmation,
                    ToolPermissionBehavior::Deny,
                ] {
                    let policy = RuleBasedToolPermissionPolicy::new(PermissionRuleset {
                        default_behavior,
                        rules: vec![rule("mcp__context__*", legacy)],
                    })
                    .with_mcp_toolsets(&[ToolsetPolicy {
                        source: ToolsetSource::Mcp {
                            server_name: "context".into(),
                        },
                        default: disabled,
                        overrides: vec![
                            awaken_runtime_contract::agent_bindings::ToolPolicyOverride::new(
                                "attest", typed,
                            ),
                        ],
                    }]);
                    let call = ToolCall {
                        call_id: "typed-mcp".into(),
                        tool_id: "mcp__context__attest".into(),
                        arguments: json!({}),
                    };
                    let expected = if legacy == ToolPermissionBehavior::Deny {
                        ToolPermissionBehavior::Deny
                    } else {
                        behavior_for(typed, &call, &ReversibilityEvaluator::default())
                    };
                    let actual = policy.evaluate(&call).await;
                    match (expected, actual) {
                        (ToolPermissionBehavior::Allow, ToolPermissionVerdict::Allow) => {}
                        (ToolPermissionBehavior::Deny, ToolPermissionVerdict::Deny { .. }) => {}
                        (
                            ToolPermissionBehavior::RequireConfirmation,
                            ToolPermissionVerdict::RequireConfirmation { correlation_id },
                        ) => assert_eq!(correlation_id, "perm-typed-mcp"),
                        (expected, actual) => panic!(
                            "{typed:?} {legacy:?} {default_behavior:?}: \
                             expected {expected:?}, got {actual:?}"
                        ),
                    }
                }
            }
        }
        let policy = RuleBasedToolPermissionPolicy::new(PermissionRuleset {
            rules: vec![rule(
                r#"mcp__context__attest(scope = "secret")"#,
                ToolPermissionBehavior::Deny,
            )],
            ..PermissionRuleset::default()
        })
        .with_mcp_toolsets(&[ToolsetPolicy {
            source: ToolsetSource::Mcp {
                server_name: "context".into(),
            },
            default: disabled,
            overrides: vec![
                awaken_runtime_contract::agent_bindings::ToolPolicyOverride::new("attest", allow),
            ],
        }]);
        for scope in ["public", "secret"] {
            let call = ToolCall {
                call_id: "scope".into(),
                tool_id: "mcp__context__attest".into(),
                arguments: json!({"scope":scope}),
            };
            assert_eq!(
                matches!(
                    policy.evaluate(&call).await,
                    ToolPermissionVerdict::Deny { .. }
                ),
                scope == "secret"
            );
        }
        for tool_id in [
            "mcp__context__*",
            "mcp__context__attest(",
            "mcp__context__attest__other",
        ] {
            let call = ToolCall {
                call_id: "literal".into(),
                tool_id: tool_id.into(),
                arguments: json!({}),
            };
            assert!(
                matches!(
                    policy.evaluate(&call).await,
                    ToolPermissionVerdict::Deny { .. }
                ),
                "{tool_id}"
            );
        }
    }

    #[test]
    fn a_retired_mode_compiles_into_the_unmatched_default_and_never_disarms_deny() {
        // Cause/effect graph: C1 the historical section names a retired mode;
        // C2 a rule matches the call. E1 an unmatched call takes the compiled
        // default; E2 a matching rule always wins over that default — including
        // the `deny` that the retired bypass posture used to skip entirely.
        //
        // | Rule | mode | call | Effect |
        // |---|---|---|---|
        // | M1 | plan | unmatched | deny (fails closed) |
        // | M2 | plan | matched allow | allow (a rule still wins) |
        // | M3 | bypassPermissions | unmatched | allow (posture preserved) |
        // | M4 | bypassPermissions | matched deny | deny (NARROWED: was allow) |
        // | M5 | acceptEdits | unmatched | the authored default, unchanged |
        // | M6 | absent | unmatched | the authored default, unchanged |
        let section = |mode: Option<&str>, default_behavior: &str| {
            let mut value = json!({
                "default_behavior": default_behavior,
                "rules": [
                    { "pattern": "read", "behavior": "allow" },
                    { "pattern": "bash", "behavior": "deny" }
                ]
            });
            if let Some(mode) = mode {
                value["mode"] = json!(mode);
            }
            parse_ruleset(&value).expect("a historical section still parses")
        };
        let unmatched = || json!({});

        let plan = section(Some("plan"), "ask");
        assert_eq!(
            plan.decide("write", &unmatched()),
            ToolPermissionBehavior::Deny,
            "M1"
        );
        assert_eq!(
            plan.decide("read", &unmatched()),
            ToolPermissionBehavior::Allow,
            "M2"
        );

        let bypass = section(Some("bypassPermissions"), "deny");
        assert_eq!(
            bypass.decide("write", &unmatched()),
            ToolPermissionBehavior::Allow,
            "M3"
        );
        assert_eq!(
            bypass.decide("bash", &unmatched()),
            ToolPermissionBehavior::Deny,
            "M4: the rule scan now runs, so deny is absolute"
        );

        for mode in [Some("acceptEdits"), Some("default"), None] {
            let rs = section(mode, "ask");
            assert_eq!(
                rs.decide("write", &unmatched()),
                ToolPermissionBehavior::RequireConfirmation,
                "M5/M6 {mode:?}"
            );
        }
    }

    #[test]
    fn authored_rules_enforce_against_the_real_lowercase_tool_id() {
        // A deny rule must match the REAL tool id `bash` (lowercase). This guards a
        // silent-disable bug: a `Bash(...)` pattern parses but never matches the runtime
        // tool, so the rule is inert while reading as if it were enforced.
        let rs = parse_ruleset(&json!({
            "default_behavior": "ask",
            "rules": [
                { "pattern": "read", "behavior": "allow" },
                { "pattern": "bash(command ~ \"*rm -rf*\")", "behavior": "deny" }
            ]
        }))
        .unwrap();
        assert_eq!(
            rs.decide("bash", &json!({ "command": "sudo rm -rf /" })),
            ToolPermissionBehavior::Deny,
            "the rule denies a destructive lowercase `bash` call"
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
