//! Permission rules: patterns, matchers, subjects, and rulesets.
//!
//! A [`ToolCallPattern`] matches tool calls by name (glob/regex/exact) and
//! optionally by argument-level conditions on JSON fields.
//!
//! Syntax overview:
//! ```text
//! Bash                            exact tool, any args
//! Bash(*)                         explicit any args
//! Bash(npm *)                     primary arg glob
//! Edit(file_path ~ "src/**")      named field glob
//! Bash(command =~ "(?i)rm")       named field regex
//! mcp__github__*                  glob tool name
//! /mcp__(gh|gl)__.*/              regex tool name
//! Tool(a.b[*].c ~ "pat")         nested field path
//! Tool(f1 ~ "a", f2 = "b")       multi-field AND
//! ```

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::matcher::{self, MatchResult, Specificity};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchOp {
    /// `~` — glob pattern match.
    Glob,
    /// `=` — exact string equality.
    Exact,
    /// `=~` — regex match.
    Regex,
    /// `!~` — negated glob.
    NotGlob,
    /// `!=` — not equal.
    NotExact,
    /// `!=~` — negated regex.
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

impl serde::Serialize for ToolCallPattern {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for ToolCallPattern {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct PatternVisitor;

        impl<'de> serde::de::Visitor<'de> for PatternVisitor {
            type Value = ToolCallPattern;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a tool call pattern string like \"Bash(npm *)\"")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                parse_pattern(v).map_err(serde::de::Error::custom)
            }
        }

        deserializer.deserialize_str(PatternVisitor)
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
// Tool permission behavior
// ---------------------------------------------------------------------------

/// Tool permission behavior.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ToolPermissionBehavior {
    Allow,
    #[default]
    Ask,
    Deny,
}

// ---------------------------------------------------------------------------
// Permission subject
// ---------------------------------------------------------------------------

/// Permission rule subject.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PermissionSubject {
    /// Legacy: exact tool ID match.
    Tool { tool_id: String },
    /// Pattern-based match.
    Pattern { pattern: ToolCallPattern },
}

impl PermissionSubject {
    #[must_use]
    pub fn tool(tool_id: impl Into<String>) -> Self {
        Self::Tool {
            tool_id: tool_id.into(),
        }
    }

    #[must_use]
    pub fn pattern(pattern: ToolCallPattern) -> Self {
        Self::Pattern { pattern }
    }

    #[must_use]
    pub fn key(&self) -> String {
        match self {
            Self::Tool { tool_id } => format!("tool:{tool_id}"),
            Self::Pattern { pattern } => format!("pattern:{pattern}"),
        }
    }

    #[must_use]
    pub fn matches_tool(&self, tool_id: &str) -> bool {
        match self {
            Self::Tool { tool_id: id } => id == tool_id,
            Self::Pattern { pattern } => {
                matcher::pattern_matches(pattern, tool_id, &Value::Null).is_match()
            }
        }
    }

    /// Match against a tool call with arguments. Returns specificity if matched.
    #[must_use]
    pub fn matches_tool_call(&self, tool_id: &str, tool_args: &Value) -> Option<Specificity> {
        match self {
            Self::Tool { tool_id: id } => {
                if id == tool_id {
                    Some(Specificity {
                        tool_kind: 3,
                        has_args: false,
                        field_count: 0,
                        field_precision: 0,
                    })
                } else {
                    None
                }
            }
            Self::Pattern { pattern } => {
                match matcher::pattern_matches(pattern, tool_id, tool_args) {
                    MatchResult::Match { specificity } => Some(specificity),
                    MatchResult::NoMatch => None,
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Permission rule metadata
// ---------------------------------------------------------------------------

/// Lifetime of a permission rule.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PermissionRuleScope {
    Once,
    Session,
    #[default]
    Thread,
    Project,
    User,
}

/// Origin of a permission rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionRuleSource {
    System,
    Definition,
    Skill,
    Session,
    User,
    Cli,
    #[default]
    Runtime,
}

// ---------------------------------------------------------------------------
// PermissionRule
// ---------------------------------------------------------------------------

/// Declarative permission rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRule {
    pub subject: PermissionSubject,
    pub behavior: ToolPermissionBehavior,
    #[serde(default)]
    pub scope: PermissionRuleScope,
    #[serde(default)]
    pub source: PermissionRuleSource,
}

impl PermissionRule {
    #[must_use]
    pub fn new_tool(tool_id: impl Into<String>, behavior: ToolPermissionBehavior) -> Self {
        Self {
            subject: PermissionSubject::tool(tool_id),
            behavior,
            scope: PermissionRuleScope::Thread,
            source: PermissionRuleSource::Runtime,
        }
    }

    /// Create a pattern-based rule from a parsed [`ToolCallPattern`].
    #[must_use]
    pub fn new_pattern(pattern: ToolCallPattern, behavior: ToolPermissionBehavior) -> Self {
        Self {
            subject: PermissionSubject::pattern(pattern),
            behavior,
            scope: PermissionRuleScope::Thread,
            source: PermissionRuleSource::Runtime,
        }
    }

    #[must_use]
    pub fn with_scope(mut self, scope: PermissionRuleScope) -> Self {
        self.scope = scope;
        self
    }

    #[must_use]
    pub fn with_source(mut self, source: PermissionRuleSource) -> Self {
        self.source = source;
        self
    }
}

// ---------------------------------------------------------------------------
// PermissionRuleset
// ---------------------------------------------------------------------------

/// Resolved rule set fed into permission evaluation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionRuleset {
    pub default_behavior: ToolPermissionBehavior,
    pub rules: HashMap<String, PermissionRule>,
}

impl PermissionRuleset {
    /// Find a matching rule by tool ID only (legacy path).
    #[must_use]
    pub fn rule_for_tool(&self, tool_id: &str) -> Option<&PermissionRule> {
        self.rules
            .values()
            .find(|rule| rule.subject.matches_tool(tool_id))
    }

    /// Find the highest-priority matching rule considering tool args.
    ///
    /// Evaluation order (like firewall rules):
    /// 1. **Deny** — if any deny rule matches, deny immediately.
    /// 2. **Allow** — if any allow rule matches, allow.
    /// 3. **Ask** — remaining unmatched calls fall to ask.
    ///
    /// Within the same behavior tier, higher specificity wins.
    #[must_use]
    pub fn rule_for_tool_call(&self, tool_id: &str, tool_args: &Value) -> Option<&PermissionRule> {
        self.rules
            .values()
            .filter_map(|rule| {
                rule.subject
                    .matches_tool_call(tool_id, tool_args)
                    .map(|specificity| (rule, specificity))
            })
            .max_by(|(a, a_spec), (b, b_spec)| {
                let a_priority = behavior_priority(a.behavior);
                let b_priority = behavior_priority(b.behavior);
                a_priority.cmp(&b_priority).then_with(|| a_spec.cmp(b_spec))
            })
            .map(|(rule, _)| rule)
    }

    /// Collect tool IDs that are unconditionally denied (no arg conditions).
    #[must_use]
    pub fn unconditionally_denied_tools(&self) -> Vec<&str> {
        self.rules
            .values()
            .filter(|rule| rule.behavior == ToolPermissionBehavior::Deny)
            .filter_map(|rule| match &rule.subject {
                PermissionSubject::Tool { tool_id } => Some(tool_id.as_str()),
                PermissionSubject::Pattern { pattern }
                    if matches!(&pattern.tool, ToolMatcher::Exact(_))
                        && matches!(&pattern.args, ArgMatcher::Any) =>
                {
                    if let ToolMatcher::Exact(name) = &pattern.tool {
                        Some(name.as_str())
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .collect()
    }
}

/// Evaluation priority: Deny > Allow > Ask.
fn behavior_priority(behavior: ToolPermissionBehavior) -> u8 {
    match behavior {
        ToolPermissionBehavior::Deny => 2,
        ToolPermissionBehavior::Allow => 1,
        ToolPermissionBehavior::Ask => 0,
    }
}

/// Strategy evaluation output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionEvaluation {
    pub subject: PermissionSubject,
    pub behavior: ToolPermissionBehavior,
    pub matched_rule: Option<PermissionRule>,
}

/// Evaluate permission rules for a tool call with arguments.
#[must_use]
pub fn evaluate_tool_permission(
    ruleset: &PermissionRuleset,
    tool_id: &str,
    tool_args: &Value,
) -> PermissionEvaluation {
    let subject = PermissionSubject::tool(tool_id);
    let matched_rule = ruleset.rule_for_tool_call(tool_id, tool_args).cloned();
    let behavior = matched_rule
        .as_ref()
        .map_or(ruleset.default_behavior, |rule| rule.behavior);

    PermissionEvaluation {
        subject,
        behavior,
        matched_rule,
    }
}

// ---------------------------------------------------------------------------
// Pattern parser
// ---------------------------------------------------------------------------

/// Error returned when a pattern string cannot be parsed.
#[derive(Debug, Clone)]
pub struct PatternParseError {
    pub message: String,
    pub position: usize,
}

impl fmt::Display for PatternParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "parse error at {}: {}", self.position, self.message)
    }
}

impl std::error::Error for PatternParseError {}

struct Cursor<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn remaining(&self) -> &'a str {
        &self.input[self.pos..]
    }

    fn is_empty(&self) -> bool {
        self.pos >= self.input.len()
    }

    fn peek(&self) -> Option<char> {
        self.remaining().chars().next()
    }

    fn advance(&mut self, n: usize) {
        self.pos += n;
    }

    fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_ascii_whitespace() {
                self.advance(c.len_utf8());
            } else {
                break;
            }
        }
    }

    fn expect(&mut self, ch: char) -> Result<(), PatternParseError> {
        self.skip_whitespace();
        match self.peek() {
            Some(c) if c == ch => {
                self.advance(c.len_utf8());
                Ok(())
            }
            other => Err(self.error(format!(
                "expected '{}', found {}",
                ch,
                match other {
                    Some(c) => format!("'{c}'"),
                    None => "end of input".to_string(),
                }
            ))),
        }
    }

    fn error(&self, message: impl Into<String>) -> PatternParseError {
        PatternParseError {
            message: message.into(),
            position: self.pos,
        }
    }
}

/// Parse a pattern string into a [`ToolCallPattern`].
pub fn parse_pattern(input: &str) -> Result<ToolCallPattern, PatternParseError> {
    let mut cursor = Cursor::new(input.trim());

    let tool = parse_tool_part(&mut cursor)?;
    cursor.skip_whitespace();

    let args = if cursor.peek() == Some('(') {
        cursor.advance(1);
        let args = parse_arg_part(&mut cursor)?;
        cursor.expect(')')?;
        args
    } else {
        ArgMatcher::Any
    };

    cursor.skip_whitespace();
    if !cursor.is_empty() {
        return Err(cursor.error(format!("unexpected trailing: '{}'", cursor.remaining())));
    }

    Ok(ToolCallPattern { tool, args })
}

fn parse_tool_part(cursor: &mut Cursor<'_>) -> Result<ToolMatcher, PatternParseError> {
    cursor.skip_whitespace();
    if cursor.peek() == Some('/') {
        cursor.advance(1);
        let start = cursor.pos;
        let mut depth = 0u32;
        while let Some(c) = cursor.peek() {
            match c {
                '\\' => {
                    cursor.advance(1);
                    if cursor.peek().is_some() {
                        cursor.advance(1);
                    }
                }
                '(' => {
                    depth += 1;
                    cursor.advance(1);
                }
                ')' => {
                    depth = depth.saturating_sub(1);
                    cursor.advance(1);
                }
                '/' if depth == 0 => break,
                _ => cursor.advance(c.len_utf8()),
            }
        }
        let body = &cursor.input[start..cursor.pos];
        if body.is_empty() {
            return Err(cursor.error("empty regex pattern"));
        }
        cursor.expect('/')?;
        let re =
            regex::Regex::new(body).map_err(|e| cursor.error(format!("invalid regex: {e}")))?;
        Ok(ToolMatcher::Regex(re))
    } else {
        let start = cursor.pos;
        while let Some(c) = cursor.peek() {
            if c == '(' || c.is_ascii_whitespace() {
                break;
            }
            cursor.advance(c.len_utf8());
        }
        let name = &cursor.input[start..cursor.pos];
        if name.is_empty() {
            return Err(cursor.error("empty tool name"));
        }
        if has_glob_chars(name) {
            Ok(ToolMatcher::Glob(name.to_string()))
        } else {
            Ok(ToolMatcher::Exact(name.to_string()))
        }
    }
}

fn has_glob_chars(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}

fn parse_arg_part(cursor: &mut Cursor<'_>) -> Result<ArgMatcher, PatternParseError> {
    cursor.skip_whitespace();

    if cursor.peek() == Some('*') {
        let after = cursor.remaining().get(1..2);
        if after.is_none_or(|s| {
            let c = s.chars().next().unwrap_or(')');
            c == ')' || c.is_ascii_whitespace()
        }) {
            cursor.advance(1);
            cursor.skip_whitespace();
            return Ok(ArgMatcher::Any);
        }
    }

    if looks_like_field_conditions(cursor.remaining()) {
        parse_field_conditions(cursor)
    } else {
        parse_primary_value(cursor)
    }
}

fn looks_like_field_conditions(s: &str) -> bool {
    let s = s.trim();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c.is_ascii_alphanumeric() || c == '_' || c == '.' {
            i += 1;
        } else if c == '[' {
            i += 1;
            while i < bytes.len() && bytes[i] != b']' {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
        } else if c == '*' {
            i += 1;
            if i < bytes.len() && (bytes[i] == b'.' || bytes[i] == b'[') {
                continue;
            }
            break;
        } else {
            break;
        }
    }
    while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
        i += 1;
    }
    let remaining = &s[i..];
    remaining.starts_with("~")
        || remaining.starts_with("=")
        || remaining.starts_with("!~")
        || remaining.starts_with("!=")
}

fn parse_field_conditions(cursor: &mut Cursor<'_>) -> Result<ArgMatcher, PatternParseError> {
    let mut conditions = Vec::new();
    loop {
        cursor.skip_whitespace();
        conditions.push(parse_single_field_condition(cursor)?);
        cursor.skip_whitespace();
        if cursor.peek() == Some(',') {
            cursor.advance(1);
        } else {
            break;
        }
    }
    Ok(ArgMatcher::Fields(conditions))
}

fn parse_single_field_condition(
    cursor: &mut Cursor<'_>,
) -> Result<FieldCondition, PatternParseError> {
    cursor.skip_whitespace();
    let path = parse_field_path(cursor)?;
    cursor.skip_whitespace();
    let op = parse_match_op(cursor)?;
    cursor.skip_whitespace();
    let value = parse_quoted_value(cursor)?;
    Ok(FieldCondition { path, op, value })
}

fn parse_field_path(cursor: &mut Cursor<'_>) -> Result<Vec<PathSegment>, PatternParseError> {
    let mut segments = Vec::new();
    loop {
        cursor.skip_whitespace();
        if cursor.peek() == Some('*') {
            cursor.advance(1);
            segments.push(PathSegment::Wildcard);
        } else {
            let ident = parse_identifier(cursor)?;
            segments.push(PathSegment::Field(ident));
        }

        while cursor.peek() == Some('[') {
            cursor.advance(1);
            cursor.skip_whitespace();
            if cursor.peek() == Some('*') {
                cursor.advance(1);
                segments.push(PathSegment::AnyIndex);
            } else {
                let idx = parse_usize(cursor)?;
                segments.push(PathSegment::Index(idx));
            }
            cursor.expect(']')?;
        }

        if cursor.peek() == Some('.') {
            cursor.advance(1);
        } else {
            break;
        }
    }
    Ok(segments)
}

fn parse_identifier(cursor: &mut Cursor<'_>) -> Result<String, PatternParseError> {
    let start = cursor.pos;
    while let Some(c) = cursor.peek() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            cursor.advance(1);
        } else {
            break;
        }
    }
    let ident = &cursor.input[start..cursor.pos];
    if ident.is_empty() {
        return Err(cursor.error("expected identifier"));
    }
    Ok(ident.to_string())
}

fn parse_usize(cursor: &mut Cursor<'_>) -> Result<usize, PatternParseError> {
    let start = cursor.pos;
    while let Some(c) = cursor.peek() {
        if c.is_ascii_digit() {
            cursor.advance(1);
        } else {
            break;
        }
    }
    let digits = &cursor.input[start..cursor.pos];
    digits
        .parse::<usize>()
        .map_err(|_| cursor.error(format!("invalid index: '{digits}'")))
}

fn parse_match_op(cursor: &mut Cursor<'_>) -> Result<MatchOp, PatternParseError> {
    let remaining = cursor.remaining();
    if remaining.starts_with("!=~") {
        cursor.advance(3);
        Ok(MatchOp::NotRegex)
    } else if remaining.starts_with("!=") {
        cursor.advance(2);
        Ok(MatchOp::NotExact)
    } else if remaining.starts_with("!~") {
        cursor.advance(2);
        Ok(MatchOp::NotGlob)
    } else if remaining.starts_with("=~") {
        cursor.advance(2);
        Ok(MatchOp::Regex)
    } else if remaining.starts_with('~') {
        cursor.advance(1);
        Ok(MatchOp::Glob)
    } else if remaining.starts_with('=') {
        cursor.advance(1);
        Ok(MatchOp::Exact)
    } else {
        Err(cursor.error("expected operator: ~, =, =~, !~, !=, or !=~"))
    }
}

fn parse_quoted_value(cursor: &mut Cursor<'_>) -> Result<String, PatternParseError> {
    cursor.skip_whitespace();
    if cursor.peek() != Some('"') {
        return Err(cursor.error("expected '\"' to start value"));
    }
    cursor.advance(1);
    let mut value = String::new();
    loop {
        match cursor.peek() {
            None => return Err(cursor.error("unterminated string literal")),
            Some('"') => {
                cursor.advance(1);
                break;
            }
            Some('\\') => {
                cursor.advance(1);
                match cursor.peek() {
                    Some(c @ ('"' | '\\')) => {
                        value.push(c);
                        cursor.advance(1);
                    }
                    Some(c) => {
                        value.push('\\');
                        value.push(c);
                        cursor.advance(c.len_utf8());
                    }
                    None => return Err(cursor.error("unterminated escape sequence")),
                }
            }
            Some(c) => {
                value.push(c);
                cursor.advance(c.len_utf8());
            }
        }
    }
    Ok(value)
}

fn parse_primary_value(cursor: &mut Cursor<'_>) -> Result<ArgMatcher, PatternParseError> {
    cursor.skip_whitespace();
    let start = cursor.pos;

    let mut depth = 0u32;
    while let Some(c) = cursor.peek() {
        match c {
            '(' => {
                depth += 1;
                cursor.advance(1);
            }
            ')' if depth > 0 => {
                depth -= 1;
                cursor.advance(1);
            }
            ')' => break,
            _ => cursor.advance(c.len_utf8()),
        }
    }

    let value = cursor.input[start..cursor.pos].trim();
    if value.is_empty() {
        return Err(cursor.error("empty primary pattern"));
    }
    Ok(ArgMatcher::Primary {
        op: MatchOp::Glob,
        value: value.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- Display tests ---

    #[test]
    fn display_exact_tool_any_args() {
        let p = ToolCallPattern::tool("Bash");
        assert_eq!(p.to_string(), "Bash");
    }

    #[test]
    fn display_glob_tool() {
        let p = ToolCallPattern::tool_glob("mcp__github__*");
        assert_eq!(p.to_string(), "mcp__github__*");
    }

    #[test]
    fn display_regex_tool() {
        let p = ToolCallPattern {
            tool: ToolMatcher::Regex(regex::Regex::new(r"mcp__(gh|gl)__.*").unwrap()),
            args: ArgMatcher::Any,
        };
        assert_eq!(p.to_string(), "/mcp__(gh|gl)__.*/");
    }

    #[test]
    fn display_primary_glob() {
        let p = ToolCallPattern::tool_with_primary("Bash", "npm *");
        assert_eq!(p.to_string(), "Bash(npm *)");
    }

    #[test]
    fn display_primary_exact() {
        let p = ToolCallPattern {
            tool: ToolMatcher::Exact("Bash".into()),
            args: ArgMatcher::Primary {
                op: MatchOp::Exact,
                value: "git status".into(),
            },
        };
        assert_eq!(p.to_string(), r#"Bash(= "git status")"#);
    }

    #[test]
    fn display_named_field_glob() {
        let p = ToolCallPattern {
            tool: ToolMatcher::Exact("Edit".into()),
            args: ArgMatcher::Fields(vec![FieldCondition {
                path: vec![PathSegment::Field("file_path".into())],
                op: MatchOp::Glob,
                value: "src/**/*.rs".into(),
            }]),
        };
        assert_eq!(p.to_string(), r#"Edit(file_path ~ "src/**/*.rs")"#);
    }

    #[test]
    fn display_nested_path() {
        let p = ToolCallPattern {
            tool: ToolMatcher::Exact("mcp__db__query".into()),
            args: ArgMatcher::Fields(vec![FieldCondition {
                path: vec![
                    PathSegment::Field("queries".into()),
                    PathSegment::AnyIndex,
                    PathSegment::Field("sql".into()),
                ],
                op: MatchOp::Regex,
                value: "(?i)DROP".into(),
            }]),
        };
        assert_eq!(
            p.to_string(),
            r#"mcp__db__query(queries[*].sql =~ "(?i)DROP")"#
        );
    }

    #[test]
    fn display_multi_field() {
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
                    op: MatchOp::Glob,
                    value: "*| *".into(),
                },
            ]),
        };
        assert_eq!(
            p.to_string(),
            r#"Bash(command ~ "curl *", command ~ "*| *")"#
        );
    }

    #[test]
    fn display_wildcard_path_segment() {
        let p = ToolCallPattern {
            tool: ToolMatcher::Glob("mcp__*".into()),
            args: ArgMatcher::Fields(vec![FieldCondition {
                path: vec![PathSegment::Wildcard, PathSegment::Field("password".into())],
                op: MatchOp::Regex,
                value: ".*".into(),
            }]),
        };
        assert_eq!(p.to_string(), r#"mcp__*(*.password =~ ".*")"#);
    }

    #[test]
    fn equality_for_regex_tool_matcher() {
        let a = ToolMatcher::Regex(regex::Regex::new("abc").unwrap());
        let b = ToolMatcher::Regex(regex::Regex::new("abc").unwrap());
        let c = ToolMatcher::Regex(regex::Regex::new("xyz").unwrap());
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    // --- Parser tests ---

    #[test]
    fn parse_exact_tool_only() {
        let p = parse_pattern("Bash").unwrap();
        assert_eq!(p.tool, ToolMatcher::Exact("Bash".into()));
        assert_eq!(p.args, ArgMatcher::Any);
    }

    #[test]
    fn parse_glob_tool_only() {
        let p = parse_pattern("mcp__github__*").unwrap();
        assert_eq!(p.tool, ToolMatcher::Glob("mcp__github__*".into()));
        assert_eq!(p.args, ArgMatcher::Any);
    }

    #[test]
    fn parse_regex_tool() {
        let p = parse_pattern(r"/mcp__(gh|gl)__.*/").unwrap();
        assert!(matches!(p.tool, ToolMatcher::Regex(_)));
        if let ToolMatcher::Regex(re) = &p.tool {
            assert_eq!(re.as_str(), r"mcp__(gh|gl)__.*");
        }
        assert_eq!(p.args, ArgMatcher::Any);
    }

    #[test]
    fn parse_any_args_explicit() {
        let p = parse_pattern("Bash(*)").unwrap();
        assert_eq!(p.tool, ToolMatcher::Exact("Bash".into()));
        assert_eq!(p.args, ArgMatcher::Any);
    }

    #[test]
    fn parse_primary_glob() {
        let p = parse_pattern("Bash(npm *)").unwrap();
        assert_eq!(p.tool, ToolMatcher::Exact("Bash".into()));
        assert_eq!(
            p.args,
            ArgMatcher::Primary {
                op: MatchOp::Glob,
                value: "npm *".into()
            }
        );
    }

    #[test]
    fn parse_primary_glob_git_status() {
        let p = parse_pattern("Bash(git status)").unwrap();
        assert_eq!(
            p.args,
            ArgMatcher::Primary {
                op: MatchOp::Glob,
                value: "git status".into()
            }
        );
    }

    #[test]
    fn parse_named_field_glob() {
        let p = parse_pattern(r#"Edit(file_path ~ "src/**/*.rs")"#).unwrap();
        assert_eq!(p.tool, ToolMatcher::Exact("Edit".into()));
        if let ArgMatcher::Fields(conditions) = &p.args {
            assert_eq!(conditions.len(), 1);
            assert_eq!(
                conditions[0].path,
                vec![PathSegment::Field("file_path".into())]
            );
            assert_eq!(conditions[0].op, MatchOp::Glob);
            assert_eq!(conditions[0].value, "src/**/*.rs");
        } else {
            panic!("expected Fields");
        }
    }

    #[test]
    fn parse_named_field_regex() {
        let p = parse_pattern(r#"Bash(command =~ "(?i)eval|exec")"#).unwrap();
        if let ArgMatcher::Fields(conditions) = &p.args {
            assert_eq!(conditions[0].op, MatchOp::Regex);
            assert_eq!(conditions[0].value, "(?i)eval|exec");
        } else {
            panic!("expected Fields");
        }
    }

    #[test]
    fn parse_named_field_not_glob() {
        let p = parse_pattern(r#"Bash(command !~ "npm *")"#).unwrap();
        if let ArgMatcher::Fields(conditions) = &p.args {
            assert_eq!(conditions[0].op, MatchOp::NotGlob);
        } else {
            panic!("expected Fields");
        }
    }

    #[test]
    fn parse_named_field_exact() {
        let p = parse_pattern(r#"Bash(command = "ls")"#).unwrap();
        if let ArgMatcher::Fields(conditions) = &p.args {
            assert_eq!(conditions[0].op, MatchOp::Exact);
            assert_eq!(conditions[0].value, "ls");
        } else {
            panic!("expected Fields");
        }
    }

    #[test]
    fn parse_named_field_not_exact() {
        let p = parse_pattern(r#"Bash(command != "rm")"#).unwrap();
        if let ArgMatcher::Fields(conditions) = &p.args {
            assert_eq!(conditions[0].op, MatchOp::NotExact);
        } else {
            panic!("expected Fields");
        }
    }

    #[test]
    fn parse_named_field_not_regex() {
        let p = parse_pattern(r#"Bash(command !=~ "danger")"#).unwrap();
        if let ArgMatcher::Fields(conditions) = &p.args {
            assert_eq!(conditions[0].op, MatchOp::NotRegex);
        } else {
            panic!("expected Fields");
        }
    }

    #[test]
    fn parse_nested_path_any_index() {
        let p = parse_pattern(r#"mcp__db__query(queries[*].sql =~ "(?i)DROP")"#).unwrap();
        if let ArgMatcher::Fields(conditions) = &p.args {
            assert_eq!(
                conditions[0].path,
                vec![
                    PathSegment::Field("queries".into()),
                    PathSegment::AnyIndex,
                    PathSegment::Field("sql".into()),
                ]
            );
        } else {
            panic!("expected Fields");
        }
    }

    #[test]
    fn parse_nested_path_specific_index() {
        let p = parse_pattern(r#"Tool(items[0].name = "test")"#).unwrap();
        if let ArgMatcher::Fields(conditions) = &p.args {
            assert_eq!(
                conditions[0].path,
                vec![
                    PathSegment::Field("items".into()),
                    PathSegment::Index(0),
                    PathSegment::Field("name".into()),
                ]
            );
        } else {
            panic!("expected Fields");
        }
    }

    #[test]
    fn parse_wildcard_path_segment() {
        let p = parse_pattern(r#"mcp__*(*.password =~ ".*")"#).unwrap();
        assert_eq!(p.tool, ToolMatcher::Glob("mcp__*".into()));
        if let ArgMatcher::Fields(conditions) = &p.args {
            assert_eq!(
                conditions[0].path,
                vec![PathSegment::Wildcard, PathSegment::Field("password".into()),]
            );
        } else {
            panic!("expected Fields");
        }
    }

    #[test]
    fn parse_multi_field_and() {
        let p = parse_pattern(r#"Bash(command ~ "curl *", command ~ "*| *")"#).unwrap();
        if let ArgMatcher::Fields(conditions) = &p.args {
            assert_eq!(conditions.len(), 2);
            assert_eq!(conditions[0].value, "curl *");
            assert_eq!(conditions[1].value, "*| *");
        } else {
            panic!("expected Fields");
        }
    }

    #[test]
    fn parse_complex_multi_field() {
        let p = parse_pattern(
            r#"mcp__db__query(connection.host = "localhost", queries[*].sql =~ "^SELECT\b")"#,
        )
        .unwrap();
        if let ArgMatcher::Fields(conditions) = &p.args {
            assert_eq!(conditions.len(), 2);
            assert_eq!(
                conditions[0].path,
                vec![
                    PathSegment::Field("connection".into()),
                    PathSegment::Field("host".into()),
                ]
            );
            assert_eq!(conditions[0].op, MatchOp::Exact);
            assert_eq!(conditions[0].value, "localhost");

            assert_eq!(
                conditions[1].path,
                vec![
                    PathSegment::Field("queries".into()),
                    PathSegment::AnyIndex,
                    PathSegment::Field("sql".into()),
                ]
            );
            assert_eq!(conditions[1].op, MatchOp::Regex);
        } else {
            panic!("expected Fields");
        }
    }

    #[test]
    fn parse_display_round_trip_exact_tool() {
        let original = "Bash";
        let p = parse_pattern(original).unwrap();
        assert_eq!(p.to_string(), original);
    }

    #[test]
    fn parse_display_round_trip_glob_tool() {
        let original = "mcp__github__*";
        let p = parse_pattern(original).unwrap();
        assert_eq!(p.to_string(), original);
    }

    #[test]
    fn parse_display_round_trip_primary() {
        let original = "Bash(npm *)";
        let p = parse_pattern(original).unwrap();
        assert_eq!(p.to_string(), original);
    }

    #[test]
    fn parse_display_round_trip_named_field() {
        let original = r#"Edit(file_path ~ "src/**")"#;
        let p = parse_pattern(original).unwrap();
        assert_eq!(p.to_string(), original);
    }

    #[test]
    fn parse_display_round_trip_nested() {
        let original = r#"mcp__db__query(queries[*].sql =~ "(?i)DROP")"#;
        let p = parse_pattern(original).unwrap();
        assert_eq!(p.to_string(), original);
    }

    #[test]
    fn error_empty_input() {
        assert!(parse_pattern("").is_err());
    }

    #[test]
    fn error_unmatched_paren() {
        assert!(parse_pattern("Bash(npm *").is_err());
    }

    #[test]
    fn error_empty_regex() {
        assert!(parse_pattern("//").is_err());
    }

    #[test]
    fn error_invalid_regex() {
        assert!(parse_pattern("/[invalid/").is_err());
    }

    #[test]
    fn error_trailing_content() {
        assert!(parse_pattern("Bash extra").is_err());
    }

    #[test]
    fn serde_round_trip() {
        let p = ToolCallPattern::tool_with_primary("Bash", "npm *");
        let json_val = serde_json::to_string(&p).unwrap();
        assert_eq!(json_val, r#""Bash(npm *)""#);
    }

    #[test]
    fn serde_deserialize_round_trip() {
        let json_str = r#""Bash(npm *)""#;
        let p: ToolCallPattern = serde_json::from_str(json_str).unwrap();
        assert_eq!(p.tool, ToolMatcher::Exact("Bash".into()));
        assert_eq!(
            p.args,
            ArgMatcher::Primary {
                op: MatchOp::Glob,
                value: "npm *".into()
            }
        );
        let re_serialized = serde_json::to_string(&p).unwrap();
        assert_eq!(re_serialized, json_str);
    }

    #[test]
    fn serde_deserialize_named_field() {
        let json_str = r#""Edit(file_path ~ \"src/**\")""#;
        let p: ToolCallPattern = serde_json::from_str(json_str).unwrap();
        assert_eq!(p.to_string(), r#"Edit(file_path ~ "src/**")"#);
    }

    // --- Ruleset tests ---

    #[test]
    fn ruleset_deny_overrides_allow() {
        let mut ruleset = PermissionRuleset::default();
        ruleset.rules.insert(
            "tool:Bash".into(),
            PermissionRule::new_tool("Bash", ToolPermissionBehavior::Allow),
        );
        ruleset.rules.insert(
            "pattern:Bash(rm *)".into(),
            PermissionRule::new_pattern(
                ToolCallPattern::tool_with_primary("Bash", "rm *"),
                ToolPermissionBehavior::Deny,
            ),
        );

        let eval = evaluate_tool_permission(&ruleset, "Bash", &json!({"command": "rm -rf /"}));
        assert_eq!(eval.behavior, ToolPermissionBehavior::Deny);
    }

    #[test]
    fn ruleset_allow_when_no_deny() {
        let mut ruleset = PermissionRuleset::default();
        ruleset.rules.insert(
            "tool:Bash".into(),
            PermissionRule::new_tool("Bash", ToolPermissionBehavior::Allow),
        );

        let eval = evaluate_tool_permission(&ruleset, "Bash", &json!({"command": "ls"}));
        assert_eq!(eval.behavior, ToolPermissionBehavior::Allow);
    }

    #[test]
    fn ruleset_default_behavior_when_no_match() {
        let ruleset = PermissionRuleset {
            default_behavior: ToolPermissionBehavior::Ask,
            rules: HashMap::new(),
        };

        let eval = evaluate_tool_permission(&ruleset, "Bash", &json!({}));
        assert_eq!(eval.behavior, ToolPermissionBehavior::Ask);
    }

    #[test]
    fn ruleset_unconditionally_denied_tools() {
        let mut ruleset = PermissionRuleset::default();
        ruleset.rules.insert(
            "tool:rm".into(),
            PermissionRule::new_tool("rm", ToolPermissionBehavior::Deny),
        );
        ruleset.rules.insert(
            "pattern:Bash".into(),
            PermissionRule::new_pattern(
                ToolCallPattern::tool("Bash"),
                ToolPermissionBehavior::Deny,
            ),
        );
        // Pattern with args should NOT be listed as unconditionally denied
        ruleset.rules.insert(
            "pattern:Edit(file_path)".into(),
            PermissionRule::new_pattern(
                ToolCallPattern::tool_with_primary("Edit", "/etc/*"),
                ToolPermissionBehavior::Deny,
            ),
        );

        let denied = ruleset.unconditionally_denied_tools();
        assert!(denied.contains(&"rm"));
        assert!(denied.contains(&"Bash"));
        assert!(!denied.iter().any(|t| *t == "Edit"));
    }

    #[test]
    fn evaluate_returns_matched_rule() {
        let mut ruleset = PermissionRuleset::default();
        let rule = PermissionRule::new_tool("Bash", ToolPermissionBehavior::Allow);
        ruleset.rules.insert("tool:Bash".into(), rule.clone());

        let eval = evaluate_tool_permission(&ruleset, "Bash", &json!({}));
        assert_eq!(eval.matched_rule, Some(rule));
    }

    #[test]
    fn evaluate_no_matched_rule_uses_default() {
        let ruleset = PermissionRuleset {
            default_behavior: ToolPermissionBehavior::Deny,
            rules: HashMap::new(),
        };

        let eval = evaluate_tool_permission(&ruleset, "Bash", &json!({}));
        assert_eq!(eval.behavior, ToolPermissionBehavior::Deny);
        assert!(eval.matched_rule.is_none());
    }
}
