//! Configuration loading for reminder rules.
//!
//! Loads declarative reminder rules from a configuration file or inline
//! value and integrates with [`PluginConfigKey`] for agent spec configuration.
//!
//! # Example JSON
//!
//! ```json
//! {
//!   "rules": [
//!     {
//!       "tool": "Bash(command ~ 'rm *')",
//!       "output": { "status": "success" },
//!       "message": { "target": "suffix_system", "content": "Just executed a deletion" }
//!     }
//!   ]
//! }
//! ```

use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use awaken_contract::PluginConfigKey;
use awaken_contract::config_loader::{
    ConfigLoadError, load_config_from_file, load_config_from_str,
};
use awaken_contract::contract::context_message::ContextMessage;
use awaken_tool_pattern::{FieldCondition, MatchOp, PathSegment, parse_pattern};

use crate::output_matcher::{ContentMatcher, OutputMatcher, ToolStatusMatcher};
use crate::rule::ReminderRule;

// ---------------------------------------------------------------------------
// Config types
// ---------------------------------------------------------------------------

/// Top-level reminder rules configuration.
///
/// Can be loaded from JSON or provided inline via agent spec.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ReminderRulesConfig {
    /// Ordered list of reminder rules.
    pub rules: Vec<ReminderRuleEntry>,
}

/// A single reminder rule entry in the config.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReminderRuleEntry {
    /// Optional human-readable name (auto-generated from tool pattern if missing).
    #[serde(default)]
    pub name: Option<String>,
    /// Tool name or pattern string using the same DSL as permission
    /// (e.g. `"Bash"`, `"*"`, `"Bash(command ~ 'rm *')"`, `"Edit(file_path ~ '*.toml')"`)
    pub tool: String,
    /// Output matcher: `"any"` or structured `{ status?, content? }`.
    #[serde(default = "default_output")]
    pub output: OutputEntry,
    /// The message to inject when the rule matches.
    pub message: MessageEntry,
}

fn default_output() -> OutputEntry {
    OutputEntry::Simple("any".to_string())
}

/// Output matcher entry in config.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum OutputEntry {
    /// Simple string `"any"`.
    Simple(String),
    /// Structured output matcher with optional status and content.
    Structured {
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        content: Option<ContentEntry>,
    },
}

/// Content matcher entry in config.
///
/// Can be a **string** (shorthand for text glob) or structured `{ fields: [...] }`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ContentEntry {
    /// Shorthand text glob: `"*permission denied*"`.
    TextGlob(String),
    /// JSON fields matcher: `{ "fields": [{ "path": "error.code", "op": "exact", "value": "403" }] }`.
    Fields { fields: Vec<FieldEntry> },
}

/// Single field entry for JSON field matching.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FieldEntry {
    pub path: String,
    #[serde(default = "default_op")]
    pub op: String,
    pub value: String,
}

fn default_op() -> String {
    "glob".to_string()
}

/// Message entry in config.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MessageEntry {
    pub target: String,
    pub content: String,
    #[serde(default)]
    pub cooldown_turns: u32,
}

/// [`PluginConfigKey`] binding for reminder configuration in agent specs.
///
/// ```ignore
/// let config = spec.config::<ReminderConfigKey>();
/// ```
pub struct ReminderConfigKey;

impl PluginConfigKey for ReminderConfigKey {
    const KEY: &'static str = "reminder";
    type Config = ReminderRulesConfig;
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Error type for configuration loading.
#[derive(Debug, thiserror::Error)]
pub enum ReminderConfigError {
    /// File I/O or parse error from shared config loader.
    #[error(transparent)]
    Load(#[from] ConfigLoadError),
    /// Invalid pattern in a rule entry.
    #[error("invalid pattern `{pattern}`: {reason}")]
    InvalidPattern { pattern: String, reason: String },
    /// Invalid output matcher value.
    #[error("invalid output matcher: {0}")]
    InvalidOutput(String),
    /// Invalid message target.
    #[error("invalid message target: {0}")]
    InvalidTarget(String),
    /// Invalid match operation.
    #[error("invalid match op: {0}")]
    InvalidOp(String),
}

impl ReminderRulesConfig {
    /// Load configuration from a file path (JSON, detected by extension).
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ReminderConfigError> {
        Ok(load_config_from_file(path)?)
    }

    /// Parse configuration from a string with an optional format hint.
    ///
    /// If `ext` is `Some("json")`, parses as JSON; otherwise auto-detects.
    pub fn from_str(content: &str, ext: Option<&str>) -> Result<Self, ReminderConfigError> {
        Ok(load_config_from_str(content, ext)?)
    }

    /// Convert this configuration into a list of [`ReminderRule`]s.
    pub fn into_rules(self) -> Result<Vec<ReminderRule>, ReminderConfigError> {
        self.rules
            .into_iter()
            .enumerate()
            .map(|(i, entry)| entry_to_rule(entry, i))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

fn entry_to_rule(
    entry: ReminderRuleEntry,
    index: usize,
) -> Result<ReminderRule, ReminderConfigError> {
    let name = entry
        .name
        .clone()
        .unwrap_or_else(|| format!("rule-{}-{}", index, entry.tool));

    let pattern = parse_pattern(&entry.tool).map_err(|e| ReminderConfigError::InvalidPattern {
        pattern: entry.tool.clone(),
        reason: e.to_string(),
    })?;

    let output = parse_output_entry(&entry.output)?;
    let message = parse_message_entry(&entry.message, &name)?;

    Ok(ReminderRule {
        name,
        pattern,
        output,
        message,
    })
}

fn parse_output_entry(entry: &OutputEntry) -> Result<OutputMatcher, ReminderConfigError> {
    match entry {
        OutputEntry::Simple(s) if s == "any" => Ok(OutputMatcher::Any),
        OutputEntry::Simple(s) => Err(ReminderConfigError::InvalidOutput(format!(
            "unknown output matcher: '{s}', expected 'any' or structured"
        ))),
        OutputEntry::Structured { status, content } => {
            let status_matcher = status.as_deref().map(parse_status_matcher).transpose()?;
            let content_matcher = content.as_ref().map(parse_content_entry).transpose()?;

            match (status_matcher, content_matcher) {
                (None, None) => Ok(OutputMatcher::Any),
                (Some(s), None) => Ok(OutputMatcher::Status(s)),
                (None, Some(c)) => Ok(OutputMatcher::Content(c)),
                (Some(s), Some(c)) => Ok(OutputMatcher::Both {
                    status: s,
                    content: c,
                }),
            }
        }
    }
}

fn parse_status_matcher(s: &str) -> Result<ToolStatusMatcher, ReminderConfigError> {
    match s {
        "success" => Ok(ToolStatusMatcher::Success),
        "error" => Ok(ToolStatusMatcher::Error),
        "pending" => Ok(ToolStatusMatcher::Pending),
        "any" => Ok(ToolStatusMatcher::Any),
        other => Err(ReminderConfigError::InvalidOutput(format!(
            "unknown status: '{other}'"
        ))),
    }
}

fn parse_content_entry(entry: &ContentEntry) -> Result<ContentMatcher, ReminderConfigError> {
    match entry {
        ContentEntry::TextGlob(text) => Ok(ContentMatcher::Text {
            op: MatchOp::Glob,
            value: text.clone(),
        }),
        ContentEntry::Fields { fields } => {
            let conditions = fields
                .iter()
                .map(|f| {
                    let op = parse_match_op(&f.op)?;
                    let path = f
                        .path
                        .split('.')
                        .map(|seg| PathSegment::Field(seg.to_string()))
                        .collect();
                    Ok(FieldCondition {
                        path,
                        op,
                        value: f.value.clone(),
                    })
                })
                .collect::<Result<Vec<_>, ReminderConfigError>>()?;
            Ok(ContentMatcher::JsonFields(conditions))
        }
    }
}

fn parse_match_op(s: &str) -> Result<MatchOp, ReminderConfigError> {
    match s {
        "glob" => Ok(MatchOp::Glob),
        "exact" => Ok(MatchOp::Exact),
        "regex" => Ok(MatchOp::Regex),
        "not_glob" => Ok(MatchOp::NotGlob),
        "not_exact" => Ok(MatchOp::NotExact),
        "not_regex" => Ok(MatchOp::NotRegex),
        other => Err(ReminderConfigError::InvalidOp(other.to_string())),
    }
}

fn parse_message_entry(
    entry: &MessageEntry,
    rule_name: &str,
) -> Result<ContextMessage, ReminderConfigError> {
    let key = format!("reminder.{rule_name}");
    let msg = match entry.target.as_str() {
        "system" => ContextMessage::system(&key, &entry.content),
        "suffix_system" => ContextMessage::suffix_system(&key, &entry.content),
        "session" => ContextMessage::session(
            &key,
            awaken_contract::contract::message::Role::System,
            &entry.content,
        ),
        "conversation" => ContextMessage::conversation(
            &key,
            awaken_contract::contract::message::Role::System,
            &entry.content,
        ),
        other => {
            return Err(ReminderConfigError::InvalidTarget(other.to_string()));
        }
    };
    Ok(msg.with_cooldown(entry.cooldown_turns))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_config() {
        let json = r#"{
            "rules": [
                {
                    "tool": "Bash(command ~ \"rm *\")",
                    "output": { "status": "success" },
                    "message": {
                        "target": "suffix_system",
                        "content": "Just executed a deletion"
                    }
                }
            ]
        }"#;

        let config = ReminderRulesConfig::from_str(json, Some("json")).unwrap();
        assert_eq!(config.rules.len(), 1);
        assert_eq!(config.rules[0].tool, "Bash(command ~ \"rm *\")");
    }

    #[test]
    fn config_into_rules() {
        let json = r#"{
            "rules": [
                {
                    "tool": "*",
                    "output": "any",
                    "message": {
                        "target": "system",
                        "content": "Remember to be careful"
                    }
                }
            ]
        }"#;

        let config = ReminderRulesConfig::from_str(json, None).unwrap();
        let rules = config.into_rules().unwrap();
        assert_eq!(rules.len(), 1);
        // Auto-generated name
        assert!(rules[0].name.contains("*"));
    }

    #[test]
    fn config_with_explicit_name() {
        let json = r#"{
            "rules": [
                {
                    "name": "my-rule",
                    "tool": "*",
                    "output": "any",
                    "message": {
                        "target": "system",
                        "content": "test"
                    }
                }
            ]
        }"#;

        let config = ReminderRulesConfig::from_str(json, None).unwrap();
        let rules = config.into_rules().unwrap();
        assert_eq!(rules[0].name, "my-rule");
    }

    #[test]
    fn config_with_cooldown() {
        let json = r#"{
            "rules": [
                {
                    "tool": "Edit(file_path ~ \"*.toml\")",
                    "output": "any",
                    "message": {
                        "target": "system",
                        "content": "Remember to cargo check",
                        "cooldown_turns": 3
                    }
                }
            ]
        }"#;

        let config = ReminderRulesConfig::from_str(json, None).unwrap();
        let rules = config.into_rules().unwrap();
        assert_eq!(rules[0].message.cooldown_turns, 3);
    }

    #[test]
    fn config_with_error_status_and_text_content() {
        let json = r#"{
            "rules": [
                {
                    "tool": "*",
                    "output": {
                        "status": "error",
                        "content": "*permission denied*"
                    },
                    "message": {
                        "target": "suffix_system",
                        "content": "Consider using sudo"
                    }
                }
            ]
        }"#;

        let config = ReminderRulesConfig::from_str(json, None).unwrap();
        let rules = config.into_rules().unwrap();
        assert_eq!(rules.len(), 1);
    }

    #[test]
    fn config_with_json_fields_content() {
        let json = r#"{
            "rules": [
                {
                    "tool": "*",
                    "output": {
                        "status": "error",
                        "content": {
                            "fields": [
                                { "path": "error.code", "op": "exact", "value": "403" }
                            ]
                        }
                    },
                    "message": {
                        "target": "suffix_system",
                        "content": "HTTP 403"
                    }
                }
            ]
        }"#;

        let config = ReminderRulesConfig::from_str(json, None).unwrap();
        let rules = config.into_rules().unwrap();
        assert_eq!(rules.len(), 1);
    }

    #[test]
    fn config_invalid_json() {
        let result = ReminderRulesConfig::from_str("not json", Some("json"));
        assert!(result.is_err());
    }

    #[test]
    fn config_invalid_target() {
        let json = r#"{
            "rules": [
                {
                    "tool": "*",
                    "output": "any",
                    "message": {
                        "target": "invalid_target",
                        "content": "text"
                    }
                }
            ]
        }"#;

        let config = ReminderRulesConfig::from_str(json, None).unwrap();
        let result = config.into_rules();
        assert!(result.is_err());
    }

    #[test]
    fn config_glob_tool_pattern() {
        let json = r#"{
            "rules": [
                {
                    "tool": "mcp__*",
                    "output": "any",
                    "message": {
                        "target": "system",
                        "content": "MCP tool used"
                    }
                }
            ]
        }"#;

        let config = ReminderRulesConfig::from_str(json, None).unwrap();
        let rules = config.into_rules().unwrap();
        assert!(matches!(
            rules[0].pattern.tool,
            awaken_tool_pattern::ToolMatcher::Glob(_)
        ));
    }

    #[test]
    fn config_auto_detect_json() {
        let json = r#"{"rules": []}"#;
        let config = ReminderRulesConfig::from_str(json, None).unwrap();
        assert!(config.rules.is_empty());
    }

    #[test]
    fn config_default_values() {
        let config = ReminderRulesConfig::default();
        assert!(config.rules.is_empty());
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = ReminderRulesConfig {
            rules: vec![ReminderRuleEntry {
                name: Some("test".to_string()),
                tool: "Bash".to_string(),
                output: OutputEntry::Simple("any".to_string()),
                message: MessageEntry {
                    target: "system".to_string(),
                    content: "hello".to_string(),
                    cooldown_turns: 0,
                },
            }],
        };

        let json = serde_json::to_string(&config).unwrap();
        let decoded: ReminderRulesConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.rules.len(), 1);
        assert_eq!(decoded.rules[0].name, Some("test".to_string()));
    }

    #[test]
    fn config_output_defaults_to_any() {
        let json = r#"{
            "rules": [
                {
                    "tool": "*",
                    "message": { "target": "system", "content": "test" }
                }
            ]
        }"#;

        let config = ReminderRulesConfig::from_str(json, None).unwrap();
        let rules = config.into_rules().unwrap();
        assert_eq!(rules.len(), 1);
    }

    #[test]
    fn config_error_display() {
        let err = ReminderConfigError::InvalidPattern {
            pattern: "bad[".to_string(),
            reason: "unclosed bracket".to_string(),
        };
        assert_eq!(err.to_string(), "invalid pattern `bad[`: unclosed bracket");
    }

    #[test]
    fn reminder_config_key_binding() {
        assert_eq!(ReminderConfigKey::KEY, "reminder");
    }
}
