//! Catalog filtering for `AgentSpec` allow/exclude lists.
//!
//! Thin facade over [`AgentSpec::tool_allowed`] in `awaken-contract` plus
//! pipeline-local diagnostics (unmatched patterns, argument-pattern misuse,
//! orphan permission rules). The matcher itself lives in the contract crate
//! so every consumer that holds an `AgentSpec` gets the same answer
//! regardless of which crate it lives in.

use awaken_contract::registry_spec::AgentSpec;
use awaken_tool_pattern::tool_id_match;

/// Decide whether a tool id passes the agent's catalog filter.
///
/// Forwards to [`AgentSpec::tool_allowed`] — see that method for the
/// canonical semantics (literals ∪ patterns) − (excluded literals ∪
/// excluded patterns), deny wins.
#[must_use]
pub(crate) fn tool_allowed(spec: &AgentSpec, id: &str) -> bool {
    spec.tool_allowed(id)
}

/// Find pattern entries that don't match any currently registered tool id.
/// Scoped to `*_tool_patterns` fields only — literal fields may name tools
/// not yet registered (e.g. user reserves a name).
///
/// Returned tuples are `(field_name, pattern)` for stable comparison in
/// callers.
#[must_use]
pub(crate) fn unmatched_patterns(
    spec: &AgentSpec,
    registered: &[&str],
) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for (field, list) in [
        (
            "allowed_tool_patterns",
            spec.allowed_tool_patterns.as_deref(),
        ),
        (
            "excluded_tool_patterns",
            spec.excluded_tool_patterns.as_deref(),
        ),
    ] {
        if let Some(entries) = list {
            for p in entries {
                if !registered.iter().any(|id| tool_id_match(p, id)) {
                    out.push((field, p.clone()));
                }
            }
        }
    }
    out
}

/// Find catalog entries shaped like permission-rule patterns
/// (e.g. `Bash(npm)`, `mcp:weather/forecast(token=X)`).
///
/// The catalog filter operates on tool IDs only — any argument-level
/// matcher inside parentheses has no effect here and almost certainly
/// belongs in `spec.sections["permission"]`. We flag such entries so
/// users catch the misuse.
///
/// Heuristic: an entry is flagged when it contains `(`, ends with `)`,
/// and the parenthetical content is non-empty. This applies uniformly
/// to all four catalog fields (literal and pattern, allowed and
/// excluded). Tighten only if a real-world tool id needs to look like
/// `Name(stuff)` — none do today.
///
/// Returned tuples are `(field_name, entry)` for stable comparison in
/// callers.
#[must_use]
pub(crate) fn argument_pattern_misuse(spec: &AgentSpec) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for (field, list) in [
        ("allowed_tools", spec.allowed_tools.as_deref()),
        (
            "allowed_tool_patterns",
            spec.allowed_tool_patterns.as_deref(),
        ),
        ("excluded_tools", spec.excluded_tools.as_deref()),
        (
            "excluded_tool_patterns",
            spec.excluded_tool_patterns.as_deref(),
        ),
    ] {
        if let Some(entries) = list {
            for entry in entries {
                if looks_like_argument_pattern(entry) {
                    out.push((field, entry.clone()));
                }
            }
        }
    }
    out
}

/// Names of tools referenced by `sections["permission"]` rules but absent
/// from the post-catalog tool set. Returned names are bare tool ids (the
/// leading identifier of the rule's `tool` field — everything before any
/// `(` is stripped).
///
/// Empty result when there is no permission section or no rules in it.
///
/// Scope: only rules whose tool field is a **literal** name are checked.
/// Glob entries (`mcp:*`) and regex entries (`/B.*/`) are skipped — a
/// simple set-membership test on the bare prefix can't decide whether
/// they fire against the surviving catalog, and a naive check would
/// false-positive every glob rule. The permission plugin owns the
/// authoritative pattern semantics; this diagnostic only catches the
/// common typo of naming a literal tool the catalog has removed.
#[must_use]
pub(crate) fn permission_rules_without_catalog_match(
    spec: &AgentSpec,
    surviving: &[&str],
) -> Vec<String> {
    let surviving_set: std::collections::HashSet<&str> = surviving.iter().copied().collect();
    let Some(rules) = spec
        .sections
        .get("permission")
        .and_then(|p| p.get("rules"))
        .and_then(|r| r.as_array())
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for rule in rules {
        let Some(tool_field) = rule.get("tool").and_then(|t| t.as_str()) else {
            continue;
        };
        let bare = tool_field
            .split('(')
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if bare.is_empty() || !is_literal_tool_id(&bare) {
            continue;
        }
        if !surviving_set.contains(bare.as_str()) {
            out.push(bare);
        }
    }
    out.sort();
    out.dedup();
    out
}

/// True when `id` is a literal tool name (no glob metacharacters, no
/// `/.../` regex wrapping). Used to decide whether the orphan-rule
/// diagnostic can honestly answer with a set-membership test.
///
/// Glob metacharacters mirror the set the permission plugin treats as
/// glob-triggering in its rule loader (`awaken-ext-permission/src/config.rs`):
/// `*`, `?`, and `[`. Missing any of them would let the diagnostic
/// false-positive on a valid glob rule like `file_?`.
fn is_literal_tool_id(id: &str) -> bool {
    if id.contains('*') || id.contains('?') || id.contains('[') {
        return false;
    }
    if id.starts_with('/') && id.ends_with('/') && id.len() >= 2 {
        return false;
    }
    true
}

/// True when `entry` looks like `name(args)` with non-empty args.
fn looks_like_argument_pattern(entry: &str) -> bool {
    let Some(open) = entry.find('(') else {
        return false;
    };
    if !entry.ends_with(')') {
        return false;
    }
    // Content strictly between the first '(' and the trailing ')'.
    let inner = &entry[open + 1..entry.len() - 1];
    !inner.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_spec() -> AgentSpec {
        // Use deserialize migration to get the legacy default
        // (allowed_tool_patterns=["*"]); ad-hoc construction would
        // require all 17 fields explicitly.
        serde_json::from_str(r#"{"id":"a","model_id":"m","system_prompt":""}"#).unwrap()
    }

    // tool_allowed semantics tests live with the canonical impl in
    // `awaken-contract/src/registry_spec/catalog_match.rs`. The diagnostics
    // below (unmatched / misuse / orphan-rule) stay here because they're
    // pipeline-local.

    #[test]
    fn unmatched_patterns_lists_dead_glob_entries() {
        let mut spec = empty_spec();
        spec.allowed_tool_patterns = Some(vec!["mcp:*".into(), "old-*".into()]);
        spec.excluded_tool_patterns = Some(vec!["never-*".into()]);
        let registered = ["mcp:weather", "Bash"];
        let out = unmatched_patterns(&spec, &registered);
        let mut got: Vec<_> = out
            .iter()
            .map(|(field, pat)| (*field, pat.clone()))
            .collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("allowed_tool_patterns", "old-*".into()),
                ("excluded_tool_patterns", "never-*".into()),
            ]
        );
    }

    #[test]
    fn unmatched_patterns_ignores_literal_fields() {
        let mut spec = empty_spec();
        spec.allowed_tools = Some(vec!["nonexistent".into()]);
        spec.allowed_tool_patterns = Some(vec![]);
        let registered = ["Bash"];
        assert!(
            unmatched_patterns(&spec, &registered).is_empty(),
            "literal-only unmatched entries are intentional and not reported"
        );
    }

    #[test]
    fn argument_pattern_misuse_flags_paren_entries() {
        let mut spec = empty_spec();
        spec.allowed_tools = Some(vec!["Bash".into(), "Bash(npm)".into()]);
        spec.allowed_tool_patterns = Some(vec!["mcp:weather/forecast(token=X)".into()]);
        let out = argument_pattern_misuse(&spec);
        let mut got: Vec<_> = out.iter().map(|(f, e)| (*f, e.clone())).collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                (
                    "allowed_tool_patterns",
                    "mcp:weather/forecast(token=X)".into()
                ),
                ("allowed_tools", "Bash(npm)".into()),
            ]
        );
    }

    #[test]
    fn argument_pattern_misuse_ignores_well_formed_entries() {
        let mut spec = empty_spec();
        spec.allowed_tools = Some(vec!["Bash".into(), "Read".into()]);
        spec.allowed_tool_patterns = Some(vec!["mcp:*".into()]);
        spec.excluded_tool_patterns = Some(vec!["dangerous-*".into()]);
        let out = argument_pattern_misuse(&spec);
        assert!(out.is_empty(), "no entries should be flagged: {out:?}");
    }

    #[test]
    fn argument_pattern_misuse_ignores_paren_edge_cases() {
        // Pin the heuristic boundary: only `name(non-empty)` should be
        // flagged. Empty-interior `Name()`, unbalanced `Name(` / `Name)`,
        // and trailing-text `foo(x)bar` are all real tool-id-shaped
        // strings (or typos that aren't permission-rule shaped), so they
        // must NOT trigger the "this belongs in sections[\"permission\"]"
        // warning.
        let mut spec = empty_spec();
        spec.allowed_tools = Some(vec![
            "Bash()".into(),
            "Bash(".into(),
            "Bash)".into(),
            "foo(x)bar".into(),
        ]);
        spec.allowed_tool_patterns = Some(vec![]);
        let out = argument_pattern_misuse(&spec);
        assert!(
            out.is_empty(),
            "paren edge cases should not be flagged: {out:?}"
        );
    }

    #[test]
    fn permission_rule_for_removed_tool_is_flagged() {
        use serde_json::json;
        let mut spec = empty_spec();
        spec.allowed_tools = Some(vec!["Read".into()]);
        spec.allowed_tool_patterns = Some(vec![]);
        spec.sections.insert(
            "permission".into(),
            json!({
                "rules": [
                    { "tool": "Bash(npm)", "action": "deny" },
                    { "tool": "Read",      "action": "allow" }
                ]
            }),
        );
        let surviving = ["Read"];
        let out = permission_rules_without_catalog_match(&spec, &surviving);
        assert_eq!(
            out,
            vec!["Bash".to_string()],
            "Bash isn't in the post-catalog tool set, so the rule referencing it is stale"
        );
    }

    #[test]
    fn permission_rule_for_kept_tool_is_not_flagged() {
        use serde_json::json;
        let mut spec = empty_spec();
        spec.sections.insert(
            "permission".into(),
            json!({
                "rules": [
                    { "tool": "Read", "action": "allow" }
                ]
            }),
        );
        let surviving = ["Read", "Bash"];
        let out = permission_rules_without_catalog_match(&spec, &surviving);
        assert!(out.is_empty());
    }

    #[test]
    fn no_permission_section_yields_no_warnings() {
        let spec = empty_spec();
        let surviving = ["Bash"];
        assert!(permission_rules_without_catalog_match(&spec, &surviving).is_empty());
    }

    #[test]
    fn permission_glob_rule_is_not_falsely_flagged_as_orphan() {
        use serde_json::json;
        // A glob rule like `mcp:*` is a real permission feature handled by
        // the permission plugin's tool matcher. The catalog diagnostic
        // can't decide whether the glob bites a surviving tool, so it must
        // skip such rules rather than flag every one as orphan.
        //
        // Covers every glob metacharacter the permission loader recognises
        // (`*`, `?`, `[`) plus the regex `/.../` form, so an orphan-rule
        // warning won't fire for any rule the permission plugin will
        // actually evaluate as a pattern.
        let mut spec = empty_spec();
        spec.sections.insert(
            "permission".into(),
            json!({
                "rules": [
                    { "tool": "mcp:*",     "action": "deny" },
                    { "tool": "/B.*/",     "action": "ask"  },
                    { "tool": "file_?",    "action": "deny" },
                    { "tool": "read[12]",  "action": "ask"  }
                ]
            }),
        );
        let surviving = ["mcp:weather", "Bash"];
        let out = permission_rules_without_catalog_match(&spec, &surviving);
        assert!(
            out.is_empty(),
            "glob/regex rules must not be flagged by the literal-orphan diagnostic, got {out:?}"
        );
    }
}
