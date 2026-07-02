//! Type definitions for tool call pattern matching.

use std::fmt;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Path segments for nested field access
// ---------------------------------------------------------------------------

/// A single segment in a dotted field path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSegment {
    /// Named object key.
    Field(String),
    /// Specific array index: `[0]`, `[3]`.
    Index(usize),
    /// Any array element: `[*]`.
    AnyIndex,
    /// Any object key: `*` as a path segment (wildcard).
    Wildcard,
}

impl fmt::Display for PathSegment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Field(name) => write!(f, "{name}"),
            Self::Index(i) => write!(f, "[{i}]"),
            Self::AnyIndex => write!(f, "[*]"),
            Self::Wildcard => write!(f, "*"),
        }
    }
}

// ---------------------------------------------------------------------------
// Match operators
// ---------------------------------------------------------------------------

/// Comparison operator for field conditions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchOp {
    /// `~` — glob pattern match.
    #[serde(rename = "glob")]
    Glob,
    /// `=` — exact string equality.
    #[serde(rename = "exact")]
    Exact,
    /// `=~` — regex match.
    #[serde(rename = "regex")]
    Regex,
    /// `!~` — negated glob.
    #[serde(rename = "not_glob")]
    NotGlob,
    /// `!=` — not equal.
    #[serde(rename = "not_exact")]
    NotExact,
    /// `!=~` — negated regex.
    #[serde(rename = "not_regex")]
    NotRegex,
}

impl fmt::Display for MatchOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Glob => write!(f, "~"),
            Self::Exact => write!(f, "="),
            Self::Regex => write!(f, "=~"),
            Self::NotGlob => write!(f, "!~"),
            Self::NotExact => write!(f, "!="),
            Self::NotRegex => write!(f, "!=~"),
        }
    }
}

// ---------------------------------------------------------------------------
// Field condition
// ---------------------------------------------------------------------------

/// A single field-level predicate: `path op "value"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldCondition {
    /// Dotted path to the JSON field.
    pub path: Vec<PathSegment>,
    /// Comparison operator.
    pub op: MatchOp,
    /// Pattern or literal value to compare against.
    pub value: String,
}

impl fmt::Display for FieldCondition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, seg) in self.path.iter().enumerate() {
            match seg {
                PathSegment::Index(_) | PathSegment::AnyIndex => {
                    write!(f, "{seg}")?;
                }
                _ => {
                    if i > 0 {
                        write!(f, ".")?;
                    }
                    write!(f, "{seg}")?;
                }
            }
        }
        write!(f, " {} \"{}\"", self.op, self.value)
    }
}

// ---------------------------------------------------------------------------
// Tool matcher
// ---------------------------------------------------------------------------

/// How to match the tool name portion of a call.
#[derive(Debug, Clone)]
pub enum ToolMatcher {
    /// Exact string equality.
    Exact(String),
    /// Glob pattern (supports `*`, `?`, `[...]`).
    Glob(String),
    /// Compiled regex.
    Regex(regex::Regex),
}

impl PartialEq for ToolMatcher {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Exact(a), Self::Exact(b)) => a == b,
            (Self::Glob(a), Self::Glob(b)) => a == b,
            (Self::Regex(a), Self::Regex(b)) => a.as_str() == b.as_str(),
            _ => false,
        }
    }
}

impl Eq for ToolMatcher {}

impl fmt::Display for ToolMatcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(s) | Self::Glob(s) => write!(f, "{s}"),
            Self::Regex(re) => write!(f, "/{}/", re.as_str()),
        }
    }
}

// ---------------------------------------------------------------------------
// Argument matcher
// ---------------------------------------------------------------------------

/// How to match the arguments portion of a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgMatcher {
    /// `(*)` or omitted — matches any arguments.
    Any,
    /// Positional shorthand: `(npm *)` — implicit glob on primary field.
    Primary { op: MatchOp, value: String },
    /// One or more named field conditions (AND semantics).
    Fields(Vec<FieldCondition>),
}

impl fmt::Display for ArgMatcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Any => write!(f, "*"),
            Self::Primary { op, value } => match op {
                MatchOp::Glob => write!(f, "{value}"),
                _ => write!(f, "{op} \"{value}\""),
            },
            Self::Fields(conditions) => {
                for (i, cond) in conditions.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{cond}")?;
                }
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ToolCallPattern
// ---------------------------------------------------------------------------

/// A complete pattern matching tool calls by name and optionally by arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallPattern {
    /// Tool name matcher (exact, glob, or regex).
    pub tool: ToolMatcher,
    /// Argument matcher (any, primary, or named fields).
    pub args: ArgMatcher,
}

impl fmt::Display for ToolCallPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.tool)?;
        match &self.args {
            ArgMatcher::Any => Ok(()),
            other => write!(f, "({other})"),
        }
    }
}

impl ToolCallPattern {
    /// Exact tool name, any args.
    ///
    /// # Examples
    ///
    /// ```
    /// use awaken_tool_pattern::ToolCallPattern;
    ///
    /// let pattern = ToolCallPattern::tool("read_file");
    /// assert_eq!(format!("{}", pattern), "read_file");
    /// ```
    #[must_use]
    pub fn tool(name: impl Into<String>) -> Self {
        Self {
            tool: ToolMatcher::Exact(name.into()),
            args: ArgMatcher::Any,
        }
    }

    /// Exact tool name with a primary glob pattern.
    #[must_use]
    pub fn tool_with_primary(name: impl Into<String>, pattern: impl Into<String>) -> Self {
        Self {
            tool: ToolMatcher::Exact(name.into()),
            args: ArgMatcher::Primary {
                op: MatchOp::Glob,
                value: pattern.into(),
            },
        }
    }

    /// Glob tool name, any args.
    #[must_use]
    pub fn tool_glob(pattern: impl Into<String>) -> Self {
        Self {
            tool: ToolMatcher::Glob(pattern.into()),
            args: ArgMatcher::Any,
        }
    }

    /// Set argument matcher.
    #[must_use]
    pub fn with_args(mut self, args: ArgMatcher) -> Self {
        self.args = args;
        self
    }
}

// ---------------------------------------------------------------------------
// Match result and specificity
// ---------------------------------------------------------------------------

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

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // PathSegment Display
    // -----------------------------------------------------------------------

    #[test]
    fn path_segment_display_field() {
        assert_eq!(PathSegment::Field("name".into()).to_string(), "name");
    }

    #[test]
    fn path_segment_display_index() {
        assert_eq!(PathSegment::Index(3).to_string(), "[3]");
    }

    #[test]
    fn path_segment_display_any_index() {
        assert_eq!(PathSegment::AnyIndex.to_string(), "[*]");
    }

    #[test]
    fn path_segment_display_wildcard() {
        assert_eq!(PathSegment::Wildcard.to_string(), "*");
    }

    // -----------------------------------------------------------------------
    // MatchOp Display
    // -----------------------------------------------------------------------

    #[test]
    fn match_op_display() {
        assert_eq!(MatchOp::Glob.to_string(), "~");
        assert_eq!(MatchOp::Exact.to_string(), "=");
        assert_eq!(MatchOp::Regex.to_string(), "=~");
        assert_eq!(MatchOp::NotGlob.to_string(), "!~");
        assert_eq!(MatchOp::NotExact.to_string(), "!=");
        assert_eq!(MatchOp::NotRegex.to_string(), "!=~");
    }

    // -----------------------------------------------------------------------
    // FieldCondition Display
    // -----------------------------------------------------------------------

    #[test]
    fn field_condition_display_simple() {
        let cond = FieldCondition {
            path: vec![PathSegment::Field("command".into())],
            op: MatchOp::Glob,
            value: "npm *".into(),
        };
        assert_eq!(cond.to_string(), "command ~ \"npm *\"");
    }

    #[test]
    fn field_condition_display_nested_with_index() {
        let cond = FieldCondition {
            path: vec![
                PathSegment::Field("items".into()),
                PathSegment::Index(0),
                PathSegment::Field("name".into()),
            ],
            op: MatchOp::Exact,
            value: "foo".into(),
        };
        assert_eq!(cond.to_string(), "items[0].name = \"foo\"");
    }

    #[test]
    fn field_condition_display_any_index() {
        let cond = FieldCondition {
            path: vec![
                PathSegment::Field("arr".into()),
                PathSegment::AnyIndex,
                PathSegment::Field("val".into()),
            ],
            op: MatchOp::Regex,
            value: ".*test.*".into(),
        };
        assert_eq!(cond.to_string(), "arr[*].val =~ \".*test.*\"");
    }

    #[test]
    fn field_condition_display_wildcard_path() {
        let cond = FieldCondition {
            path: vec![PathSegment::Wildcard, PathSegment::Field("id".into())],
            op: MatchOp::NotExact,
            value: "secret".into(),
        };
        assert_eq!(cond.to_string(), "*.id != \"secret\"");
    }

    #[test]
    fn field_condition_display_not_glob() {
        let cond = FieldCondition {
            path: vec![PathSegment::Field("cmd".into())],
            op: MatchOp::NotGlob,
            value: "rm *".into(),
        };
        assert_eq!(cond.to_string(), "cmd !~ \"rm *\"");
    }

    #[test]
    fn field_condition_display_not_regex() {
        let cond = FieldCondition {
            path: vec![PathSegment::Field("cmd".into())],
            op: MatchOp::NotRegex,
            value: "^evil".into(),
        };
        assert_eq!(cond.to_string(), "cmd !=~ \"^evil\"");
    }

    // -----------------------------------------------------------------------
    // ToolMatcher Display and PartialEq
    // -----------------------------------------------------------------------

    #[test]
    fn tool_matcher_display_exact() {
        assert_eq!(ToolMatcher::Exact("Bash".into()).to_string(), "Bash");
    }

    #[test]
    fn tool_matcher_display_glob() {
        assert_eq!(ToolMatcher::Glob("mcp__*".into()).to_string(), "mcp__*");
    }

    #[test]
    fn tool_matcher_display_regex() {
        let re = regex::Regex::new(r"foo|bar").unwrap();
        assert_eq!(ToolMatcher::Regex(re).to_string(), "/foo|bar/");
    }

    #[test]
    fn tool_matcher_eq_exact() {
        assert_eq!(
            ToolMatcher::Exact("A".into()),
            ToolMatcher::Exact("A".into())
        );
        assert_ne!(
            ToolMatcher::Exact("A".into()),
            ToolMatcher::Exact("B".into())
        );
    }

    #[test]
    fn tool_matcher_eq_glob() {
        assert_eq!(ToolMatcher::Glob("*".into()), ToolMatcher::Glob("*".into()));
        assert_ne!(
            ToolMatcher::Glob("a*".into()),
            ToolMatcher::Glob("b*".into())
        );
    }

    #[test]
    fn tool_matcher_eq_regex() {
        let r1 = regex::Regex::new("abc").unwrap();
        let r2 = regex::Regex::new("abc").unwrap();
        let r3 = regex::Regex::new("def").unwrap();
        assert_eq!(ToolMatcher::Regex(r1), ToolMatcher::Regex(r2));
        assert_ne!(
            ToolMatcher::Regex(r3),
            ToolMatcher::Regex(regex::Regex::new("abc").unwrap())
        );
    }

    #[test]
    fn tool_matcher_eq_cross_variant() {
        assert_ne!(
            ToolMatcher::Exact("foo".into()),
            ToolMatcher::Glob("foo".into())
        );
        assert_ne!(
            ToolMatcher::Glob("foo".into()),
            ToolMatcher::Regex(regex::Regex::new("foo").unwrap())
        );
        assert_ne!(
            ToolMatcher::Exact("foo".into()),
            ToolMatcher::Regex(regex::Regex::new("foo").unwrap())
        );
    }

    // -----------------------------------------------------------------------
    // ArgMatcher Display
    // -----------------------------------------------------------------------

    #[test]
    fn arg_matcher_display_any() {
        assert_eq!(ArgMatcher::Any.to_string(), "*");
    }

    #[test]
    fn arg_matcher_display_primary_glob() {
        let m = ArgMatcher::Primary {
            op: MatchOp::Glob,
            value: "npm *".into(),
        };
        assert_eq!(m.to_string(), "npm *");
    }

    #[test]
    fn arg_matcher_display_primary_non_glob() {
        let m = ArgMatcher::Primary {
            op: MatchOp::Exact,
            value: "ls".into(),
        };
        assert_eq!(m.to_string(), "= \"ls\"");
    }

    #[test]
    fn arg_matcher_display_primary_regex() {
        let m = ArgMatcher::Primary {
            op: MatchOp::Regex,
            value: "^npm".into(),
        };
        assert_eq!(m.to_string(), "=~ \"^npm\"");
    }

    #[test]
    fn arg_matcher_display_fields_single() {
        let m = ArgMatcher::Fields(vec![FieldCondition {
            path: vec![PathSegment::Field("cmd".into())],
            op: MatchOp::Glob,
            value: "npm *".into(),
        }]);
        assert_eq!(m.to_string(), "cmd ~ \"npm *\"");
    }

    #[test]
    fn arg_matcher_display_fields_multiple() {
        let m = ArgMatcher::Fields(vec![
            FieldCondition {
                path: vec![PathSegment::Field("f1".into())],
                op: MatchOp::Glob,
                value: "a*".into(),
            },
            FieldCondition {
                path: vec![PathSegment::Field("f2".into())],
                op: MatchOp::Exact,
                value: "b".into(),
            },
        ]);
        assert_eq!(m.to_string(), "f1 ~ \"a*\", f2 = \"b\"");
    }

    // -----------------------------------------------------------------------
    // ToolCallPattern Display and builders
    // -----------------------------------------------------------------------

    #[test]
    fn pattern_display_no_args() {
        let p = ToolCallPattern::tool("Bash");
        assert_eq!(p.to_string(), "Bash");
    }

    #[test]
    fn pattern_display_with_primary() {
        let p = ToolCallPattern::tool_with_primary("Bash", "npm *");
        assert_eq!(p.to_string(), "Bash(npm *)");
    }

    #[test]
    fn pattern_display_with_fields() {
        let p = ToolCallPattern {
            tool: ToolMatcher::Exact("Edit".into()),
            args: ArgMatcher::Fields(vec![FieldCondition {
                path: vec![PathSegment::Field("file_path".into())],
                op: MatchOp::Glob,
                value: "src/**".into(),
            }]),
        };
        assert_eq!(p.to_string(), "Edit(file_path ~ \"src/**\")");
    }

    #[test]
    fn pattern_display_regex_tool() {
        let p = ToolCallPattern {
            tool: ToolMatcher::Regex(regex::Regex::new(r"mcp__.*").unwrap()),
            args: ArgMatcher::Any,
        };
        assert_eq!(p.to_string(), "/mcp__.*/");
    }

    #[test]
    fn tool_glob_builder() {
        let p = ToolCallPattern::tool_glob("mcp__*");
        assert_eq!(p.tool, ToolMatcher::Glob("mcp__*".into()));
        assert_eq!(p.args, ArgMatcher::Any);
    }

    #[test]
    fn with_args_builder() {
        let p = ToolCallPattern::tool("Bash").with_args(ArgMatcher::Primary {
            op: MatchOp::Glob,
            value: "npm *".into(),
        });
        assert_eq!(
            p.args,
            ArgMatcher::Primary {
                op: MatchOp::Glob,
                value: "npm *".into()
            }
        );
    }

    #[test]
    fn with_args_replaces_previous() {
        let p = ToolCallPattern::tool_with_primary("Bash", "npm *").with_args(ArgMatcher::Any);
        assert_eq!(p.args, ArgMatcher::Any);
    }

    // -----------------------------------------------------------------------
    // MatchResult
    // -----------------------------------------------------------------------

    #[test]
    fn match_result_no_match() {
        assert!(!MatchResult::NoMatch.is_match());
    }

    #[test]
    fn match_result_match() {
        let r = MatchResult::Match {
            specificity: Specificity {
                tool_kind: 3,
                has_args: false,
                field_count: 0,
                field_precision: 0,
            },
        };
        assert!(r.is_match());
    }

    // -----------------------------------------------------------------------
    // Specificity ordering
    // -----------------------------------------------------------------------

    #[test]
    fn specificity_ordering() {
        let low = Specificity {
            tool_kind: 1,
            has_args: false,
            field_count: 0,
            field_precision: 0,
        };
        let mid = Specificity {
            tool_kind: 2,
            has_args: false,
            field_count: 0,
            field_precision: 0,
        };
        let high = Specificity {
            tool_kind: 3,
            has_args: true,
            field_count: 2,
            field_precision: 6,
        };
        assert!(low < mid);
        assert!(mid < high);
        assert!(low < high);
    }

    #[test]
    fn specificity_has_args_higher() {
        let without = Specificity {
            tool_kind: 3,
            has_args: false,
            field_count: 0,
            field_precision: 0,
        };
        let with = Specificity {
            tool_kind: 3,
            has_args: true,
            field_count: 1,
            field_precision: 2,
        };
        assert!(with > without);
    }
}
