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
    /// `true` when the agent has the permission plugin in `plugin_ids` AND
    /// a permission config section. When `false` the `effective_tools` are
    /// just the candidate set with no further filtering.
    pub permission_plugin_enabled: bool,
    /// Default behavior when no rule matches a call. `None` when the
    /// permission plugin isn't enabled.
    pub default_behavior: Option<String>,
    /// `allowed_tools ∖ excluded_tools` over the full tool registry.
    pub candidate_tools: Vec<String>,
    /// Tools the BeforeInference hook will unconditionally strip (any rule
    /// matching `Deny` + exact tool + `args == Any`). Empty when permission
    /// plugin is disabled.
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

    // candidate = (allowed.unwrap_or(all)) ∖ excluded
    let excluded: HashSet<&str> = spec
        .excluded_tools
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(String::as_str)
        .collect();
    let candidate_iter: Box<dyn Iterator<Item = String>> = match &spec.allowed_tools {
        Some(allowed) => Box::new(allowed.iter().cloned()),
        None => Box::new(all_tools.iter().cloned()),
    };
    let mut candidate_tools: Vec<String> = candidate_iter
        .filter(|tool| !excluded.contains(tool.as_str()))
        .collect();
    candidate_tools.sort();
    candidate_tools.dedup();

    // If the permission plugin isn't in the plugin list, there's no
    // further filtering to do — the model sees the candidate set as-is.
    let permission_plugin_enabled = spec.plugin_ids.iter().any(|id| id == "permission");
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

    let denied: HashSet<String> = ruleset
        .unconditionally_denied_tools()
        .into_iter()
        .map(str::to_string)
        .collect();
    let effective_tools: Vec<String> = candidate_tools
        .iter()
        .filter(|tool| !denied.contains(*tool))
        .cloned()
        .collect();
    let mut unconditionally_denied: Vec<String> = denied.into_iter().collect();
    unconditionally_denied.sort();

    let args_conditional_rules = collect_args_conditional_rules(&ruleset);

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

fn collect_args_conditional_rules(ruleset: &PermissionRuleset) -> Vec<ArgConditionalRule> {
    let mut out = Vec::new();
    for rule in ruleset.rules.values() {
        if let Some(entry) = describe_args_conditional(rule) {
            out.push(entry);
        }
    }
    out.sort_by(|a, b| a.tool.cmp(&b.tool).then_with(|| a.pattern.cmp(&b.pattern)));
    out
}

fn describe_args_conditional(rule: &PermissionRule) -> Option<ArgConditionalRule> {
    let pattern = match &rule.subject {
        PermissionSubject::Pattern { pattern } => pattern,
        PermissionSubject::Tool { .. } => return None, // tool-only is unconditional
    };
    if matches!(&pattern.args, ArgMatcher::Any) {
        // exact-tool + any-args is unconditional; already captured by
        // `unconditionally_denied_tools` for Deny. Glob/regex tool with
        // any-args is conditional on the tool name match but not args —
        // we still surface it because the user wants to see "any mcp__db__*
        // gets Ask'd" without it being mistaken for a deny.
        if matches!(&pattern.tool, ToolMatcher::Exact(_)) {
            return None;
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
        let entries = collect_args_conditional_rules(&ruleset);
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
        let entries = collect_args_conditional_rules(&ruleset);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].tool, "Bash");
        assert_eq!(entries[0].behavior, "allow");
        assert!(entries[0].pattern.contains("Bash"));
    }

    #[test]
    fn args_conditional_surfaces_glob_tool_any_args() {
        // mcp__db__* (glob tool) + any args is still "tool-name dependent"
        // — surfaced because the user wants to see it covers a set of
        // dynamically-discovered tools.
        let ruleset = ruleset_from_json(json!({
            "default_behavior": "ask",
            "rules": [
                { "tool": "mcp__db__*", "behavior": "ask" },
            ]
        }));
        let entries = collect_args_conditional_rules(&ruleset);
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn behavior_label_maps_each_variant() {
        assert_eq!(behavior_label(ToolPermissionBehavior::Allow), "allow");
        assert_eq!(behavior_label(ToolPermissionBehavior::Ask), "ask");
        assert_eq!(behavior_label(ToolPermissionBehavior::Deny), "deny");
    }
}
