//! Pattern matching engine for [`ToolCallPattern`].
//!
//! Evaluates a pattern against a `(tool_id, tool_args)` pair and returns a
//! [`MatchResult`] with specificity information for priority-based rule ordering.

use crate::rules::{
    ArgMatcher, FieldCondition, MatchOp, PathSegment, ToolCallPattern, ToolMatcher,
};
use serde_json::Value;

/// Result of matching a pattern against a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchResult {
    /// The pattern does not match this tool call.
    NoMatch,
    /// The pattern matches with the given specificity.
    Match { specificity: Specificity },
}

impl MatchResult {
    #[must_use]
    pub fn is_match(&self) -> bool {
        matches!(self, Self::Match { .. })
    }
}

/// Specificity of a pattern match, used for priority ordering.
///
/// Higher values = more specific = higher priority.
/// Ordering: exact tool > glob tool > regex tool,
/// with-args > without-args, more fields > fewer fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Specificity {
    /// Tool name match precision: 3=exact, 2=glob, 1=regex.
    pub tool_kind: u8,
    /// Whether argument conditions are present.
    pub has_args: bool,
    /// Number of field conditions.
    pub field_count: u8,
    /// Sum of per-field precision (exact=3, glob=2, regex=1 per field).
    pub field_precision: u8,
}

/// Evaluate whether a pattern matches a tool call.
#[must_use]
pub fn pattern_matches(pattern: &ToolCallPattern, tool_id: &str, tool_args: &Value) -> MatchResult {
    // 1. Match tool name
    let tool_kind = match &pattern.tool {
        ToolMatcher::Exact(name) => {
            if name != tool_id {
                return MatchResult::NoMatch;
            }
            3
        }
        ToolMatcher::Glob(pat) => {
            if !wildcard_match(pat, tool_id) {
                return MatchResult::NoMatch;
            }
            2
        }
        ToolMatcher::Regex(re) => {
            if !re.is_match(tool_id) {
                return MatchResult::NoMatch;
            }
            1
        }
    };

    // 2. Match arguments
    match &pattern.args {
        ArgMatcher::Any => MatchResult::Match {
            specificity: Specificity {
                tool_kind,
                has_args: false,
                field_count: 0,
                field_precision: 0,
            },
        },
        ArgMatcher::Primary { op, value } => {
            let primary_value = infer_primary_value(tool_args);
            if evaluate_op(op, value, &primary_value) {
                MatchResult::Match {
                    specificity: Specificity {
                        tool_kind,
                        has_args: true,
                        field_count: 1,
                        field_precision: op_precision(op),
                    },
                }
            } else {
                MatchResult::NoMatch
            }
        }
        ArgMatcher::Fields(conditions) => {
            let mut field_precision = 0u8;
            for cond in conditions {
                if !evaluate_field_condition(cond, tool_args) {
                    return MatchResult::NoMatch;
                }
                field_precision = field_precision.saturating_add(op_precision(&cond.op));
            }
            MatchResult::Match {
                specificity: Specificity {
                    tool_kind,
                    has_args: true,
                    field_count: conditions.len().min(255) as u8,
                    field_precision,
                },
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Field resolution
// ---------------------------------------------------------------------------

/// Resolve a dotted path against a JSON value, returning all matching leaf values.
fn resolve_path<'a>(value: &'a Value, path: &[PathSegment]) -> Vec<&'a Value> {
    if path.is_empty() {
        return vec![value];
    }

    let (head, tail) = path.split_first().unwrap();

    match head {
        PathSegment::Field(name) => match value.get(name.as_str()) {
            Some(child) => resolve_path(child, tail),
            None => vec![],
        },
        PathSegment::Index(i) => match value.as_array().and_then(|arr| arr.get(*i)) {
            Some(child) => resolve_path(child, tail),
            None => vec![],
        },
        PathSegment::AnyIndex => match value.as_array() {
            Some(arr) => arr
                .iter()
                .flat_map(|elem| resolve_path(elem, tail))
                .collect(),
            None => vec![],
        },
        PathSegment::Wildcard => match value.as_object() {
            Some(obj) => obj.values().flat_map(|v| resolve_path(v, tail)).collect(),
            None => vec![],
        },
    }
}

/// Infer the "primary" field value from tool args.
///
/// - If args is an object with exactly one key, use that value as string.
/// - Otherwise, stringify the whole args.
fn infer_primary_value(args: &Value) -> String {
    if let Some(obj) = args.as_object() {
        if obj.len() == 1 {
            let v = obj.values().next().unwrap();
            return value_to_string(v);
        }
    }
    value_to_string(args)
}

/// Convert a JSON value to a string for matching.
fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Condition evaluation
// ---------------------------------------------------------------------------

/// Evaluate a single field condition against a JSON value.
///
/// If the field path does not resolve to any value, the condition evaluates
/// to `false` regardless of operator polarity.
fn evaluate_field_condition(cond: &FieldCondition, args: &Value) -> bool {
    let resolved = resolve_path(args, &cond.path);
    if resolved.is_empty() {
        return false;
    }
    // Existential: any resolved value matching -> condition passes.
    resolved
        .iter()
        .any(|v| evaluate_op(&cond.op, &cond.value, &value_to_string(v)))
}

/// Evaluate a match operator against a string value.
fn evaluate_op(op: &MatchOp, pattern: &str, value: &str) -> bool {
    match op {
        MatchOp::Glob => wildcard_match(pattern, value),
        MatchOp::Exact => pattern == value,
        MatchOp::Regex => regex::Regex::new(pattern)
            .map(|re| re.is_match(value))
            .unwrap_or(false),
        MatchOp::NotGlob => !wildcard_match(pattern, value),
        MatchOp::NotExact => pattern != value,
        MatchOp::NotRegex => regex::Regex::new(pattern)
            .map(|re| !re.is_match(value))
            .unwrap_or(true),
    }
}

/// Wildcard match where `*` matches any characters including `/`.
fn wildcard_match(pattern: &str, value: &str) -> bool {
    let normalized = normalize_wildcards(pattern);
    glob_match::glob_match(&normalized, value)
}

/// Convert standalone `*` to `**` so glob_match treats them as crossing `/`.
fn normalize_wildcards(pattern: &str) -> String {
    let bytes = pattern.as_bytes();
    let len = bytes.len();
    let mut result = String::with_capacity(len + 8);
    let mut i = 0;
    while i < len {
        if bytes[i] == b'*' {
            let start = i;
            while i < len && bytes[i] == b'*' {
                i += 1;
            }
            let count = i - start;
            if count >= 2 {
                for _ in 0..count {
                    result.push('*');
                }
            } else {
                let preceded_by_globstar = start >= 3 && &bytes[start - 3..start] == b"**/";
                let followed_by_globstar =
                    i + 2 < len && &bytes[i..i + 2] == b"/*" && bytes[i + 2] == b'*';

                if preceded_by_globstar || followed_by_globstar {
                    result.push('*');
                } else {
                    result.push_str("**");
                }
            }
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Schema validation
// ---------------------------------------------------------------------------

/// Validate that a pattern's field references exist in a tool's JSON Schema.
#[must_use]
pub fn validate_pattern_fields(
    pattern: &ToolCallPattern,
    parameters_schema: &Value,
) -> Vec<String> {
    let conditions = match &pattern.args {
        ArgMatcher::Any | ArgMatcher::Primary { .. } => return vec![],
        ArgMatcher::Fields(conditions) => conditions,
    };

    let mut warnings = Vec::new();
    for cond in conditions {
        if !schema_has_path(parameters_schema, &cond.path) {
            let path_str = cond
                .path
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join(".");
            warnings.push(path_str);
        }
    }
    warnings
}

/// Check if a JSON Schema has properties along the given path.
fn schema_has_path(schema: &Value, path: &[PathSegment]) -> bool {
    if path.is_empty() {
        return true;
    }

    let (head, tail) = path.split_first().unwrap();
    match head {
        PathSegment::Field(name) => {
            let prop = schema.get("properties").and_then(|p| p.get(name.as_str()));
            match prop {
                Some(sub_schema) => schema_has_path(sub_schema, tail),
                None => schema
                    .get("additionalProperties")
                    .is_some_and(|ap| ap.is_object() && schema_has_path(ap, tail)),
            }
        }
        PathSegment::Index(_) | PathSegment::AnyIndex => schema
            .get("items")
            .is_some_and(|items| schema_has_path(items, tail)),
        PathSegment::Wildcard => {
            if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
                props.values().any(|sub| schema_has_path(sub, tail))
            } else {
                schema
                    .get("additionalProperties")
                    .is_some_and(|ap| ap.is_object() && schema_has_path(ap, tail))
            }
        }
    }
}

/// Precision score for an operator (used in specificity).
fn op_precision(op: &MatchOp) -> u8 {
    match op {
        MatchOp::Exact | MatchOp::NotExact => 3,
        MatchOp::Glob | MatchOp::NotGlob => 2,
        MatchOp::Regex | MatchOp::NotRegex => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::*;
    use serde_json::json;

    fn exact(name: &str) -> ToolCallPattern {
        ToolCallPattern::tool(name)
    }

    fn primary(name: &str, pat: &str) -> ToolCallPattern {
        ToolCallPattern::tool_with_primary(name, pat)
    }

    fn field_rule(name: &str, field: &str, op: MatchOp, value: &str) -> ToolCallPattern {
        ToolCallPattern {
            tool: ToolMatcher::Exact(name.into()),
            args: ArgMatcher::Fields(vec![FieldCondition {
                path: vec![PathSegment::Field(field.into())],
                op,
                value: value.into(),
            }]),
        }
    }

    // --- Tool name matching ---

    #[test]
    fn exact_tool_matches() {
        assert!(pattern_matches(&exact("Bash"), "Bash", &json!({})).is_match());
    }

    #[test]
    fn exact_tool_no_match() {
        assert!(!pattern_matches(&exact("Bash"), "Read", &json!({})).is_match());
    }

    #[test]
    fn glob_tool_matches() {
        let p = ToolCallPattern::tool_glob("mcp__github__*");
        assert!(pattern_matches(&p, "mcp__github__create_issue", &json!({})).is_match());
        assert!(pattern_matches(&p, "mcp__github__list_repos", &json!({})).is_match());
        assert!(!pattern_matches(&p, "mcp__slack__post", &json!({})).is_match());
    }

    #[test]
    fn regex_tool_matches() {
        let p = ToolCallPattern {
            tool: ToolMatcher::Regex(regex::Regex::new(r"mcp__(github|gitlab)__.*").unwrap()),
            args: ArgMatcher::Any,
        };
        assert!(pattern_matches(&p, "mcp__github__create_issue", &json!({})).is_match());
        assert!(pattern_matches(&p, "mcp__gitlab__merge", &json!({})).is_match());
        assert!(!pattern_matches(&p, "mcp__slack__post", &json!({})).is_match());
    }

    // --- Primary arg matching ---

    #[test]
    fn primary_glob_matches() {
        let p = primary("Bash", "npm *");
        assert!(pattern_matches(&p, "Bash", &json!({"command": "npm install"})).is_match());
        assert!(!pattern_matches(&p, "Bash", &json!({"command": "git status"})).is_match());
    }

    #[test]
    fn primary_glob_multi_key_uses_stringify() {
        let p = primary("Bash", "*npm*");
        assert!(pattern_matches(&p, "Bash", &json!({"a": "npm", "b": "x"})).is_match());
        let p2 = primary("Bash", "*cargo*");
        assert!(!pattern_matches(&p2, "Bash", &json!({"a": "npm", "b": "x"})).is_match());
    }

    // --- Named field matching ---

    #[test]
    fn named_field_glob() {
        let p = field_rule("Edit", "file_path", MatchOp::Glob, "src/**/*.rs");
        assert!(pattern_matches(&p, "Edit", &json!({"file_path": "src/main.rs"})).is_match());
        assert!(pattern_matches(&p, "Edit", &json!({"file_path": "src/sub/lib.rs"})).is_match());
        assert!(!pattern_matches(&p, "Edit", &json!({"file_path": "tests/test.rs"})).is_match());
    }

    #[test]
    fn named_field_exact() {
        let p = field_rule("Bash", "command", MatchOp::Exact, "ls");
        assert!(pattern_matches(&p, "Bash", &json!({"command": "ls"})).is_match());
        assert!(!pattern_matches(&p, "Bash", &json!({"command": "ls -la"})).is_match());
    }

    #[test]
    fn named_field_regex() {
        let p = field_rule("Bash", "command", MatchOp::Regex, "(?i)eval|exec");
        assert!(pattern_matches(&p, "Bash", &json!({"command": "eval foo"})).is_match());
        assert!(pattern_matches(&p, "Bash", &json!({"command": "EXEC bar"})).is_match());
        assert!(!pattern_matches(&p, "Bash", &json!({"command": "npm install"})).is_match());
    }

    #[test]
    fn named_field_not_glob() {
        let p = field_rule("Bash", "command", MatchOp::NotGlob, "rm *");
        assert!(!pattern_matches(&p, "Bash", &json!({"command": "rm -rf /"})).is_match());
        assert!(pattern_matches(&p, "Bash", &json!({"command": "ls"})).is_match());
    }

    #[test]
    fn missing_field_positive_op_no_match() {
        let p = field_rule("Bash", "command", MatchOp::Glob, "npm *");
        assert!(!pattern_matches(&p, "Bash", &json!({})).is_match());
    }

    #[test]
    fn missing_field_negative_op_no_match() {
        let p = field_rule("Bash", "command", MatchOp::NotGlob, "rm *");
        assert!(!pattern_matches(&p, "Bash", &json!({})).is_match());
    }

    // --- Nested paths ---

    #[test]
    fn nested_path_dot_notation() {
        let p = ToolCallPattern {
            tool: ToolMatcher::Exact("Tool".into()),
            args: ArgMatcher::Fields(vec![FieldCondition {
                path: vec![
                    PathSegment::Field("config".into()),
                    PathSegment::Field("host".into()),
                ],
                op: MatchOp::Exact,
                value: "localhost".into(),
            }]),
        };
        assert!(pattern_matches(&p, "Tool", &json!({"config": {"host": "localhost"}})).is_match());
        assert!(!pattern_matches(&p, "Tool", &json!({"config": {"host": "prod"}})).is_match());
    }

    #[test]
    fn nested_path_any_index() {
        let p = ToolCallPattern {
            tool: ToolMatcher::Exact("Tool".into()),
            args: ArgMatcher::Fields(vec![FieldCondition {
                path: vec![
                    PathSegment::Field("items".into()),
                    PathSegment::AnyIndex,
                    PathSegment::Field("name".into()),
                ],
                op: MatchOp::Exact,
                value: "target".into(),
            }]),
        };
        assert!(
            pattern_matches(
                &p,
                "Tool",
                &json!({"items": [{"name": "other"}, {"name": "target"}]})
            )
            .is_match()
        );
        assert!(
            !pattern_matches(
                &p,
                "Tool",
                &json!({"items": [{"name": "a"}, {"name": "b"}]})
            )
            .is_match()
        );
    }

    #[test]
    fn nested_path_specific_index() {
        let p = ToolCallPattern {
            tool: ToolMatcher::Exact("Tool".into()),
            args: ArgMatcher::Fields(vec![FieldCondition {
                path: vec![
                    PathSegment::Field("items".into()),
                    PathSegment::Index(0),
                    PathSegment::Field("name".into()),
                ],
                op: MatchOp::Exact,
                value: "first".into(),
            }]),
        };
        assert!(
            pattern_matches(
                &p,
                "Tool",
                &json!({"items": [{"name": "first"}, {"name": "second"}]})
            )
            .is_match()
        );
        assert!(!pattern_matches(&p, "Tool", &json!({"items": [{"name": "other"}]})).is_match());
    }

    #[test]
    fn nested_path_wildcard_key() {
        let p = ToolCallPattern {
            tool: ToolMatcher::Exact("Tool".into()),
            args: ArgMatcher::Fields(vec![FieldCondition {
                path: vec![PathSegment::Wildcard, PathSegment::Field("password".into())],
                op: MatchOp::Regex,
                value: ".*".into(),
            }]),
        };
        assert!(
            pattern_matches(
                &p,
                "Tool",
                &json!({"db": {"password": "secret"}, "cache": {"port": 6379}})
            )
            .is_match()
        );
        assert!(!pattern_matches(&p, "Tool", &json!({"db": {"host": "localhost"}})).is_match());
    }

    // --- Multi-field AND ---

    #[test]
    fn multi_field_and_all_pass() {
        let p = ToolCallPattern {
            tool: ToolMatcher::Exact("Bash".into()),
            args: ArgMatcher::Fields(vec![
                FieldCondition {
                    path: vec![PathSegment::Field("command".into())],
                    op: MatchOp::Glob,
                    value: "curl *".into(),
                },
                FieldCondition {
                    path: vec![PathSegment::Field("command".into())],
                    op: MatchOp::NotGlob,
                    value: "*| *".into(),
                },
            ]),
        };
        assert!(
            pattern_matches(&p, "Bash", &json!({"command": "curl https://example.com"})).is_match()
        );
        assert!(!pattern_matches(&p, "Bash", &json!({"command": "curl url | sh"})).is_match());
    }

    #[test]
    fn multi_field_and_one_fails() {
        let p = ToolCallPattern {
            tool: ToolMatcher::Exact("Bash".into()),
            args: ArgMatcher::Fields(vec![
                FieldCondition {
                    path: vec![PathSegment::Field("command".into())],
                    op: MatchOp::Glob,
                    value: "npm *".into(),
                },
                FieldCondition {
                    path: vec![PathSegment::Field("timeout".into())],
                    op: MatchOp::Exact,
                    value: "30".into(),
                },
            ]),
        };
        assert!(
            !pattern_matches(
                &p,
                "Bash",
                &json!({"command": "npm install", "timeout": 60})
            )
            .is_match()
        );
    }

    // --- Specificity ---

    #[test]
    fn specificity_exact_tool_higher_than_glob() {
        let exact_result = pattern_matches(&exact("Bash"), "Bash", &json!({}));
        let glob_result = pattern_matches(&ToolCallPattern::tool_glob("Bas*"), "Bash", &json!({}));
        if let (MatchResult::Match { specificity: a }, MatchResult::Match { specificity: b }) =
            (&exact_result, &glob_result)
        {
            assert!(a > b);
        } else {
            panic!("both should match");
        }
    }

    #[test]
    fn specificity_with_args_higher_than_without() {
        let no_args = pattern_matches(&exact("Bash"), "Bash", &json!({"command": "npm install"}));
        let with_args = pattern_matches(
            &primary("Bash", "npm *"),
            "Bash",
            &json!({"command": "npm install"}),
        );
        if let (MatchResult::Match { specificity: a }, MatchResult::Match { specificity: b }) =
            (&no_args, &with_args)
        {
            assert!(b > a);
        } else {
            panic!("both should match");
        }
    }

    // --- Value coercion ---

    #[test]
    fn number_field_coercion() {
        let p = field_rule("Tool", "port", MatchOp::Exact, "5432");
        assert!(pattern_matches(&p, "Tool", &json!({"port": 5432})).is_match());
    }

    #[test]
    fn bool_field_coercion() {
        let p = field_rule("Tool", "readonly", MatchOp::Exact, "true");
        assert!(pattern_matches(&p, "Tool", &json!({"readonly": true})).is_match());
    }

    #[test]
    fn null_field_coercion() {
        let p = field_rule("Tool", "value", MatchOp::Exact, "");
        assert!(pattern_matches(&p, "Tool", &json!({"value": null})).is_match());
    }

    #[test]
    fn wildcard_match_crosses_slashes() {
        assert!(wildcard_match("rm *", "rm -rf /"));
        assert!(wildcard_match("curl *", "curl https://example.com"));
        assert!(wildcard_match("npm *", "npm install"));
        assert!(wildcard_match("mcp__*", "mcp__github__create"));
        assert!(wildcard_match("rm **", "rm -rf /"));
        assert!(wildcard_match("src/**/*.rs", "src/main.rs"));
        assert!(wildcard_match("src/**/*.rs", "src/sub/lib.rs"));
        assert!(!wildcard_match("src/**/*.rs", "tests/test.rs"));
    }

    #[test]
    fn empty_array_any_index_no_match() {
        let p = ToolCallPattern {
            tool: ToolMatcher::Exact("Tool".into()),
            args: ArgMatcher::Fields(vec![FieldCondition {
                path: vec![PathSegment::Field("items".into()), PathSegment::AnyIndex],
                op: MatchOp::Glob,
                value: "*".into(),
            }]),
        };
        assert!(!pattern_matches(&p, "Tool", &json!({"items": []})).is_match());
    }

    // --- Schema validation ---

    #[test]
    fn validate_pattern_fields_ok() {
        let schema = json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" }
            }
        });
        let p = field_rule("Bash", "command", MatchOp::Glob, "npm *");
        assert!(validate_pattern_fields(&p, &schema).is_empty());
    }

    #[test]
    fn validate_pattern_fields_missing() {
        let schema = json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string" }
            }
        });
        let p = field_rule("Edit", "command", MatchOp::Glob, "npm *");
        let warnings = validate_pattern_fields(&p, &schema);
        assert_eq!(warnings, vec!["command"]);
    }

    #[test]
    fn validate_pattern_fields_nested() {
        let schema = json!({
            "type": "object",
            "properties": {
                "config": {
                    "type": "object",
                    "properties": {
                        "host": { "type": "string" }
                    }
                }
            }
        });
        let p = ToolCallPattern {
            tool: ToolMatcher::Exact("Tool".into()),
            args: ArgMatcher::Fields(vec![FieldCondition {
                path: vec![
                    PathSegment::Field("config".into()),
                    PathSegment::Field("host".into()),
                ],
                op: MatchOp::Exact,
                value: "localhost".into(),
            }]),
        };
        assert!(validate_pattern_fields(&p, &schema).is_empty());

        let p_bad = ToolCallPattern {
            tool: ToolMatcher::Exact("Tool".into()),
            args: ArgMatcher::Fields(vec![FieldCondition {
                path: vec![
                    PathSegment::Field("config".into()),
                    PathSegment::Field("port".into()),
                ],
                op: MatchOp::Exact,
                value: "5432".into(),
            }]),
        };
        assert_eq!(
            validate_pattern_fields(&p_bad, &schema),
            vec!["config.port"]
        );
    }

    #[test]
    fn validate_pattern_fields_array_items() {
        let schema = json!({
            "type": "object",
            "properties": {
                "queries": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "sql": { "type": "string" }
                        }
                    }
                }
            }
        });
        let p = ToolCallPattern {
            tool: ToolMatcher::Exact("Tool".into()),
            args: ArgMatcher::Fields(vec![FieldCondition {
                path: vec![
                    PathSegment::Field("queries".into()),
                    PathSegment::AnyIndex,
                    PathSegment::Field("sql".into()),
                ],
                op: MatchOp::Regex,
                value: ".*".into(),
            }]),
        };
        assert!(validate_pattern_fields(&p, &schema).is_empty());
    }

    #[test]
    fn validate_skips_primary_and_any() {
        let schema = json!({"type": "object", "properties": {}});
        assert!(validate_pattern_fields(&exact("Bash"), &schema).is_empty());
        assert!(validate_pattern_fields(&primary("Bash", "npm *"), &schema).is_empty());
    }

    // --- Misconfigured rule safety ---

    #[test]
    fn wrong_field_deny_does_not_accidentally_match() {
        let p = field_rule("Edit", "command", MatchOp::NotGlob, "safe*");
        assert!(
            !pattern_matches(&p, "Edit", &json!({"file_path": "src/main.rs"})).is_match(),
            "misconfigured rule referencing non-existent field should not match"
        );
    }
}
