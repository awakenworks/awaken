use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

use awaken_contract::contract::tool::Tool;
use awaken_contract::registry_spec::AgentSpec;

/// Apply allow/exclude filtering to a mutable tool map.
///
/// Catalog semantics — answers "is this tool visible to the agent?":
///
/// - `allowed_tools = Some(["*"])` -> explicit allow-all (keeps every tool).
/// - `allowed_tools = Some([])` -> explicit empty (removes every tool).
/// - `allowed_tools = Some([patterns..])` -> keep tools whose ID matches any pattern.
/// - `allowed_tools = None` -> deprecated allow-all (emits a rate-limited warning).
///
/// Entries are **tool-id patterns**, not filesystem globs. The grammar is
/// intentionally minimal so `/`, `:`, and `_` are ordinary characters:
///
/// - The full pattern must match the full tool id.
/// - `*` matches any sequence of characters, including `/`, `:`, and `_`.
/// - `\*` and `\\` escape literal `*` and `\`.
/// - Every other character is a literal.
///
/// Argument-level expressions (`Bash(npm *)`) belong in the permission plugin
/// (`sections.permission`) and have no meaning here.
pub(super) fn filter_tools(tools: &mut HashMap<String, Arc<dyn Tool>>, spec: &AgentSpec) {
    let original_tool_ids: Vec<String> = tools.keys().cloned().collect();
    if let Some(allow) = &spec.allowed_tools {
        warn_catalog_argument_patterns(&spec.id, "allowed_tools", allow, &original_tool_ids);
        warn_unmatched_catalog_patterns(&spec.id, "allowed_tools", allow, &original_tool_ids);
        tools.retain(|id, _| catalog_pattern_matches(allow, id));
    } else {
        warn_legacy_allow_all(&spec.id);
    }

    if let Some(exclude) = &spec.excluded_tools {
        warn_catalog_argument_patterns(&spec.id, "excluded_tools", exclude, &original_tool_ids);
        warn_unmatched_catalog_patterns(&spec.id, "excluded_tools", exclude, &original_tool_ids);
        tools.retain(|id, _| !catalog_pattern_matches(exclude, id));
    }
}

/// Return true if any pattern in `patterns` matches `tool_id` under the
/// catalog tool-id grammar (see `awaken_tool_pattern::tool_id_match`).
pub(super) fn catalog_pattern_matches(patterns: &[String], tool_id: &str) -> bool {
    patterns
        .iter()
        .any(|p| awaken_tool_pattern::tool_id_match(p, tool_id))
}

/// Warn when a catalog entry parses as an argument-level pattern (`Bash(npm *)`).
/// Catalog matching is tool-name only; such entries silently never match.
fn warn_catalog_argument_patterns(
    agent_id: &str,
    field: &str,
    patterns: &[String],
    tool_ids: &[String],
) {
    for p in patterns {
        if is_argument_level_catalog_pattern(p)
            || is_argument_syntax_for_registered_tool(p, tool_ids)
        {
            tracing::warn!(
                agent_id = %agent_id,
                field = %field,
                pattern = %p,
                "catalog patterns are tool-name only — argument syntax has no effect; \
                 move this rule to sections[\"permission\"] for argument-level matching"
            );
        }
    }
}

pub(super) fn is_argument_syntax_for_registered_tool(pattern: &str, tool_ids: &[String]) -> bool {
    if !pattern.contains('(') {
        return false;
    }
    let Ok(parsed) = awaken_tool_pattern::parse_pattern(pattern) else {
        return false;
    };
    if !tool_matcher_matches_any(&parsed.tool, tool_ids) {
        return false;
    }
    true
}

pub(super) fn is_argument_level_catalog_pattern(pattern: &str) -> bool {
    let Ok(parsed) = awaken_tool_pattern::parse_pattern(pattern) else {
        return false;
    };
    match parsed.args {
        awaken_tool_pattern::ArgMatcher::Any => false,
        awaken_tool_pattern::ArgMatcher::Fields(_) => true,
        awaken_tool_pattern::ArgMatcher::Primary { value, .. } => {
            value.chars().any(|ch| matches!(ch, '*' | '?' | '[' | ']')) || value.contains(' ')
        }
    }
}

pub(super) fn unmatched_catalog_patterns(patterns: &[String], tool_ids: &[String]) -> Vec<String> {
    if tool_ids.is_empty() {
        return Vec::new();
    }
    patterns
        .iter()
        .filter(|pattern| {
            !is_argument_level_catalog_pattern(pattern)
                && !is_argument_syntax_for_registered_tool(pattern, tool_ids)
        })
        .filter(|pattern| {
            !tool_ids
                .iter()
                .any(|tool_id| awaken_tool_pattern::tool_id_match(pattern, tool_id))
        })
        .cloned()
        .collect()
}

fn warn_unmatched_catalog_patterns(
    agent_id: &str,
    field: &str,
    patterns: &[String],
    tool_ids: &[String],
) {
    for pattern in unmatched_catalog_patterns(patterns, tool_ids) {
        tracing::warn!(
            agent_id = %agent_id,
            field = %field,
            pattern = %pattern,
            "catalog pattern matches no registered tools — check the tool id or wildcard"
        );
    }
}

pub(super) const LEGACY_ALLOW_ALL_WARN_CACHE_LIMIT: usize = 1024;

/// Emit a bounded, per-agent deprecation warning when `allowed_tools` is `null`/absent.
fn warn_legacy_allow_all(agent_id: &str) {
    static WARNED_AGENTS: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
    let warned_agents = WARNED_AGENTS.get_or_init(|| Mutex::new(VecDeque::new()));

    if should_warn_legacy_allow_all(agent_id, warned_agents) {
        tracing::warn!(
            agent_id = %agent_id,
            "allowed_tools=null is deprecated; use [\"*\"] for explicit allow-all \
             or list patterns explicitly (repeated warnings are rate-limited per agent)"
        );
    }
}

pub(super) fn should_warn_legacy_allow_all(
    agent_id: &str,
    warned_agents: &Mutex<VecDeque<String>>,
) -> bool {
    let mut warned_agents = warned_agents
        .lock()
        .expect("legacy allowed_tools warning cache poisoned");
    if warned_agents.iter().any(|warned| warned == agent_id) {
        return false;
    }
    if warned_agents.len() >= LEGACY_ALLOW_ALL_WARN_CACHE_LIMIT {
        warned_agents.pop_front();
    }
    warned_agents.push_back(agent_id.to_string());
    true
}

/// Cross-layer consistency check: emit warnings for permission rules in
/// `sections["permission"]` that reference tools excluded by the catalog.
///
/// Reads the section as raw JSON to avoid taking a dependency on the
/// permission plugin crate. Returns the list of rule pattern strings whose
/// tool-name portion does not match any retained catalog tool.
pub(super) fn permission_rules_without_catalog_match(
    sections: &HashMap<String, serde_json::Value>,
    retained_tool_ids: &[String],
) -> Vec<String> {
    let Some(section) = sections.get("permission") else {
        return Vec::new();
    };
    let Some(rules) = section.get("rules").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };

    let mut orphans = Vec::new();
    for rule in rules {
        let Some(pattern_str) = rule.get("tool").and_then(serde_json::Value::as_str) else {
            continue;
        };
        // Parse pattern; on parse error skip (plugin validation will surface it).
        let Ok(pattern) = awaken_tool_pattern::parse_pattern(pattern_str) else {
            continue;
        };
        let matched = retained_tool_ids.iter().any(|id| match &pattern.tool {
            awaken_tool_pattern::ToolMatcher::Exact(name) => name == id,
            awaken_tool_pattern::ToolMatcher::Glob(g) => awaken_tool_pattern::wildcard_match(g, id),
            awaken_tool_pattern::ToolMatcher::Regex(r) => r.is_match(id),
        });
        if !matched {
            orphans.push(pattern_str.to_string());
        }
    }
    orphans
}

fn tool_matcher_matches_any(
    matcher: &awaken_tool_pattern::ToolMatcher,
    tool_ids: &[String],
) -> bool {
    tool_ids.iter().any(|id| match matcher {
        awaken_tool_pattern::ToolMatcher::Exact(name) => name == id,
        awaken_tool_pattern::ToolMatcher::Glob(g) => awaken_tool_pattern::wildcard_match(g, id),
        awaken_tool_pattern::ToolMatcher::Regex(r) => r.is_match(id),
    })
}
