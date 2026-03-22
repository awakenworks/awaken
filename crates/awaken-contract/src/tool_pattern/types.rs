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
