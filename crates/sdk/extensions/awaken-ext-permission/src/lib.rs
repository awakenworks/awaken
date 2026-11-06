// @formal entrypoint permission.extension_policy_merge_is_exact_and_fail_closed
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
use awaken_agent_contract::audit::permission::{PermissionPolicy, PermissionReasonCode};
use awaken_agent_contract::{
    ToolExecutionPolicy, ToolPermissionRequirement, ToolsetPolicy, ToolsetSource,
};
use awaken_runtime_contract::agent_bindings::AgentBindings;
use awaken_runtime_contract::permission::{
    PermissionBasis, ToolPermissionPolicy, ToolPermissionRuling, ToolPermissionVerdict,
};
use awaken_tool_contract::tool::ToolCall;
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

impl AutoDecision {
    /// The behavior this verdict resolves to. Stated once: `Indeterminate` is
    /// the ask an `always_ask` policy would have produced, so a call the
    /// evaluator cannot judge reaches a person through the same ticket path.
    #[must_use]
    pub const fn into_behavior(self) -> ToolPermissionBehavior {
        match self {
            Self::Allow => ToolPermissionBehavior::Allow,
            Self::Deny => ToolPermissionBehavior::Deny,
            Self::Indeterminate => ToolPermissionBehavior::RequireConfirmation,
        }
    }

    /// The grounds this judgement records in the audit fact (ADR-0030 D4): a
    /// refusal was judged high-risk, an ask reached no conclusion, and an allow
    /// carries none.
    #[must_use]
    pub const fn reason_code(self) -> Option<PermissionReasonCode> {
        match self {
            Self::Allow => None,
            Self::Deny => Some(PermissionReasonCode::HIGH_RISK),
            Self::Indeterminate => Some(PermissionReasonCode::INDETERMINATE),
        }
    }

    /// The complete audit basis of this judgement under the `auto` policy.
    #[must_use]
    pub const fn basis(self) -> PermissionBasis {
        PermissionBasis::auto(self.reason_code())
    }
}

/// Decides one call under the `auto` policy.
///
/// Implementations are supplied at construction, which is what keeps this crate
/// linkable on its own: an embedded runtime gets [`ObservingToolEvaluator`]
/// without depending on anything that knows about sessions or servers, while a
/// richer deployment injects its own. It is synchronous on purpose — an
/// evaluator that needs to await external judgement belongs behind the policy
/// port itself, not inside a per-call rule fold.
pub trait AutoPermissionEvaluator: Send + Sync + std::fmt::Debug {
    /// Judge one normalized call.
    fn evaluate(&self, call: &ToolCall) -> AutoDecision;
}

/// The evaluator an embedded runtime gets by default: a call is allowed when
/// the tool it names only observes, and falls to a person otherwise. It is
/// named for the one property it can actually judge from a tool id.
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
pub struct ObservingToolEvaluator {
    observing: std::collections::BTreeSet<String>,
}

impl ObservingToolEvaluator {
    /// Build from the tool ids whose calls only observe.
    #[must_use]
    pub fn new(observing: impl IntoIterator<Item = String>) -> Self {
        Self {
            observing: observing.into_iter().collect(),
        }
    }
}

impl AutoPermissionEvaluator for ObservingToolEvaluator {
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

/// One rule that matched a call: how specifically, what it grants, and which
/// authored policy it states. The policy rides through the fold untouched so
/// the winner names its own origin; it never influences precedence.
type MatchedRule = (Specificity, ToolPermissionBehavior, PermissionPolicy);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchedRuleSelection {
    Deny(PermissionPolicy),
    Continue(Option<MatchedRule>),
}

/// Fold one matching rule into the current non-deny winner. A deny is an
/// absorbing result; otherwise only a strictly more-specific candidate may
/// replace the winner, preserving deterministic first-wins behavior on ties.
fn select_matched_rule(
    best: Option<MatchedRule>,
    specificity: Specificity,
    behavior: ToolPermissionBehavior,
    policy: PermissionPolicy,
) -> MatchedRuleSelection {
    if behavior == ToolPermissionBehavior::Deny {
        return MatchedRuleSelection::Deny(policy);
    }
    if best.is_none_or(|(best_specificity, _, _)| specificity > best_specificity) {
        MatchedRuleSelection::Continue(Some((specificity, behavior, policy)))
    } else {
        MatchedRuleSelection::Continue(best)
    }
}

/// Fold the rules that matched one call into its decision.
///
/// This is the complete precedence rule — deny is absolute, otherwise the most
/// specific match wins, and `None` means no rule spoke for the call — expressed
/// over nothing but `(Specificity, ToolPermissionBehavior, PermissionPolicy)`
/// triples, where the policy is inert cargo naming the winner's origin. It
/// never sees a pattern, a tool id, or an argument.
///
/// That separation is what makes the rule provable. Stating the same precedence
/// inside the matching loop entangled it with heap-allocated patterns and
/// symbolic `serde_json::Value` traversal, so a solver spent its whole budget on
/// string equality instead of on the three-valued fold. Whether a pattern
/// matches belongs to `awaken-tool-pattern`, which proves it; whether a set of
/// matches yields allow, ask or deny belongs here.
///
/// Returning `Option` is load-bearing too: `None` is the seam the `auto` policy
/// occupies, between every decision an author stated and the ambient default.
fn fold_matches(
    matches: impl Iterator<Item = MatchedRule>,
) -> Option<(ToolPermissionBehavior, PermissionPolicy)> {
    let mut best: Option<MatchedRule> = None;
    for (specificity, behavior, policy) in matches {
        match select_matched_rule(best, specificity, behavior, policy) {
            MatchedRuleSelection::Deny(policy) => {
                return Some((ToolPermissionBehavior::Deny, policy));
            }
            MatchedRuleSelection::Continue(selected) => best = selected,
        }
    }
    best.map(|(_, behavior, policy)| (behavior, policy))
}

/// One rule: a pattern, the behavior it grants, and the authored policy it
/// states — what the audit fact names when this rule decides (ADR-0030 D4).
#[derive(Debug, Clone)]
pub struct PermissionRule {
    pub pattern: ToolCallPattern,
    pub behavior: ToolPermissionBehavior,
    pub policy: PermissionPolicy,
}

impl PermissionRule {
    /// An explicit authored rule outside the toolset policy vocabulary.
    pub fn new(pattern: ToolCallPattern, behavior: ToolPermissionBehavior) -> Self {
        Self {
            pattern,
            behavior,
            policy: PermissionPolicy::Rule,
        }
    }

    /// The exact rule a fixed toolset requirement compiles into, or `None` for
    /// an enabled `auto` — a deferral is not a decision and stays out of the
    /// rule vocabulary. A disabled tool is a fixed deny the toolset authored
    /// outside the permission vocabulary, so it audits as `rule`.
    ///
    /// This is deliberately not a `From` conversion: `Auto` needs the call and
    /// an evaluator, which a policy alone does not have.
    #[must_use]
    pub fn from_fixed_policy(
        pattern: ToolCallPattern,
        policy: ToolExecutionPolicy,
    ) -> Option<Self> {
        let (behavior, source) = if !policy.enabled {
            (ToolPermissionBehavior::Deny, PermissionPolicy::Rule)
        } else {
            match policy.permission {
                ToolPermissionRequirement::AlwaysAllow => {
                    (ToolPermissionBehavior::Allow, PermissionPolicy::AlwaysAllow)
                }
                ToolPermissionRequirement::AlwaysAsk => (
                    ToolPermissionBehavior::RequireConfirmation,
                    PermissionPolicy::AlwaysAsk,
                ),
                ToolPermissionRequirement::Auto => return None,
            }
        };
        Some(Self {
            pattern,
            behavior,
            policy: source,
        })
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

    /// The behavior a rule decided for this call and the policy that rule
    /// states, or `None` when none matched.
    ///
    /// Separating "a rule decided" from "nothing matched" is what lets `auto`
    /// sit exactly where it belongs: after every fixed decision the author
    /// stated, and before the ambient default. Folding the default in here
    /// would hide that seam and let an evaluator overrule an authored rule.
    fn matched_rule(
        &self,
        tool_id: &str,
        args: &Value,
        exact: Option<&PermissionRule>,
    ) -> Option<(ToolPermissionBehavior, PermissionPolicy)> {
        // Matching and precedence are separated on purpose. This expression owns
        // only "which rules matched, and how specifically"; `fold_matches` owns
        // the entire precedence rule. Keeping them apart is what lets the
        // precedence rule be proved over scalars, with pattern matching left to
        // the crate that owns it (and proves it).
        fold_matches(exact.into_iter().chain(&self.rules).filter_map(|rule| {
            rule.pattern
                .matches(tool_id, args)
                .map(|specificity| (specificity, rule.behavior, rule.policy))
        }))
    }

    fn decide_with_rule(
        &self,
        tool_id: &str,
        args: &Value,
        exact: Option<&PermissionRule>,
    ) -> ToolPermissionBehavior {
        self.matched_rule(tool_id, args, exact)
            .map_or(self.default_behavior, |(behavior, _)| behavior)
    }
}

#[cfg(test)]
mod exhaustive {
    use super::{LegacyMode, ToolPermissionBehavior};

    const fn listed_mode(mode: LegacyMode) -> usize {
        match mode {
            LegacyMode::Default => 0,
            LegacyMode::AcceptEdits => 1,
            LegacyMode::Plan => 2,
            LegacyMode::BypassPermissions => 3,
        }
    }
    const EVERY_MODE: [LegacyMode; 4] = [
        LegacyMode::Default,
        LegacyMode::AcceptEdits,
        LegacyMode::Plan,
        LegacyMode::BypassPermissions,
    ];

    const fn listed_behavior(behavior: ToolPermissionBehavior) -> usize {
        match behavior {
            ToolPermissionBehavior::Allow => 0,
            ToolPermissionBehavior::RequireConfirmation => 1,
            ToolPermissionBehavior::Deny => 2,
        }
    }
    const EVERY_BEHAVIOR: [ToolPermissionBehavior; 3] = [
        ToolPermissionBehavior::Allow,
        ToolPermissionBehavior::RequireConfirmation,
        ToolPermissionBehavior::Deny,
    ];

    #[test]
    fn every_list_names_each_variant_once_in_match_order() {
        for (index, value) in EVERY_MODE.into_iter().enumerate() {
            assert_eq!(listed_mode(value), index);
        }
        for (index, value) in EVERY_BEHAVIOR.into_iter().enumerate() {
            assert_eq!(listed_behavior(value), index);
        }
    }

    /// @exhaustive LegacyMode(4) x ToolPermissionBehavior(3) = 12
    /// @obligation permission.extension_policy_merge_is_exact_and_fail_closed
    #[test]
    fn retired_mode_compiles_into_one_exact_unmatched_default() {
        // The mode axis is gone; a historical section's mode folds into the one
        // remaining knob. The fold must be exact for every retired value, and it
        // must never turn an authored `deny` default into a weaker one except
        // through the single documented bypass case, which the deny-fold proof
        // shows is a narrowing overall (deny rules regain their authority).
        for mode in EVERY_MODE {
            for authored in EVERY_BEHAVIOR {
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
                // Only the retired bypass posture may produce an allow default out
                // of a stricter authored one; every other mode preserves it exactly.
                if compiled == ToolPermissionBehavior::Allow
                    && authored != ToolPermissionBehavior::Allow
                {
                    assert_eq!(mode, LegacyMode::BypassPermissions);
                }
            }
        }
    }
}

#[cfg(kani)]
mod kani_proofs {
    use super::{
        MatchedRule, MatchedRuleSelection, PermissionPolicy, Specificity, ToolPermissionBehavior,
        fold_matches, select_matched_rule,
    };

    fn arbitrary_policy() -> PermissionPolicy {
        match kani::any::<u8>() % 4 {
            0 => PermissionPolicy::AlwaysAllow,
            1 => PermissionPolicy::AlwaysAsk,
            2 => PermissionPolicy::Auto,
            _ => PermissionPolicy::Rule,
        }
    }

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

    /// A bounded symbolic match set: up to three rules matched this call, each
    /// with its own specificity and behavior. Absence models a rule that did
    /// not match, so the empty set — "nobody spoke for this call" — is inside
    /// the explored space rather than a separate case.
    fn symbolic_matches() -> [Option<MatchedRule>; 3] {
        let entry = || {
            if kani::any() {
                Some((
                    arbitrary_specificity(),
                    arbitrary_behavior(),
                    arbitrary_policy(),
                ))
            } else {
                None
            }
        };
        [entry(), entry(), entry()]
    }

    /// @obligation permission.extension_policy_merge_is_exact_and_fail_closed
    #[kani::proof]
    #[kani::unwind(5)]
    fn a_matching_deny_decides_the_whole_fold_whatever_else_matched() {
        // This replaces a proof that restated the fold instead of running it.
        // `fold_matches` is the production precedence rule, and it is executed
        // here over a symbolic match set.
        //
        // Cause/effect graph: C1 some matched rule denies; C2 some matched rule
        // allows, possibly more specifically; C3 nothing matched. E1 deny wins
        // outright and in particular never yields allow — the guarantee the
        // retired bypass posture broke by returning before the scan; E2 an empty
        // set yields `None`, never a decision.
        //
        // | Rule | C1 | C2 | C3 | Effect |
        // |---|---|---|---|---|
        // | D1 | T | * | F | E1 Some(Deny) |
        // | D2 | F | T | F | Some(allow/ask), never Deny |
        // | D3 | F | F | T | E2 None |
        let matches = symbolic_matches();
        let decided = fold_matches(matches.iter().copied().flatten()).map(|(behavior, _)| behavior);

        let any_deny = matches
            .iter()
            .flatten()
            .any(|(_, behavior, _)| *behavior == ToolPermissionBehavior::Deny);
        let nothing_matched = matches.iter().all(Option::is_none);

        if any_deny {
            assert_eq!(decided, Some(ToolPermissionBehavior::Deny), "D1");
        }
        assert!(
            !(any_deny && decided == Some(ToolPermissionBehavior::Allow)),
            "D1: a deny can never surface as allow"
        );
        if nothing_matched {
            assert_eq!(decided, None, "D3");
        }
        // A decision appears exactly when some rule matched: the fold invents
        // nothing, which is what leaves the seam open for `auto`.
        assert_eq!(decided.is_some(), !nothing_matched, "D2/D3");
    }

    /// @obligation permission.extension_policy_merge_is_exact_and_fail_closed
    #[kani::proof]
    #[kani::unwind(5)]
    fn the_fold_returns_a_behavior_some_matching_rule_actually_carried() {
        // Non-invention, stated directly: whatever the fold returns must be a
        // behavior one of the matched rules carried, together with the policy
        // that same rule stated. Without this a "most specific wins"
        // implementation could satisfy the deny law above while still
        // fabricating a verdict nobody authored, or attributing an authored
        // verdict to a policy that did not state it.
        let matches = symbolic_matches();
        let decided = fold_matches(matches.iter().copied().flatten());
        if let Some((behavior, policy)) = decided {
            assert!(
                matches
                    .iter()
                    .flatten()
                    .any(|(_, candidate, origin)| *candidate == behavior && *origin == policy),
                "the decision and its policy are ones a single authored rule carried"
            );
        }
    }

    /// @obligation permission.extension_policy_merge_is_exact_and_fail_closed
    #[kani::proof]
    fn matched_deny_is_absolute_and_only_stricter_non_deny_replaces_authority() {
        let existing = if kani::any() {
            Some((
                arbitrary_specificity(),
                arbitrary_behavior(),
                arbitrary_policy(),
            ))
        } else {
            None
        };
        let candidate_specificity = arbitrary_specificity();
        let candidate_behavior = arbitrary_behavior();
        let candidate_policy = arbitrary_policy();
        let selected = select_matched_rule(
            existing,
            candidate_specificity,
            candidate_behavior,
            candidate_policy,
        );

        if candidate_behavior == ToolPermissionBehavior::Deny {
            assert_eq!(selected, MatchedRuleSelection::Deny(candidate_policy));
            return;
        }
        let expected =
            if existing.is_none_or(|(specificity, _, _)| candidate_specificity > specificity) {
                Some((candidate_specificity, candidate_behavior, candidate_policy))
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
    deferred_members: std::collections::BTreeSet<String>,
}

impl RuleBasedToolPermissionPolicy {
    pub fn new(ruleset: PermissionRuleset) -> Self {
        Self {
            ruleset,
            mcp_bindings: AgentBindings::default(),
            // No observing tools are known until a caller names them, so an
            // unconfigured `auto` sends every call to a person rather than
            // guessing which ids are safe.
            auto: std::sync::Arc::new(ObservingToolEvaluator::default()),
            deferred_members: std::collections::BTreeSet::new(),
        }
    }

    /// Whether this call's authored policy delegates the decision to the
    /// evaluator.
    ///
    /// Two paths, because membership works differently on either side: an open
    /// toolset resolves per call (the id names its server), while a closed
    /// toolset was expanded by a roster-aware caller and its deferred members
    /// were named up front. Neither is a fallback for the other.
    fn is_auto(&self, resolved: &Option<ToolExecutionPolicy>, tool_id: &str) -> bool {
        resolved.is_some_and(|policy| {
            policy.enabled && policy.permission == ToolPermissionRequirement::Auto
        }) || self.deferred_members.contains(tool_id)
    }

    /// Name the members of a CLOSED toolset whose authored policy is `auto`.
    ///
    /// A closed toolset's default is a shorthand: its membership is fixed and
    /// versioned, so a roster-aware caller expands it per member. That expansion
    /// yields two kinds of member, and only one of them can become a rule — a
    /// rule states a decision, and `auto` is precisely the absence of one until
    /// the call is known. The fixed members arrive as rules; the deferred ones
    /// arrive here.
    ///
    /// An OPEN toolset (MCP today) is not expanded at all: its membership is
    /// whatever the server reports, which is unknown when the policy is
    /// authored and can grow afterwards. Its default has to stay a live
    /// fallback, so [`Self::with_mcp_toolsets`] keeps it typed and this policy
    /// resolves it per call. Treating that default as a shorthand would snapshot
    /// the tools known at authoring time and let a newly added server tool
    /// escape the very default that exists to catch it.
    #[must_use]
    pub fn with_deferred_members(mut self, tools: impl IntoIterator<Item = String>) -> Self {
        self.deferred_members = tools.into_iter().collect();
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
    fn process_bound(&self) -> bool {
        true
    }
    async fn evaluate(&self, call: &ToolCall) -> ToolPermissionRuling {
        // Resolution order, outermost first: a matching `deny` rule is absolute;
        // otherwise the most specific rule the author wrote decides; only where
        // the author stated no fixed answer does `auto` evaluate; and a call
        // nobody spoke for falls to the ambient default. An evaluator therefore
        // never overrules an instruction — it only fills a silence.
        let resolved = self.mcp_bindings.tool_policy(&call.tool_id);
        // A fixed requirement becomes an exact rule and joins the scan. `auto`
        // is a deferral rather than a decision, so it stays out of the rule
        // vocabulary and is consulted below instead.
        let exact = resolved.and_then(|policy| {
            PermissionRule::from_fixed_policy(
                ToolCallPattern(awaken_tool_pattern::ToolCallPattern::tool(&call.tool_id)),
                policy,
            )
        });
        let decided = self
            .ruleset
            .matched_rule(&call.tool_id, &call.arguments, exact.as_ref());
        // Every branch names the policy that decided: the matched rule's own,
        // the `auto` judgement with its grounds, or the authored default.
        let (behavior, basis) = match decided {
            Some((behavior, policy)) => (behavior, PermissionBasis::fixed(policy)),
            None if self.is_auto(&resolved, &call.tool_id) => {
                let judgement = self.auto.evaluate(call);
                (judgement.into_behavior(), judgement.basis())
            }
            None => (
                self.ruleset.default_behavior,
                PermissionBasis::fixed(PermissionPolicy::Rule),
            ),
        };
        let verdict = match behavior {
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
        };
        ToolPermissionRuling::policy(verdict, basis)
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
        use awaken_tool_contract::tool::ToolCall;
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
            policy.evaluate(&ctx("Read", json!({}))).await.verdict,
            ToolPermissionVerdict::Allow
        ));
        assert!(matches!(
            policy
                .evaluate(&ctx("Bash", json!({"c": "x rm y"})))
                .await
                .verdict,
            ToolPermissionVerdict::Deny { .. }
        ));
        match policy.evaluate(&ctx("Other", json!({}))).await.verdict {
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
                        overrides: vec![awaken_agent_contract::ToolPolicyOverride::new(
                            "attest", typed,
                        )],
                    }]);
                    let call = ToolCall {
                        call_id: "typed-mcp".into(),
                        tool_id: "mcp__context__attest".into(),
                        arguments: json!({}),
                    };
                    let expected = if legacy == ToolPermissionBehavior::Deny {
                        ToolPermissionBehavior::Deny
                    } else {
                        PermissionRule::from_fixed_policy(
                            ToolCallPattern::parse("mcp__context__attest").unwrap(),
                            typed,
                        )
                        .expect("a fixed requirement compiles into a rule")
                        .behavior
                    };
                    let actual = policy.evaluate(&call).await.verdict;
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
            overrides: vec![awaken_agent_contract::ToolPolicyOverride::new(
                "attest", allow,
            )],
        }]);
        for scope in ["public", "secret"] {
            let call = ToolCall {
                call_id: "scope".into(),
                tool_id: "mcp__context__attest".into(),
                arguments: json!({"scope":scope}),
            };
            assert_eq!(
                matches!(
                    policy.evaluate(&call).await.verdict,
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
                    policy.evaluate(&call).await.verdict,
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
