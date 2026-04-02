use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use unicode_normalization::UnicodeNormalization;

/// YAML frontmatter for a skill.
///
/// Follows the agentskills specification:
/// - frontmatter is required
/// - unknown keys are silently ignored for forward compatibility
/// - `allowed-tools` is a space-delimited string (not a YAML list)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SkillFrontmatter {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_metadata"
    )]
    pub metadata: Option<HashMap<String, String>>,
    /// Space-delimited list of allowed tools (spec).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<String>,

    // --- Visibility & invocation control ---
    /// Hint for LLM on when to invoke this skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    /// Formal argument definitions for the skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Vec<SkillArgumentDef>>,
    /// Free-text hint shown next to the skill name in listings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
    /// Whether the user can invoke this skill via `/skill-name` (default: true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_invocable: Option<bool>,
    /// If true, this skill is hidden from the LLM catalog (default: false).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_model_invocation: Option<bool>,
    /// Override the model used when this skill is activated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Execution mode: "inline" (default) or "fork".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Comma/newline-separated glob patterns for conditional activation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<String>,
}

/// A formal argument definition for a skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillArgumentDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
}

/// Parsed SKILL.md document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillDoc {
    pub frontmatter: SkillFrontmatter,
    pub body: String,
}

/// Parsed allowed-tools token from frontmatter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowedTool {
    /// Raw token as declared in frontmatter.
    pub raw: String,
    /// Base tool id (the part before optional scope `(...)`).
    pub tool_id: String,
    /// Optional scope/selector payload inside `(...)`.
    pub scope: Option<String>,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum SkillParseError {
    #[error("missing YAML frontmatter (expected leading '---' fence)")]
    MissingFrontmatter,
    #[error("unterminated YAML frontmatter (missing closing '---' fence)")]
    UnterminatedFrontmatter,
    #[error("invalid YAML frontmatter: {0}")]
    InvalidYaml(String),
    #[error("invalid frontmatter: {0}")]
    InvalidFrontmatter(String),
}

/// Parse a SKILL.md (frontmatter + markdown body).
pub fn parse_skill_md(input: &str) -> Result<SkillDoc, SkillParseError> {
    let input = input.replace("\r\n", "\n");
    if !input.starts_with("---\n") {
        return Err(SkillParseError::MissingFrontmatter);
    }

    let mut lines = input.split('\n');
    let first = lines.next().unwrap_or_default();
    if first != "---" {
        return Err(SkillParseError::MissingFrontmatter);
    }

    let mut fm_lines = Vec::new();
    let mut found_end = false;
    for line in &mut lines {
        if line == "---" {
            found_end = true;
            break;
        }
        fm_lines.push(line);
    }

    if !found_end {
        return Err(SkillParseError::UnterminatedFrontmatter);
    }

    let fm_str = fm_lines.join("\n");
    let mut frontmatter = serde_yaml::from_str::<SkillFrontmatter>(&fm_str)
        .map_err(|e| SkillParseError::InvalidYaml(e.to_string()))?;
    frontmatter.name = normalize_skill_name(&frontmatter.name);
    validate_frontmatter(&frontmatter)?;
    let body = lines.collect::<Vec<_>>().join("\n");

    Ok(SkillDoc { frontmatter, body })
}

/// Parse the `allowed-tools` frontmatter field into ordered tokens.
pub fn parse_allowed_tools(value: &str) -> Result<Vec<AllowedTool>, SkillParseError> {
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
                    return Err(SkillParseError::InvalidFrontmatter(
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
        return Err(SkillParseError::InvalidFrontmatter(
            "allowed-tools contains unterminated quote".to_string(),
        ));
    }
    if paren_depth != 0 {
        return Err(SkillParseError::InvalidFrontmatter(
            "allowed-tools contains unbalanced parentheses".to_string(),
        ));
    }

    let t = current.trim();
    if !t.is_empty() {
        tokens.push(t.to_string());
    }

    tokens
        .into_iter()
        .map(parse_allowed_tool_token)
        .collect::<Result<Vec<_>, _>>()
}

/// Parse one allowed-tools token.
pub fn parse_allowed_tool_token(token: String) -> Result<AllowedTool, SkillParseError> {
    let raw = token.trim().to_string();
    if raw.is_empty() {
        return Err(SkillParseError::InvalidFrontmatter(
            "allowed-tools contains an empty token".to_string(),
        ));
    }

    let (tool_id, scope) = if let Some(open_idx) = raw.find('(') {
        if !raw.ends_with(')') {
            return Err(SkillParseError::InvalidFrontmatter(format!(
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
        return Err(SkillParseError::InvalidFrontmatter(format!(
            "invalid allowed-tools token '{raw}'"
        )));
    }

    if tool_id
        .chars()
        .any(|c| c.is_whitespace() || c == '(' || c == ')')
    {
        return Err(SkillParseError::InvalidFrontmatter(format!(
            "invalid tool id in allowed-tools token '{raw}'"
        )));
    }

    Ok(AllowedTool {
        raw,
        tool_id,
        scope,
    })
}

fn normalize_skill_name(name: &str) -> String {
    name.trim().nfkc().collect::<String>()
}

fn deserialize_metadata<'de, D>(
    deserializer: D,
) -> Result<Option<HashMap<String, String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Option<HashMap<String, serde_yaml::Value>> =
        Option::<HashMap<String, serde_yaml::Value>>::deserialize(deserializer)?;
    let Some(raw) = raw else { return Ok(None) };

    let mut out = HashMap::with_capacity(raw.len());
    for (k, v) in raw {
        let s = match v {
            serde_yaml::Value::String(s) => s,
            serde_yaml::Value::Null
            | serde_yaml::Value::Bool(_)
            | serde_yaml::Value::Number(_)
            | serde_yaml::Value::Sequence(_)
            | serde_yaml::Value::Mapping(_) => {
                return Err(serde::de::Error::custom(format!(
                    "metadata value for key '{k}' must be a string"
                )));
            }
            serde_yaml::Value::Tagged(_) => {
                return Err(serde::de::Error::custom(format!(
                    "metadata value for key '{k}' must not be tagged"
                )));
            }
        };
        out.insert(k, s);
    }
    Ok(Some(out))
}

fn validate_frontmatter(fm: &SkillFrontmatter) -> Result<(), SkillParseError> {
    validate_skill_name(&fm.name)?;

    let desc = fm.description.trim();
    if desc.is_empty() {
        return Err(SkillParseError::InvalidFrontmatter(
            "description must be non-empty".to_string(),
        ));
    }
    if desc.chars().count() > 1024 {
        return Err(SkillParseError::InvalidFrontmatter(
            "description must be <= 1024 characters".to_string(),
        ));
    }

    if let Some(c) = &fm.compatibility {
        let c = c.trim();
        if c.is_empty() {
            return Err(SkillParseError::InvalidFrontmatter(
                "compatibility must be non-empty if provided".to_string(),
            ));
        }
        if c.chars().count() > 500 {
            return Err(SkillParseError::InvalidFrontmatter(
                "compatibility must be <= 500 characters".to_string(),
            ));
        }
    }

    if let Some(allowed) = fm.allowed_tools.as_deref() {
        let _ = parse_allowed_tools(allowed)?;
    }

    if let Some(ctx) = &fm.context {
        let ctx = ctx.trim();
        if ctx != "inline" && ctx != "fork" {
            return Err(SkillParseError::InvalidFrontmatter(
                "context must be \"inline\" or \"fork\"".to_string(),
            ));
        }
    }

    if fm
        .when_to_use
        .as_deref()
        .is_some_and(|w| w.trim().is_empty())
    {
        return Err(SkillParseError::InvalidFrontmatter(
            "when-to-use must be non-empty if provided".to_string(),
        ));
    }

    if let Some(args) = &fm.arguments {
        for arg in args {
            if arg.name.trim().is_empty() {
                return Err(SkillParseError::InvalidFrontmatter(
                    "argument name must be non-empty".to_string(),
                ));
            }
        }
    }

    Ok(())
}

fn validate_skill_name(name: &str) -> Result<(), SkillParseError> {
    let n = normalize_skill_name(name);
    if n.is_empty() {
        return Err(SkillParseError::InvalidFrontmatter(
            "name must be non-empty".to_string(),
        ));
    }
    let len = n.chars().count();
    if len > 64 {
        return Err(SkillParseError::InvalidFrontmatter(
            "name must be <= 64 characters".to_string(),
        ));
    }
    if n != n.to_lowercase() {
        return Err(SkillParseError::InvalidFrontmatter(
            "name must be lowercase".to_string(),
        ));
    }
    if n.starts_with('-') {
        return Err(SkillParseError::InvalidFrontmatter(
            "name must not start with '-'".to_string(),
        ));
    }
    if n.ends_with('-') {
        return Err(SkillParseError::InvalidFrontmatter(
            "name must not end with '-'".to_string(),
        ));
    }
    if n.contains("--") {
        return Err(SkillParseError::InvalidFrontmatter(
            "name must not contain consecutive '-'".to_string(),
        ));
    }
    if !n.chars().all(|c| c.is_alphanumeric() || c == '-') {
        return Err(SkillParseError::InvalidFrontmatter(
            "name contains invalid characters (only letters, digits, and '-' are allowed)"
                .to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_skill_md_requires_frontmatter() {
        let err = parse_skill_md("# Title\nBody").unwrap_err().to_string();
        assert!(err.contains("missing YAML frontmatter"));
    }

    #[test]
    fn parse_skill_md_with_frontmatter() {
        let input = r#"---
name: docx-processing
description: Test
allowed-tools: read_file other_tool(arg=1)
---
# Title
Body
"#;
        let doc = parse_skill_md(input).unwrap();
        assert_eq!(doc.frontmatter.name, "docx-processing");
        assert_eq!(doc.frontmatter.description, "Test");
        assert_eq!(
            doc.frontmatter.allowed_tools.as_deref(),
            Some("read_file other_tool(arg=1)")
        );
        assert!(doc.body.starts_with("# Title"));
    }

    #[test]
    fn parse_skill_md_invalid_yaml_is_error() {
        let input = r#"---
name: [unterminated
---
Body
"#;
        let err = parse_skill_md(input).unwrap_err().to_string();
        assert!(err.contains("invalid YAML"));
    }

    #[test]
    fn parse_skill_md_invalid_name_is_error() {
        let input = r#"---
name: Docx Processing
description: Test
---
Body
"#;
        let err = parse_skill_md(input).unwrap_err().to_string();
        assert!(err.contains("lowercase") || err.contains("invalid characters"));
    }

    #[test]
    fn parse_skill_md_allows_i18n_names_like_chinese() {
        let input = "---\nname: \u{6280}\u{80fd}\ndescription: ok\n---\nBody\n";
        let doc = parse_skill_md(input).unwrap();
        assert_eq!(doc.frontmatter.name, "\u{6280}\u{80fd}");
    }

    #[test]
    fn parse_skill_md_rejects_i18n_uppercase_names() {
        let input =
            "---\nname: \u{041D}\u{0410}\u{0412}\u{042B}\u{041A}\ndescription: ok\n---\nBody\n";
        let err = parse_skill_md(input).unwrap_err().to_string();
        assert!(err.contains("lowercase"));
    }

    #[test]
    fn parse_skill_md_accepts_unknown_frontmatter_keys() {
        // Unknown keys are silently ignored for forward compatibility (ADR-0020).
        let input = r#"---
name: good-skill
description: ok
extra: no
---
Body
"#;
        let doc = parse_skill_md(input).unwrap();
        assert_eq!(doc.frontmatter.name, "good-skill");
    }

    #[test]
    fn parse_skill_md_allows_non_string_metadata_values() {
        let input = "---\nname: good-skill\ndescription: ok\nmetadata:\n  k: 1\n---\nBody\n";
        let err = parse_skill_md(input).unwrap_err().to_string();
        assert!(err.contains("metadata value for key 'k' must be a string"));
    }

    #[test]
    fn parse_skill_md_rejects_non_mapping_metadata() {
        let input = "---\nname: good-skill\ndescription: ok\nmetadata: 1\n---\nBody\n";
        let err = parse_skill_md(input).unwrap_err().to_string();
        assert!(err.contains("invalid type") || err.contains("Invalid"));
    }

    #[test]
    fn parse_skill_md_rejects_blank_description() {
        let input = "---\nname: good-skill\ndescription: \"   \"\n---\nBody\n";
        let err = parse_skill_md(input).unwrap_err().to_string();
        assert!(err.contains("description must be non-empty"));
    }

    #[test]
    fn parse_skill_md_rejects_overlong_description() {
        let long = "a".repeat(1025);
        let input = format!(
            "---\nname: good-skill\ndescription: \"{}\"\n---\nBody\n",
            long
        );
        let err = parse_skill_md(&input).unwrap_err().to_string();
        assert!(err.contains("description must be <= 1024"));
    }

    #[test]
    fn parse_skill_md_rejects_empty_compatibility() {
        let input = "---\nname: good-skill\ndescription: ok\ncompatibility: \"  \"\n---\nBody\n";
        let err = parse_skill_md(input).unwrap_err().to_string();
        assert!(err.contains("compatibility must be non-empty"));
    }

    #[test]
    fn parse_skill_md_allows_crlf_newlines() {
        let input = "---\r\nname: good-skill\r\ndescription: ok\r\n---\r\nBody\r\n";
        let doc = parse_skill_md(input).unwrap();
        assert_eq!(doc.frontmatter.name, "good-skill");
        assert_eq!(doc.frontmatter.description, "ok");
        assert_eq!(doc.body.trim(), "Body");
    }

    #[test]
    fn parse_skill_md_allows_string_metadata_values() {
        let input =
            "---\nname: good-skill\ndescription: ok\nmetadata:\n  env: production\n---\nBody\n";
        let doc = parse_skill_md(input).unwrap();
        let meta = doc.frontmatter.metadata.expect("metadata present");
        assert_eq!(meta.get("env").map(String::as_str), Some("production"));
    }

    #[test]
    fn parse_allowed_tools_preserves_scoped_token_with_spaces() {
        let parsed = parse_allowed_tools(r#"read_file Bash(command: "git status")"#)
            .expect("allowed-tools should parse");

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].raw, "read_file");
        assert_eq!(parsed[0].tool_id, "read_file");
        assert!(parsed[0].scope.is_none());

        assert_eq!(parsed[1].raw, r#"Bash(command: "git status")"#);
        assert_eq!(parsed[1].tool_id, "Bash");
        assert_eq!(parsed[1].scope.as_deref(), Some(r#"command: "git status""#));
    }

    #[test]
    fn parse_skill_md_unterminated_frontmatter() {
        let input = "---\nname: good-skill\ndescription: ok\n";
        let err = parse_skill_md(input).unwrap_err();
        assert!(matches!(err, SkillParseError::UnterminatedFrontmatter));
    }

    #[test]
    fn parse_skill_md_rejects_name_starting_with_dash() {
        let input = "---\nname: -bad\ndescription: ok\n---\nBody\n";
        let err = parse_skill_md(input).unwrap_err().to_string();
        assert!(err.contains("must not start with '-'"));
    }

    #[test]
    fn parse_skill_md_rejects_name_ending_with_dash() {
        let input = "---\nname: bad-\ndescription: ok\n---\nBody\n";
        let err = parse_skill_md(input).unwrap_err().to_string();
        assert!(err.contains("must not end with '-'"));
    }

    #[test]
    fn parse_skill_md_rejects_name_with_consecutive_dashes() {
        let input = "---\nname: bad--name\ndescription: ok\n---\nBody\n";
        let err = parse_skill_md(input).unwrap_err().to_string();
        assert!(err.contains("consecutive '-'"));
    }

    #[test]
    fn parse_skill_md_rejects_overlong_name() {
        let long_name: String = "a".repeat(65);
        let input = format!("---\nname: {}\ndescription: ok\n---\nBody\n", long_name);
        let err = parse_skill_md(&input).unwrap_err().to_string();
        assert!(err.contains("64 characters"));
    }

    #[test]
    fn parse_skill_md_rejects_empty_name() {
        let input = "---\nname: \"\"\ndescription: ok\n---\nBody\n";
        let err = parse_skill_md(input).unwrap_err().to_string();
        assert!(err.contains("name must be non-empty"));
    }

    #[test]
    fn parse_skill_md_rejects_overlong_compatibility() {
        let long_compat: String = "a".repeat(501);
        let input = format!(
            "---\nname: good-skill\ndescription: ok\ncompatibility: \"{}\"\n---\nBody\n",
            long_compat
        );
        let err = parse_skill_md(&input).unwrap_err().to_string();
        assert!(err.contains("compatibility must be <= 500"));
    }

    #[test]
    fn parse_skill_md_accepts_valid_compatibility() {
        let input = "---\nname: good-skill\ndescription: ok\ncompatibility: claude\n---\nBody\n";
        let doc = parse_skill_md(input).unwrap();
        assert_eq!(doc.frontmatter.compatibility.as_deref(), Some("claude"));
    }

    #[test]
    fn parse_skill_md_accepts_license() {
        let input = "---\nname: good-skill\ndescription: ok\nlicense: MIT\n---\nBody\n";
        let doc = parse_skill_md(input).unwrap();
        assert_eq!(doc.frontmatter.license.as_deref(), Some("MIT"));
    }

    #[test]
    fn parse_allowed_tools_empty_string() {
        let result = parse_allowed_tools("").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn parse_allowed_tools_whitespace_only() {
        let result = parse_allowed_tools("   ").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn parse_allowed_tools_unmatched_close_paren() {
        let err = parse_allowed_tools("tool)").unwrap_err();
        assert!(err.to_string().contains("unmatched ')'"));
    }

    #[test]
    fn parse_allowed_tools_unterminated_quote() {
        let err = parse_allowed_tools(r#"Bash("unclosed)"#).unwrap_err();
        // paren inside quote doesn't count, so quote is unterminated
        assert!(err.to_string().contains("unterminated quote"));
    }

    #[test]
    fn parse_allowed_tools_unbalanced_parens() {
        let err = parse_allowed_tools("Bash(git status").unwrap_err();
        assert!(err.to_string().contains("unbalanced parentheses"));
    }

    #[test]
    fn parse_allowed_tool_token_rejects_empty_tool_id() {
        let err = parse_allowed_tool_token("(scope)".to_string()).unwrap_err();
        assert!(err.to_string().contains("invalid allowed-tools token"));
    }

    #[test]
    fn parse_allowed_tool_token_rejects_whitespace_in_id() {
        let err = parse_allowed_tool_token("ba sh(scope)".to_string());
        // This actually gets tokenized as two tokens by parse_allowed_tools,
        // but parse_allowed_tool_token on the raw string checks for whitespace in tool_id.
        // "ba sh(scope)" has whitespace but also unbalanced parens in the id portion.
        assert!(err.is_err());
    }

    #[test]
    fn parse_allowed_tool_token_rejects_unclosed_scope() {
        let err = parse_allowed_tool_token("Bash(git".to_string()).unwrap_err();
        assert!(err.to_string().contains("invalid allowed-tools token"));
    }

    #[test]
    fn parse_allowed_tools_multiple_tokens_with_nested_parens() {
        let result = parse_allowed_tools("Read Edit(path(nested))").unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].tool_id, "Read");
        assert_eq!(result[1].tool_id, "Edit");
        assert_eq!(result[1].scope.as_deref(), Some("path(nested)"));
    }

    #[test]
    fn parse_allowed_tools_escaped_char_in_quote() {
        let result = parse_allowed_tools(r#"Bash(command: "git \"status\"")"#).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].tool_id, "Bash");
    }

    #[test]
    fn skill_parse_error_display() {
        let err = SkillParseError::MissingFrontmatter;
        assert_eq!(
            err.to_string(),
            "missing YAML frontmatter (expected leading '---' fence)"
        );
        let err = SkillParseError::UnterminatedFrontmatter;
        assert!(err.to_string().contains("unterminated"));
    }

    #[test]
    fn parse_skill_md_rejects_unbalanced_allowed_tools_parentheses() {
        let input = "---\nname: good-skill\ndescription: ok\nallowed-tools: read_file Bash(git-status\n---\nBody\n";
        let err = parse_skill_md(input).unwrap_err().to_string();
        assert!(err.contains("allowed-tools"));
    }

    // --- New visibility/invocation fields ---

    #[test]
    fn parse_skill_md_with_when_to_use() {
        let input = "---\nname: good-skill\ndescription: ok\nwhen-to-use: when editing React components\n---\nBody\n";
        let doc = parse_skill_md(input).unwrap();
        assert_eq!(
            doc.frontmatter.when_to_use.as_deref(),
            Some("when editing React components")
        );
    }

    #[test]
    fn parse_skill_md_rejects_blank_when_to_use() {
        let input = "---\nname: good-skill\ndescription: ok\nwhen-to-use: \"  \"\n---\nBody\n";
        let err = parse_skill_md(input).unwrap_err().to_string();
        assert!(err.contains("when-to-use must be non-empty"));
    }

    #[test]
    fn parse_skill_md_with_arguments() {
        let input = r#"---
name: good-skill
description: ok
arguments:
  - name: file
    description: Target file
    required: true
  - name: mode
---
Body
"#;
        let doc = parse_skill_md(input).unwrap();
        let args = doc.frontmatter.arguments.unwrap();
        assert_eq!(args.len(), 2);
        assert_eq!(args[0].name, "file");
        assert_eq!(args[0].description.as_deref(), Some("Target file"));
        assert!(args[0].required);
        assert_eq!(args[1].name, "mode");
        assert!(!args[1].required);
    }

    #[test]
    fn parse_skill_md_rejects_empty_argument_name() {
        let input =
            "---\nname: good-skill\ndescription: ok\narguments:\n  - name: \"\"\n---\nBody\n";
        let err = parse_skill_md(input).unwrap_err().to_string();
        assert!(err.contains("argument name must be non-empty"));
    }

    #[test]
    fn parse_skill_md_with_disable_model_invocation() {
        let input =
            "---\nname: good-skill\ndescription: ok\ndisable-model-invocation: true\n---\nBody\n";
        let doc = parse_skill_md(input).unwrap();
        assert_eq!(doc.frontmatter.disable_model_invocation, Some(true));
    }

    #[test]
    fn parse_skill_md_with_user_invocable_false() {
        let input = "---\nname: good-skill\ndescription: ok\nuser-invocable: false\n---\nBody\n";
        let doc = parse_skill_md(input).unwrap();
        assert_eq!(doc.frontmatter.user_invocable, Some(false));
    }

    #[test]
    fn parse_skill_md_with_context_inline() {
        let input = "---\nname: good-skill\ndescription: ok\ncontext: inline\n---\nBody\n";
        let doc = parse_skill_md(input).unwrap();
        assert_eq!(doc.frontmatter.context.as_deref(), Some("inline"));
    }

    #[test]
    fn parse_skill_md_with_context_fork() {
        let input = "---\nname: good-skill\ndescription: ok\ncontext: fork\n---\nBody\n";
        let doc = parse_skill_md(input).unwrap();
        assert_eq!(doc.frontmatter.context.as_deref(), Some("fork"));
    }

    #[test]
    fn parse_skill_md_rejects_invalid_context() {
        let input = "---\nname: good-skill\ndescription: ok\ncontext: parallel\n---\nBody\n";
        let err = parse_skill_md(input).unwrap_err().to_string();
        assert!(err.contains("context must be"));
    }

    #[test]
    fn parse_skill_md_with_paths() {
        let input =
            "---\nname: good-skill\ndescription: ok\npaths: \"*.tsx, src/**/*.ts\"\n---\nBody\n";
        let doc = parse_skill_md(input).unwrap();
        assert_eq!(doc.frontmatter.paths.as_deref(), Some("*.tsx, src/**/*.ts"));
    }

    #[test]
    fn parse_skill_md_with_model_override() {
        let input = "---\nname: good-skill\ndescription: ok\nmodel: claude-sonnet-4-6\n---\nBody\n";
        let doc = parse_skill_md(input).unwrap();
        assert_eq!(doc.frontmatter.model.as_deref(), Some("claude-sonnet-4-6"));
    }

    #[test]
    fn parse_skill_md_with_argument_hint() {
        let input =
            "---\nname: good-skill\ndescription: ok\nargument-hint: \"<file> [mode]\"\n---\nBody\n";
        let doc = parse_skill_md(input).unwrap();
        assert_eq!(
            doc.frontmatter.argument_hint.as_deref(),
            Some("<file> [mode]")
        );
    }
}
