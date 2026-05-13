//! Permission preview service — answers "what tools can the model actually
//! see for this agent after the permission plugin filters?".
//!
//! Static analysis only: walks the agent's declared `allowed_tools` /
//! `excluded_tools` over the tool registry to compute the candidate set,
//! then subtracts tools that any permission rule marks as unconditionally
//! denied (matching `Deny` + exact tool + `ArgMatcher::Any`). Rules whose
//! match depends on runtime arguments are surfaced separately as
//! informational entries — they cannot be resolved without an actual tool
//! call, but the editor can show that "Edit" will be `ask` only when the
//! path matches a glob, etc.
//!
//! Replaces the misleading frontend port reverted in PR #189 G6.

use std::collections::HashSet;

use awaken_contract::AgentSpec;
use awaken_ext_permission::{
    ArgMatcher, PermissionConfigKey, PermissionRule, PermissionRulesConfig, PermissionRuleset,
    PermissionSubject, ToolCallPattern, ToolMatcher, ToolPermissionBehavior,
};
use serde::Serialize;

use crate::app::AppState;
use crate::services::config_service::{ConfigNamespace, ConfigService, ConfigServiceError};

#[derive(Debug, Clone, Serialize)]
pub struct PermissionPreviewResponse {
    pub agent_id: String,
    /// `true` when the permission plugin is loaded (`plugin_ids` contains
    /// `"permission"`) AND `active_hook_filter` admits permission hooks
    /// (filter is empty, or explicitly contains `"permission"`). When
    /// `false` the runtime won't run any permission BeforeInference hooks,
    /// so `effective_tools` equals `candidate_tools` and no rules are
    /// surfaced.
    pub permission_plugin_enabled: bool,
    /// Default behavior when no rule matches a call. `None` when the
    /// permission plugin isn't enabled.
    pub default_behavior: Option<String>,
    /// `allowed_tools ∖ excluded_tools` over the full tool registry.
    pub candidate_tools: Vec<String>,
    /// Tools from `candidate_tools` that the BeforeInference hook will
    /// unconditionally strip — i.e. only the deny rules that bite a tool
    /// the model would otherwise see. Deny rules whose tool target falls
    /// outside the candidate set (denied tool wasn't allowed to begin
    /// with) are NOT counted here so the UI's "stripped before model"
    /// summary doesn't overstate the filter.
    pub unconditionally_denied: Vec<String>,
    /// `candidate_tools ∖ unconditionally_denied`. This is what the model
    /// actually sees in the tool list it's offered. Per-call args-dependent
    /// rules can still gate / Ask / Deny at invocation time — see
    /// `args_conditional_rules` below.
    pub effective_tools: Vec<String>,
    /// Rules whose match depends on runtime arguments. Informational only —
    /// the editor surfaces them so the user can see "Edit will be denied
    /// when path matches /etc/*".
    pub args_conditional_rules: Vec<ArgConditionalRule>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArgConditionalRule {
    pub tool: String,
    pub behavior: String,
    pub pattern: String,
}

#[derive(Debug, thiserror::Error)]
pub enum PermissionPreviewError {
    #[error(transparent)]
    Config(#[from] ConfigServiceError),
    #[error("agent `{0}` not found")]
    AgentNotFound(String),
    #[error("invalid agent spec: {0}")]
    InvalidSpec(String),
    #[error("invalid permission config for agent `{agent_id}`: {reason}")]
    InvalidPermissionConfig { agent_id: String, reason: String },
    #[error("runtime registry not available")]
    RegistryUnavailable,
}

/// Run the preview for the given agent id and return the analysis.
pub async fn preview_agent_permissions(
    state: &AppState,
    agent_id: &str,
) -> Result<PermissionPreviewResponse, PermissionPreviewError> {
    let service = ConfigService::new(state).map_err(PermissionPreviewError::Config)?;
    let raw = service
        .get(ConfigNamespace::Agents, agent_id)
        .await
        .map_err(PermissionPreviewError::Config)?
        .ok_or_else(|| PermissionPreviewError::AgentNotFound(agent_id.to_string()))?;

    let spec: AgentSpec = serde_json::from_value(raw)
        .map_err(|err| PermissionPreviewError::InvalidSpec(err.to_string()))?;

    let registries = state
        .runtime
        .registry_set()
        .ok_or(PermissionPreviewError::RegistryUnavailable)?;
    let all_tools: Vec<String> = registries.tools.tool_ids().into_iter().collect();
    let all_tool_set: HashSet<&str> = all_tools.iter().map(String::as_str).collect();

    // candidate = (allowed ∩ registry).unwrap_or(registry) ∖ excluded
    //
    // INTERSECT WITH REGISTRY: an agent's `allowed_tools` is a string list
    // written into config; nothing forces every entry to correspond to a
    // currently-registered tool. Without filtering against the registry,
    // a stale id (renamed plugin, removed MCP server, typo) would show up
    // in `effective_tools` as if the model could call it — but the
    // runtime tool catalog never offers it. The preview must mirror what
    // the model actually sees.
    let excluded: HashSet<&str> = spec
        .excluded_tools
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(String::as_str)
        .collect();
    let candidate_iter: Box<dyn Iterator<Item = String>> = match &spec.allowed_tools {
        Some(allowed) => Box::new(
            allowed
                .iter()
                .filter(|tool| all_tool_set.contains(tool.as_str()))
                .cloned(),
        ),
        None => Box::new(all_tools.iter().cloned()),
    };
    let mut candidate_tools: Vec<String> = candidate_iter
        .filter(|tool| !excluded.contains(tool.as_str()))
        .collect();
    candidate_tools.sort();
    candidate_tools.dedup();

    // The permission plugin is "enabled" for preview purposes only when it
    // is both loaded AND its hooks will actually run for this agent. The
    // runtime's hook dispatcher (phase/engine.rs) filters hooks through
    // `active_hook_filter`: empty filter = all hooks run, non-empty filter
    // = only listed plugins' hooks run. If permission is loaded but
    // filtered out, no BeforeInference filtering happens, so the preview
    // must report the candidate set verbatim instead of claiming tools
    // would be stripped.
    let permission_loaded = spec.plugin_ids.iter().any(|id| id == "permission");
    let permission_hooks_active = spec.active_hook_filter.is_empty()
        || spec.active_hook_filter.iter().any(|id| id == "permission");
    let permission_plugin_enabled = permission_loaded && permission_hooks_active;
    if !permission_plugin_enabled {
        return Ok(PermissionPreviewResponse {
            agent_id: agent_id.to_string(),
            permission_plugin_enabled: false,
            default_behavior: None,
            effective_tools: candidate_tools.clone(),
            candidate_tools,
            unconditionally_denied: Vec::new(),
            args_conditional_rules: Vec::new(),
        });
    }

    // Try to load the agent's permission section. Missing section is
    // equivalent to "no rules, default deny" depending on how the plugin
    // initialises — we treat it as "permission plugin enabled but no
    // ruleset configured" (default_behavior=Ask, no rules).
    let perm_config: PermissionRulesConfig = match spec.config::<PermissionConfigKey>() {
        Ok(cfg) => cfg,
        Err(err) => {
            return Err(PermissionPreviewError::InvalidPermissionConfig {
                agent_id: agent_id.to_string(),
                reason: err.to_string(),
            });
        }
    };

    let default_behavior = behavior_label(perm_config.default_behavior);
    let ruleset: PermissionRuleset = perm_config.into_ruleset().map_err(|err| {
        PermissionPreviewError::InvalidPermissionConfig {
            agent_id: agent_id.to_string(),
            reason: err.to_string(),
        }
    })?;

    // Unconditional deny = exact-tool Deny ∪ (glob/regex Deny + Any args
    // expanded against the registry). The runtime BeforeInference hook
    // (`PermissionToolFilterHook`) strips the same set via the shared
    // helper, so a `Bash(npm *)` Deny stays in `args_conditional_rules`
    // while `mcp__db__*` Deny is expanded into every matching registry
    // id here AND removed from the model's tool list at runtime.
    let denied: HashSet<String> = ruleset.unconditionally_denied_against(&all_tools);
    let effective_tools: Vec<String> = candidate_tools
        .iter()
        .filter(|tool| !denied.contains(*tool))
        .cloned()
        .collect();
    // Only tools the model *would* otherwise see are "stripped" by the
    // permission layer. A deny rule for a tool that was already excluded
    // by `allowed_tools` / `excluded_tools` (or simply not in the
    // candidate set for any reason) is not surfaced as a strip — the UI
    // summary "N tools stripped before the model sees the list" must
    // count only real strips.
    let candidate_set: HashSet<&str> = candidate_tools.iter().map(String::as_str).collect();
    let mut unconditionally_denied: Vec<String> = denied
        .into_iter()
        .filter(|tool| candidate_set.contains(tool.as_str()))
        .collect();
    unconditionally_denied.sort();

    // R12 #1 — Surface only rules whose target tool the model could
    // actually call. We filter against `effective_tools` (candidate
    // minus unconditionally-denied) so that a rule on an unconditionally
    // denied tool — which the model would never call regardless of args
    // — does NOT show up as an args-conditional surprise. R10 used
    // `candidate_tools`, which still listed args rules for tools that
    // had already been stripped by the BeforeInference hook.
    let args_conditional_rules = collect_args_conditional_rules(&ruleset, &effective_tools);

    Ok(PermissionPreviewResponse {
        agent_id: agent_id.to_string(),
        permission_plugin_enabled: true,
        default_behavior: Some(default_behavior.to_string()),
        candidate_tools,
        unconditionally_denied,
        effective_tools,
        args_conditional_rules,
    })
}

fn behavior_label(behavior: ToolPermissionBehavior) -> &'static str {
    match behavior {
        ToolPermissionBehavior::Allow => "allow",
        ToolPermissionBehavior::Ask => "ask",
        ToolPermissionBehavior::Deny => "deny",
    }
}

/// `callable_tools` is the set of tool ids the model could actually
/// invoke at runtime — i.e. `effective_tools = candidate_tools ∖
/// unconditionally_denied`. Rules targeting tools outside this set
/// can never fire and are filtered out so the preview doesn't list
/// stale guards.
fn collect_args_conditional_rules(
    ruleset: &PermissionRuleset,
    callable_tools: &[String],
) -> Vec<ArgConditionalRule> {
    let mut out = Vec::new();
    for rule in ruleset.rules.values() {
        if let Some(entry) = describe_args_conditional(rule, callable_tools) {
            out.push(entry);
        }
    }
    out.sort_by(|a, b| a.tool.cmp(&b.tool).then_with(|| a.pattern.cmp(&b.pattern)));
    out
}

fn describe_args_conditional(
    rule: &PermissionRule,
    callable_tools: &[String],
) -> Option<ArgConditionalRule> {
    let pattern = match &rule.subject {
        PermissionSubject::Pattern { pattern } => pattern,
        PermissionSubject::Tool { .. } => return None, // tool-only is unconditional
    };
    if matches!(&pattern.args, ArgMatcher::Any) {
        // exact-tool + any-args is unconditional; already captured by
        // `unconditionally_denied_tools` for Deny / behavior-default for
        // others. Skip.
        if matches!(&pattern.tool, ToolMatcher::Exact(_)) {
            return None;
        }
        // Glob/regex tool + any-args + Deny is now expanded against the
        // registry into the unconditionally-denied set, so it should NOT
        // also show up here — that would double-count. Allow/Ask glob
        // rules still get surfaced informationally ("all mcp__db__* are
        // auto-allowed without confirmation", etc.).
        if rule.behavior == ToolPermissionBehavior::Deny {
            return None;
        }
    }
    // R10 #3 / R12 #1 — A rule whose tool target is outside the
    // callable set (excluded by `excluded_tools`, missing from
    // `allowed_tools`, unregistered, OR already unconditionally
    // denied) cannot fire at runtime. Drop it from the preview so the
    // operator sees only actionable entries.
    match &pattern.tool {
        ToolMatcher::Exact(name) => {
            if !callable_tools.iter().any(|tool| tool == name) {
                return None;
            }
        }
        ToolMatcher::Glob(_) | ToolMatcher::Regex(_) => {
            if !callable_tools
                .iter()
                .any(|tool| rule.subject.matches_tool(tool))
            {
                return None;
            }
        }
    }
    Some(ArgConditionalRule {
        tool: tool_display(&pattern.tool),
        behavior: behavior_label(rule.behavior).to_string(),
        pattern: ToolCallPattern::to_string(pattern),
    })
}

fn tool_display(matcher: &ToolMatcher) -> String {
    match matcher {
        ToolMatcher::Exact(name) => name.clone(),
        ToolMatcher::Glob(g) => g.to_string(),
        ToolMatcher::Regex(r) => format!("/{}/", r.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_ext_permission::PermissionRulesConfig;
    use serde_json::json;

    fn ruleset_from_json(value: serde_json::Value) -> PermissionRuleset {
        let cfg: PermissionRulesConfig =
            serde_json::from_value(value).expect("valid permission config");
        cfg.into_ruleset().expect("compile ruleset")
    }

    #[test]
    fn args_conditional_skips_exact_any_args() {
        // Bash with no args = unconditional (whether allow/ask/deny).
        let ruleset = ruleset_from_json(json!({
            "default_behavior": "ask",
            "rules": [
                { "tool": "Bash", "behavior": "deny" },
            ]
        }));
        let candidate = vec!["Bash".to_string()];
        let entries = collect_args_conditional_rules(&ruleset, &candidate);
        assert!(entries.is_empty(), "exact tool any-args isn't conditional");
    }

    #[test]
    fn args_conditional_surfaces_primary_glob() {
        let ruleset = ruleset_from_json(json!({
            "default_behavior": "ask",
            "rules": [
                { "tool": "Bash(npm *)", "behavior": "allow" },
            ]
        }));
        let candidate = vec!["Bash".to_string()];
        let entries = collect_args_conditional_rules(&ruleset, &candidate);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].tool, "Bash");
        assert_eq!(entries[0].behavior, "allow");
        assert!(entries[0].pattern.contains("Bash"));
    }

    #[test]
    fn args_conditional_surfaces_glob_tool_any_args_for_non_deny() {
        // Glob tool + any args + Allow/Ask is "tool-name dependent" —
        // surfaced because the user wants to see it covers a set of
        // dynamically-discovered tools.
        let ruleset = ruleset_from_json(json!({
            "default_behavior": "ask",
            "rules": [
                { "tool": "mcp__db__*", "behavior": "ask" },
            ]
        }));
        let candidate = vec!["mcp__db__query".to_string()];
        let entries = collect_args_conditional_rules(&ruleset, &candidate);
        assert_eq!(entries.len(), 1);
    }

    // R10 #3 — rules whose tool target is outside the candidate set
    // can never fire at runtime; the preview must drop them so the
    // operator doesn't see stale entries for excluded or unregistered
    // tools.
    #[test]
    fn args_conditional_drops_exact_rule_outside_candidate() {
        let ruleset = ruleset_from_json(json!({
            "default_behavior": "ask",
            "rules": [
                { "tool": "Read(/etc/*)", "behavior": "deny" },
            ]
        }));
        // `Read` is not in candidate (user restricted `allowed_tools` to
        // just `Bash`, or excluded `Read`, etc.) — the args-conditional
        // entry should be filtered out.
        let candidate = vec!["Bash".to_string()];
        let entries = collect_args_conditional_rules(&ruleset, &candidate);
        assert!(
            entries.is_empty(),
            "rule targeting a non-candidate tool must be dropped"
        );
    }

    #[test]
    fn args_conditional_drops_glob_rule_with_no_candidate_match() {
        let ruleset = ruleset_from_json(json!({
            "default_behavior": "ask",
            "rules": [
                { "tool": "mcp__db__*", "behavior": "ask" },
            ]
        }));
        // Candidate has no `mcp__db__*` tools — glob doesn't bite
        // anything the model can call.
        let candidate = vec!["Bash".to_string(), "Read".to_string()];
        let entries = collect_args_conditional_rules(&ruleset, &candidate);
        assert!(
            entries.is_empty(),
            "glob rule that matches no candidate tool must be dropped"
        );
    }

    #[test]
    fn args_conditional_keeps_exact_rule_inside_candidate() {
        let ruleset = ruleset_from_json(json!({
            "default_behavior": "ask",
            "rules": [
                { "tool": "Bash(npm *)", "behavior": "ask" },
            ]
        }));
        let candidate = vec!["Bash".to_string()];
        let entries = collect_args_conditional_rules(&ruleset, &candidate);
        assert_eq!(entries.len(), 1, "rule on candidate tool stays");
    }

    #[test]
    fn behavior_label_maps_each_variant() {
        assert_eq!(behavior_label(ToolPermissionBehavior::Allow), "allow");
        assert_eq!(behavior_label(ToolPermissionBehavior::Ask), "ask");
        assert_eq!(behavior_label(ToolPermissionBehavior::Deny), "deny");
    }

    // The unit tests for glob/regex Deny expansion moved to
    // `awaken_ext_permission::rules::tests::against_*` along with the
    // helper — `PermissionRuleset::unconditionally_denied_against` is the
    // single source of truth for this expansion, shared between the
    // runtime BeforeInference filter and this preview.

    #[test]
    fn args_conditional_no_longer_lists_glob_deny_any_args() {
        // After R7 #3, glob/regex Deny + any args is expanded into
        // `unconditionally_denied`. To avoid double-display, it must
        // disappear from args_conditional_rules.
        let ruleset = ruleset_from_json(json!({
            "default_behavior": "ask",
            "rules": [
                { "tool": "mcp__db__*", "behavior": "deny" },
            ]
        }));
        let candidate = vec!["mcp__db__query".to_string()];
        let entries = collect_args_conditional_rules(&ruleset, &candidate);
        assert!(
            entries.is_empty(),
            "glob Deny + any-args is now unconditional"
        );
    }

    // R12 #1 — Passing the EFFECTIVE set (candidate minus
    // unconditionally-denied) drops args-conditional rules whose tool
    // target the model can no longer call. Previously the call site
    // passed the raw `candidate_tools`, leaving stale entries that
    // implied "Bash will still be denied when args match" even though
    // Bash itself was already stripped by the BeforeInference hook.
    #[test]
    fn args_conditional_drops_rules_on_unconditionally_denied_tools() {
        let ruleset = ruleset_from_json(json!({
            "default_behavior": "ask",
            "rules": [
                // Unconditional deny for Bash — strips the tool entirely.
                { "tool": "Bash", "behavior": "deny" },
                // Args-conditional rule on the SAME tool — cannot fire
                // once Bash is removed from the model's tool list.
                { "tool": "Bash(npm *)", "behavior": "ask" },
            ]
        }));
        // The call site passes effective_tools = candidate ∖ denied.
        // Here Bash is denied, so it's not in `effective`.
        let effective = vec!["Read".to_string()];
        let entries = collect_args_conditional_rules(&ruleset, &effective);
        assert!(
            entries.is_empty(),
            "args-conditional rule on a denied tool must be dropped, got: {entries:?}"
        );
    }
}
