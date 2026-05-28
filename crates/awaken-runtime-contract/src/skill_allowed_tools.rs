use serde::{Deserialize, Serialize};

/// Parsed `allowed-tools` token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AllowedTool {
    /// Raw token as declared.
    pub raw: String,
    /// Base tool id or matcher before optional scope `(...)`.
    pub tool_id: String,
    /// Optional scope/selector payload inside `(...)`.
    pub scope: Option<String>,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum AllowedToolParseError {
    #[error("{0}")]
    Invalid(String),
}

/// Return true when an allowed-tools `tool_id` should be treated as a matcher
/// rather than an exact tool id.
#[must_use]
pub fn is_skill_allowed_tool_pattern(tool_id: &str) -> bool {
    tool_id.contains('*')
        || tool_id.contains('?')
        || tool_id.contains('[')
        || tool_id.starts_with('/')
}

/// Validate a pattern-like allowed-tools matcher using the same parser used by
/// runtime permission rules and agent tool-pattern configuration.
pub fn validate_skill_allowed_tool_pattern(tool_id: &str) -> Result<(), AllowedToolParseError> {
    if !is_skill_allowed_tool_pattern(tool_id) {
        return Ok(());
    }

    if !tool_id.starts_with('/') {
        awaken_tool_pattern::validate_tool_id_pattern(tool_id).map_err(|error| {
            AllowedToolParseError::Invalid(format!(
                "invalid allowed-tools pattern '{tool_id}': {error}"
            ))
        })?;
    }

    awaken_tool_pattern::parse_pattern(tool_id)
        .map(|_| ())
        .map_err(|error| {
            AllowedToolParseError::Invalid(format!(
                "invalid allowed-tools pattern '{tool_id}': {error}"
            ))
        })
}

/// Parse an `allowed-tools` string into ordered tokens.
///
/// The grammar mirrors SKILL.md frontmatter: tokens are whitespace-separated
/// except while inside parentheses or quotes.
pub fn parse_skill_allowed_tools(value: &str) -> Result<Vec<AllowedTool>, AllowedToolParseError> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut paren_depth = 0usize;
    let mut in_quote: Option<char> = None;
    let mut escaped = false;

    for ch in value.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }

        if let Some(q) = in_quote {
            current.push(ch);
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == q {
                in_quote = None;
            }
            continue;
        }

        match ch {
            '"' | '\'' => {
                in_quote = Some(ch);
                current.push(ch);
            }
            '(' => {
                paren_depth += 1;
                current.push(ch);
            }
            ')' => {
                if paren_depth == 0 {
                    return Err(AllowedToolParseError::Invalid(
                        "allowed-tools contains unmatched ')'".to_string(),
                    ));
                }
                paren_depth -= 1;
                current.push(ch);
            }
            c if c.is_whitespace() && paren_depth == 0 => {
                let t = current.trim();
                if !t.is_empty() {
                    tokens.push(t.to_string());
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }

    if in_quote.is_some() {
        return Err(AllowedToolParseError::Invalid(
            "allowed-tools contains unterminated quote".to_string(),
        ));
    }
    if paren_depth != 0 {
        return Err(AllowedToolParseError::Invalid(
            "allowed-tools contains unbalanced parentheses".to_string(),
        ));
    }

    let t = current.trim();
    if !t.is_empty() {
        tokens.push(t.to_string());
    }

    tokens
        .into_iter()
        .map(parse_skill_allowed_tool_token)
        .collect::<Result<Vec<_>, _>>()
}

/// Parse one `allowed-tools` token.
pub fn parse_skill_allowed_tool_token(token: String) -> Result<AllowedTool, AllowedToolParseError> {
    let raw = token.trim().to_string();
    if raw.is_empty() {
        return Err(AllowedToolParseError::Invalid(
            "allowed-tools contains an empty token".to_string(),
        ));
    }

    let (tool_id, scope) = if let Some(open_idx) = raw.find('(') {
        if !raw.ends_with(')') {
            return Err(AllowedToolParseError::Invalid(format!(
                "invalid allowed-tools token '{raw}'"
            )));
        }
        let base = raw[..open_idx].trim();
        let inner = raw[open_idx + 1..raw.len() - 1].to_string();
        (base.to_string(), Some(inner))
    } else {
        (raw.clone(), None)
    };

    if tool_id.is_empty() {
        return Err(AllowedToolParseError::Invalid(format!(
            "invalid allowed-tools token '{raw}'"
        )));
    }

    if tool_id
        .chars()
        .any(|c| c.is_whitespace() || c == '(' || c == ')')
    {
        return Err(AllowedToolParseError::Invalid(format!(
            "invalid tool id in allowed-tools token '{raw}'"
        )));
    }

    Ok(AllowedTool {
        raw,
        tool_id,
        scope,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_allowed_tools_preserves_scoped_token_with_spaces() {
        let parsed = parse_skill_allowed_tools(r#"read_file Bash(command: "git status")"#).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].tool_id, "read_file");
        assert_eq!(parsed[1].tool_id, "Bash");
        assert_eq!(parsed[1].scope.as_deref(), Some(r#"command: "git status""#));
    }

    #[test]
    fn parse_allowed_tool_rejects_malformed_parentheses() {
        assert!(parse_skill_allowed_tools("tool)").is_err());
        assert!(parse_skill_allowed_tool_token("()".to_string()).is_err());
        assert!(parse_skill_allowed_tool_token("foo)bar(".to_string()).is_err());
    }

    #[test]
    fn parse_allowed_tools_rejects_unterminated_quote() {
        assert!(parse_skill_allowed_tools(r#"Bash("unclosed)"#).is_err());
    }

    #[test]
    fn validate_pattern_uses_runtime_pattern_parser() {
        assert!(validate_skill_allowed_tool_pattern("mcp__db__*").is_ok());
        assert!(validate_skill_allowed_tool_pattern("/mcp__db__.*/").is_ok());
        assert!(validate_skill_allowed_tool_pattern("Bash").is_ok());

        let err =
            validate_skill_allowed_tool_pattern("/[invalid/").expect_err("invalid regex must fail");
        assert!(err.to_string().contains("invalid allowed-tools pattern"));

        let err = validate_skill_allowed_tool_pattern(r"mcp__db__*\")
            .expect_err("invalid glob must fail");
        assert!(err.to_string().contains("dangling escape"));
    }
}
