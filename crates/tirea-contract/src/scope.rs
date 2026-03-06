use serde_json::Value;
use crate::RunConfig;

/// Scope key: tool allow-list policy.
pub const SCOPE_ALLOWED_TOOLS_KEY: &str = "__agent_policy_allowed_tools";
/// Scope key: tool deny-list policy.
pub const SCOPE_EXCLUDED_TOOLS_KEY: &str = "__agent_policy_excluded_tools";
/// Scope key: skill allow-list policy.
pub const SCOPE_ALLOWED_SKILLS_KEY: &str = "__agent_policy_allowed_skills";
/// Scope key: skill deny-list policy.
pub const SCOPE_EXCLUDED_SKILLS_KEY: &str = "__agent_policy_excluded_skills";
/// Scope key: delegate-agent allow-list policy.
pub const SCOPE_ALLOWED_AGENTS_KEY: &str = "__agent_policy_allowed_agents";
/// Scope key: delegate-agent deny-list policy.
pub const SCOPE_EXCLUDED_AGENTS_KEY: &str = "__agent_policy_excluded_agents";

/// Check whether an identifier is allowed by optional allow/deny lists.
#[must_use]
pub fn is_id_allowed(id: &str, allowed: Option<&[String]>, excluded: Option<&[String]>) -> bool {
    if let Some(allowed) = allowed {
        if !allowed.iter().any(|v| v == id) {
            return false;
        }
    }
    if let Some(excluded) = excluded {
        if excluded.iter().any(|v| v == id) {
            return false;
        }
    }
    true
}

/// Parse scope filter values from a JSON array into normalized string values.
#[must_use]
pub fn parse_scope_filter(values: Option<&Value>) -> Option<Vec<String>> {
    let arr = values?.as_array()?;
    let parsed: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    Some(parsed)
}

/// Check whether an identifier is allowed by filters stored in scope keys.
#[must_use]
pub fn is_scope_allowed(
    run_config: Option<&RunConfig>,
    id: &str,
    allowed_key: &str,
    excluded_key: &str,
) -> bool {
    let allowed = parse_scope_filter(run_config.and_then(|s| s.value(allowed_key)));
    let excluded = parse_scope_filter(run_config.and_then(|s| s.value(excluded_key)));
    is_id_allowed(id, allowed.as_deref(), excluded.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_scope_filter_normalizes_strings() {
        let parsed = parse_scope_filter(Some(&json!([" a ", "", "b", 1, null]))).unwrap();
        assert_eq!(parsed, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn is_id_allowed_uses_allow_and_exclude_lists() {
        let allowed = vec!["a".to_string(), "b".to_string()];
        let excluded = vec!["b".to_string()];
        assert!(is_id_allowed("a", Some(&allowed), Some(&excluded)));
        assert!(!is_id_allowed("b", Some(&allowed), Some(&excluded)));
        assert!(!is_id_allowed("c", Some(&allowed), Some(&excluded)));
    }

    #[test]
    fn is_id_allowed_allows_when_no_filters() {
        assert!(is_id_allowed("a", None, None));
    }

    #[test]
    fn is_id_allowed_allows_when_in_allowed() {
        let allowed = vec!["a".to_string(), "b".to_string()];
        assert!(is_id_allowed("a", Some(&allowed), None));
        assert!(is_id_allowed("b", Some(&allowed), None));
        assert!(!is_id_allowed("c", Some(&allowed), None));
    }

    #[test]
    fn is_id_allowed_denies_when_excluded() {
        let excluded = vec!["b".to_string()];
        assert!(is_id_allowed("a", None, Some(&excluded)));
        assert!(!is_id_allowed("b", None, Some(&excluded)));
    }

    #[test]
    fn is_scope_allowed_reads_filters_from_scope() {
        let allowed_key = "__test_allowed";
        let excluded_key = "__test_excluded";
        let mut scope = RunConfig::new();
        scope.set(allowed_key, vec!["a", "b"]).expect("set allowed");
        scope.set(excluded_key, vec!["b"]).expect("set excluded");

        assert!(is_scope_allowed(
            Some(&scope),
            "a",
            allowed_key,
            excluded_key,
        ));
        assert!(!is_scope_allowed(
            Some(&scope),
            "b",
            allowed_key,
            excluded_key,
        ));
    }

    #[test]
    fn is_scope_allowed_with_tool_keys() {
        let mut rt = RunConfig::new();
        rt.set(SCOPE_ALLOWED_TOOLS_KEY, vec!["a", "b"]).unwrap();
        rt.set(SCOPE_EXCLUDED_TOOLS_KEY, vec!["b"]).unwrap();
        assert!(is_scope_allowed(
            Some(&rt),
            "a",
            SCOPE_ALLOWED_TOOLS_KEY,
            SCOPE_EXCLUDED_TOOLS_KEY
        ));
        assert!(!is_scope_allowed(
            Some(&rt),
            "b",
            SCOPE_ALLOWED_TOOLS_KEY,
            SCOPE_EXCLUDED_TOOLS_KEY
        ));
        assert!(!is_scope_allowed(
            Some(&rt),
            "c",
            SCOPE_ALLOWED_TOOLS_KEY,
            SCOPE_EXCLUDED_TOOLS_KEY
        ));
    }

    #[test]
    fn no_scope_filters_allows_all() {
        assert!(is_scope_allowed(
            None,
            "any_tool",
            SCOPE_ALLOWED_TOOLS_KEY,
            SCOPE_EXCLUDED_TOOLS_KEY
        ));
        let rt = RunConfig::new();
        assert!(is_scope_allowed(
            Some(&rt),
            "any_tool",
            SCOPE_ALLOWED_TOOLS_KEY,
            SCOPE_EXCLUDED_TOOLS_KEY
        ));
    }
}
