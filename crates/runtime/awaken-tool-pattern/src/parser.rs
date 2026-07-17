//! Pattern string parser for [`ToolCallPattern`].
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

use std::fmt;

use crate::types::{
    ArgMatcher, FieldCondition, MatchOp, PathSegment, ToolCallPattern, ToolMatcher,
};

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

impl ToolCallPattern {
    /// Parse a pattern string. Convenience wrapper over [`parse_pattern`].
    pub fn parse(input: &str) -> Result<Self, PatternParseError> {
        parse_pattern(input)
    }
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

// ---------------------------------------------------------------------------
// Serde for ToolCallPattern (uses the parser)
// ---------------------------------------------------------------------------

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

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a tool call pattern string like \"Bash(npm *)\"")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                parse_pattern(v).map_err(serde::de::Error::custom)
            }
        }

        deserializer.deserialize_str(PatternVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }

    #[test]
    fn parse_primary_glob() {
        let p = parse_pattern("Bash(npm *)").unwrap();
        assert_eq!(
            p.args,
            ArgMatcher::Primary {
                op: MatchOp::Glob,
                value: "npm *".into()
            }
        );
    }

    #[test]
    fn parse_named_field_glob() {
        let p = parse_pattern(r#"Edit(file_path ~ "src/**/*.rs")"#).unwrap();
        if let ArgMatcher::Fields(conditions) = &p.args {
            assert_eq!(conditions[0].op, MatchOp::Glob);
            assert_eq!(conditions[0].value, "src/**/*.rs");
        } else {
            panic!("expected Fields");
        }
    }

    #[test]
    fn serde_round_trip() {
        let p = ToolCallPattern::tool_with_primary("Bash", "npm *");
        let json_val = serde_json::to_string(&p).unwrap();
        assert_eq!(json_val, r#""Bash(npm *)""#);
        let decoded: ToolCallPattern = serde_json::from_str(&json_val).unwrap();
        assert_eq!(decoded, p);
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
    fn parse_regex_tool() {
        let p = parse_pattern(r"/mcp__(github|gitlab)__.*/").unwrap();
        assert!(matches!(p.tool, ToolMatcher::Regex(_)));
        assert_eq!(p.args, ArgMatcher::Any);
    }

    #[test]
    fn parse_regex_tool_with_escape() {
        let p = parse_pattern(r"/foo\/bar/").unwrap();
        if let ToolMatcher::Regex(re) = &p.tool {
            assert_eq!(re.as_str(), r"foo\/bar");
        } else {
            panic!("expected Regex");
        }
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
    fn parse_explicit_any_args() {
        let p = parse_pattern("Bash(*)").unwrap();
        assert_eq!(p.args, ArgMatcher::Any);
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
    fn parse_named_field_regex() {
        let p = parse_pattern(r#"Bash(command =~ "(?i)rm")"#).unwrap();
        if let ArgMatcher::Fields(conditions) = &p.args {
            assert_eq!(conditions[0].op, MatchOp::Regex);
        } else {
            panic!("expected Fields");
        }
    }

    #[test]
    fn parse_negated_operators() {
        let p1 = parse_pattern(r#"T(f !~ "pat")"#).unwrap();
        let p2 = parse_pattern(r#"T(f != "val")"#).unwrap();
        let p3 = parse_pattern(r#"T(f !=~ "re")"#).unwrap();
        if let ArgMatcher::Fields(c) = &p1.args {
            assert_eq!(c[0].op, MatchOp::NotGlob);
        }
        if let ArgMatcher::Fields(c) = &p2.args {
            assert_eq!(c[0].op, MatchOp::NotExact);
        }
        if let ArgMatcher::Fields(c) = &p3.args {
            assert_eq!(c[0].op, MatchOp::NotRegex);
        }
    }

    #[test]
    fn parse_multi_field_conditions() {
        let p = parse_pattern(r#"Tool(f1 ~ "a", f2 = "b")"#).unwrap();
        if let ArgMatcher::Fields(conditions) = &p.args {
            assert_eq!(conditions.len(), 2);
            assert_eq!(conditions[0].op, MatchOp::Glob);
            assert_eq!(conditions[1].op, MatchOp::Exact);
        } else {
            panic!("expected Fields");
        }
    }

    #[test]
    fn parse_nested_field_path() {
        let p = parse_pattern(r#"Tool(a.b[*].c ~ "pat")"#).unwrap();
        if let ArgMatcher::Fields(conditions) = &p.args {
            let path = &conditions[0].path;
            assert_eq!(path.len(), 4);
            assert_eq!(path[0], PathSegment::Field("a".into()));
            assert_eq!(path[1], PathSegment::Field("b".into()));
            assert_eq!(path[2], PathSegment::AnyIndex);
            assert_eq!(path[3], PathSegment::Field("c".into()));
        } else {
            panic!("expected Fields");
        }
    }

    #[test]
    fn parse_specific_index_path() {
        let p = parse_pattern(r#"Tool(items[0] = "val")"#).unwrap();
        if let ArgMatcher::Fields(conditions) = &p.args {
            assert_eq!(conditions[0].path[1], PathSegment::Index(0));
        } else {
            panic!("expected Fields");
        }
    }

    #[test]
    fn parse_wildcard_path_segment() {
        let p = parse_pattern(r#"Tool(*.id = "val")"#).unwrap();
        if let ArgMatcher::Fields(conditions) = &p.args {
            assert_eq!(conditions[0].path[0], PathSegment::Wildcard);
            assert_eq!(conditions[0].path[1], PathSegment::Field("id".into()));
        } else {
            panic!("expected Fields");
        }
    }

    #[test]
    fn parse_escaped_quote_in_value() {
        let p = parse_pattern(r#"T(f = "say \"hello\"")"#).unwrap();
        if let ArgMatcher::Fields(c) = &p.args {
            assert_eq!(c[0].value, r#"say "hello""#);
        } else {
            panic!("expected Fields");
        }
    }

    #[test]
    fn parse_escaped_backslash_in_value() {
        let p = parse_pattern(r#"T(f = "path\\file")"#).unwrap();
        if let ArgMatcher::Fields(c) = &p.args {
            assert_eq!(c[0].value, r"path\file");
        } else {
            panic!("expected Fields");
        }
    }

    #[test]
    fn parse_non_special_escape_in_value() {
        let p = parse_pattern(r#"T(f = "hello\nworld")"#).unwrap();
        if let ArgMatcher::Fields(c) = &p.args {
            // Non-special escapes preserve the backslash
            assert_eq!(c[0].value, "hello\\nworld");
        } else {
            panic!("expected Fields");
        }
    }

    #[test]
    fn error_trailing_chars() {
        assert!(parse_pattern("Bash extra").is_err());
    }

    #[test]
    fn error_unterminated_string() {
        assert!(parse_pattern(r#"T(f = "unterminated)"#).is_err());
    }

    #[test]
    fn error_unterminated_escape() {
        assert!(parse_pattern(r#"T(f = "end\"#).is_err());
    }

    #[test]
    fn error_missing_quote() {
        assert!(parse_pattern(r#"T(f = noquote)"#).is_err());
    }

    #[test]
    fn error_bad_operator() {
        // f = needs a quoted value; unquoted triggers an error
        assert!(parse_pattern(r#"T(f = unquoted)"#).is_err());
    }

    #[test]
    fn parse_pattern_error_display() {
        let err = parse_pattern("").unwrap_err();
        assert!(err.to_string().contains("parse error at"));
    }

    #[test]
    fn serde_deserialize_invalid() {
        let result: Result<ToolCallPattern, _> = serde_json::from_str(r#""""#);
        assert!(result.is_err());
    }

    #[test]
    fn parse_glob_tool_question_mark() {
        let p = parse_pattern("Bas?").unwrap();
        assert_eq!(p.tool, ToolMatcher::Glob("Bas?".into()));
    }

    #[test]
    fn parse_glob_tool_bracket() {
        let p = parse_pattern("Bas[hH]").unwrap();
        assert_eq!(p.tool, ToolMatcher::Glob("Bas[hH]".into()));
    }

    // -----------------------------------------------------------------------
    // parse <-> Display round-trip fidelity (`parse(p.to_string()) == p`)
    //
    // proptest is NOT a dev-dependency of this crate, so this uses an
    // exhaustive hand-written table of representative canonical `Pattern`
    // values covering every variant (tool: exact/glob/regex incl. escaped
    // slash; args: any/primary/all six field ops; nested paths: index /
    // any-index / wildcard; multi-field AND). Every value here is one the
    // parser can itself emit, so both directions must agree.
    // -----------------------------------------------------------------------
    #[test]
    fn parse_display_round_trip_exhaustive() {
        let re = |s: &str| regex::Regex::new(s).unwrap();
        let field = |name: &str| PathSegment::Field(name.into());
        let cond = |path: Vec<PathSegment>, op: MatchOp, value: &str| FieldCondition {
            path,
            op,
            value: value.into(),
        };

        let patterns: Vec<ToolCallPattern> = vec![
            // --- tool-only (ArgMatcher::Any) across all ToolMatcher kinds ---
            ToolCallPattern {
                tool: ToolMatcher::Exact("Bash".into()),
                args: ArgMatcher::Any,
            },
            ToolCallPattern {
                tool: ToolMatcher::Glob("mcp__github__*".into()),
                args: ArgMatcher::Any,
            },
            ToolCallPattern {
                tool: ToolMatcher::Glob("Bas?".into()),
                args: ArgMatcher::Any,
            },
            ToolCallPattern {
                tool: ToolMatcher::Glob("Bas[hH]".into()),
                args: ArgMatcher::Any,
            },
            ToolCallPattern {
                tool: ToolMatcher::Regex(re("mcp__(github|gitlab)__.*")),
                args: ArgMatcher::Any,
            },
            // escaped-slash regex tool — the tricky escaping case
            ToolCallPattern {
                tool: ToolMatcher::Regex(re(r"foo\/bar")),
                args: ArgMatcher::Any,
            },
            // --- primary glob (the only Primary form the parser emits) ---
            ToolCallPattern {
                tool: ToolMatcher::Exact("Bash".into()),
                args: ArgMatcher::Primary {
                    op: MatchOp::Glob,
                    value: "npm *".into(),
                },
            },
            ToolCallPattern {
                tool: ToolMatcher::Glob("Bas*".into()),
                args: ArgMatcher::Primary {
                    op: MatchOp::Glob,
                    value: "git status".into(),
                },
            },
            // --- named fields, one entry per operator ---
            ToolCallPattern {
                tool: ToolMatcher::Exact("Edit".into()),
                args: ArgMatcher::Fields(vec![cond(
                    vec![field("file_path")],
                    MatchOp::Glob,
                    "src/**/*.rs",
                )]),
            },
            ToolCallPattern {
                tool: ToolMatcher::Exact("Bash".into()),
                args: ArgMatcher::Fields(vec![cond(vec![field("command")], MatchOp::Exact, "ls")]),
            },
            ToolCallPattern {
                tool: ToolMatcher::Exact("Bash".into()),
                args: ArgMatcher::Fields(vec![cond(
                    vec![field("command")],
                    MatchOp::Regex,
                    "(?i)rm",
                )]),
            },
            ToolCallPattern {
                tool: ToolMatcher::Exact("Bash".into()),
                args: ArgMatcher::Fields(vec![cond(
                    vec![field("command")],
                    MatchOp::NotGlob,
                    "rm *",
                )]),
            },
            ToolCallPattern {
                tool: ToolMatcher::Exact("Bash".into()),
                args: ArgMatcher::Fields(vec![cond(
                    vec![field("command")],
                    MatchOp::NotExact,
                    "rm",
                )]),
            },
            ToolCallPattern {
                tool: ToolMatcher::Exact("Bash".into()),
                args: ArgMatcher::Fields(vec![cond(
                    vec![field("command")],
                    MatchOp::NotRegex,
                    "^rm",
                )]),
            },
            // --- multi-field AND ---
            ToolCallPattern {
                tool: ToolMatcher::Exact("Tool".into()),
                args: ArgMatcher::Fields(vec![
                    cond(vec![field("f1")], MatchOp::Glob, "a*"),
                    cond(vec![field("f2")], MatchOp::Exact, "b"),
                ]),
            },
            // --- nested field paths: specific index / any-index / wildcard ---
            ToolCallPattern {
                tool: ToolMatcher::Exact("Tool".into()),
                args: ArgMatcher::Fields(vec![cond(
                    vec![field("items"), PathSegment::Index(0), field("name")],
                    MatchOp::Exact,
                    "foo",
                )]),
            },
            ToolCallPattern {
                tool: ToolMatcher::Exact("Tool".into()),
                args: ArgMatcher::Fields(vec![cond(
                    vec![field("a"), field("b"), PathSegment::AnyIndex, field("c")],
                    MatchOp::Glob,
                    "pat",
                )]),
            },
            ToolCallPattern {
                tool: ToolMatcher::Exact("Tool".into()),
                args: ArgMatcher::Fields(vec![cond(
                    vec![PathSegment::Wildcard, field("id")],
                    MatchOp::NotExact,
                    "secret",
                )]),
            },
            // --- value with a lone backslash before a non-special char:
            //     Display escapes the `\` to `\\`, parse collapses it back => round-trips ---
            ToolCallPattern {
                tool: ToolMatcher::Exact("T".into()),
                args: ArgMatcher::Fields(vec![cond(
                    vec![field("f")],
                    MatchOp::Exact,
                    r"hello\nworld",
                )]),
            },
        ];

        for p in &patterns {
            let s = p.to_string();
            let reparsed = ToolCallPattern::parse(&s)
                .unwrap_or_else(|e| panic!("failed to reparse {s:?}: {e}"));
            assert_eq!(&reparsed, p, "round-trip mismatch for {s:?}");
            // Display of the reparsed pattern must reproduce the original string.
            assert_eq!(reparsed.to_string(), s, "second Display differs for {s:?}");
        }
    }

    // `FieldCondition`'s Display impl (types.rs) escapes `\` then `"` inside the
    // quoted value, exactly the inverse of `parse_quoted_value`'s unescaping, so a
    // field value containing a bare `"` survives a Display->parse round-trip (and
    // therefore serde serialization of such patterns).
    #[test]
    fn field_value_with_quote_round_trips() {
        let p = ToolCallPattern {
            tool: ToolMatcher::Exact("T".into()),
            args: ArgMatcher::Fields(vec![FieldCondition {
                path: vec![PathSegment::Field("f".into())],
                op: MatchOp::Exact,
                value: r#"say "hi""#.into(),
            }]),
        };
        // Display escapes the inner quotes.
        let s = p.to_string();
        assert_eq!(s, r#"T(f = "say \"hi\"")"#);
        // Which reparses back to the original pattern.
        let reparsed = ToolCallPattern::parse(&s).unwrap();
        assert_eq!(reparsed, p, "quoted value must round-trip");
    }

    // Sibling of the above for backslashes: Display escapes each `\` to `\\`, which
    // `parse_quoted_value` collapses back to a single `\`, so a value with
    // consecutive backslashes round-trips with none lost.
    #[test]
    fn field_value_with_double_backslash_round_trips() {
        let p = ToolCallPattern {
            tool: ToolMatcher::Exact("T".into()),
            args: ArgMatcher::Fields(vec![FieldCondition {
                path: vec![PathSegment::Field("f".into())],
                op: MatchOp::Exact,
                value: r"a\\b".into(), // a, backslash, backslash, b
            }]),
        };
        // Two backslashes are each escaped -> four backslashes on the wire.
        let s = p.to_string();
        assert_eq!(s, r#"T(f = "a\\\\b")"#);
        let reparsed = ToolCallPattern::parse(&s).unwrap();
        assert_eq!(reparsed, p, "double-backslash must round-trip");
        if let ArgMatcher::Fields(c) = &reparsed.args {
            assert_eq!(c[0].value, r"a\\b", "both backslashes preserved on reparse");
        } else {
            panic!("expected Fields");
        }
    }
}
