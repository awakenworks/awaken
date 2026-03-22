//! Configuration loading for reminder rules.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use awaken_contract::contract::context_message::ContextMessage;
use awaken_contract::tool_pattern::{
    ArgMatcher, FieldCondition, MatchOp, PathSegment, ToolCallPattern, ToolMatcher, parse_pattern,
};

use crate::output_matcher::{ContentMatcher, OutputMatcher, ToolStatusMatcher};
use crate::rule::ReminderRule;

/// Top-level reminder configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReminderConfig {
    pub reminders: Vec<ReminderRuleEntry>,
}

/// A single reminder rule entry in the config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReminderRuleEntry {
    pub name: String,
    pub pattern: PatternEntry,
    pub output: OutputEntry,
    pub message: MessageEntry,
}

/// Tool call pattern entry in config (JSON-friendly).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PatternEntry {
    /// Simple string pattern: "Bash(npm *)" or "*"
    Simple(String),
    /// Structured pattern with tool + args
    Structured {
        tool: String,
        #[serde(default)]
        args: Option<Value>,
    },
}

/// Output matcher entry in config.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OutputEntry {
    /// Simple string "any"
    Simple(String),
    /// Structured output matcher
    Structured {
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        content: Option<ContentEntry>,
    },
}

/// Content matcher entry in config.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ContentEntry {
    /// Text matcher: {"text": "*pattern*"}
    Text { text: String },
    /// JSON fields matcher: {"fields": [{"path": "status", "op": "exact", "value": "ok"}]}
    Fields { fields: Vec<FieldEntry> },
}

/// Single field entry for JSON field matching.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageEntry {
    pub target: String,
    pub content: String,
    #[serde(default)]
    pub cooldown_turns: u32,
}

/// Error type for config loading.
#[derive(Debug, thiserror::Error)]
pub enum ReminderConfigError {
    #[error("parse error: {0}")]
    Parse(String),
    #[error("invalid pattern `{pattern}`: {reason}")]
    InvalidPattern { pattern: String, reason: String },
    #[error("invalid output matcher: {0}")]
    InvalidOutput(String),
    #[error("invalid message target: {0}")]
    InvalidTarget(String),
    #[error("invalid match op: {0}")]
    InvalidOp(String),
}

impl ReminderConfig {
    /// Parse configuration from a JSON string.
    pub fn from_json(json: &str) -> Result<Self, ReminderConfigError> {
        serde_json::from_str(json).map_err(|e| ReminderConfigError::Parse(e.to_string()))
    }

    /// Convert this configuration into a list of [`ReminderRule`]s.
    pub fn into_rules(self) -> Result<Vec<ReminderRule>, ReminderConfigError> {
        self.reminders
            .into_iter()
            .map(|entry| entry_to_rule(entry))
            .collect()
    }
}

fn entry_to_rule(entry: ReminderRuleEntry) -> Result<ReminderRule, ReminderConfigError> {
    let pattern = parse_pattern_entry(&entry.pattern)?;
    let output = parse_output_entry(&entry.output)?;
    let message = parse_message_entry(&entry.message, &entry.name)?;

    Ok(ReminderRule {
        name: entry.name,
        pattern,
        output,
        message,
    })
}

fn parse_pattern_entry(entry: &PatternEntry) -> Result<ToolCallPattern, ReminderConfigError> {
    match entry {
        PatternEntry::Simple(s) => {
            parse_pattern(s).map_err(|e| ReminderConfigError::InvalidPattern {
                pattern: s.clone(),
                reason: e.to_string(),
            })
        }
        PatternEntry::Structured { tool, args } => {
            let tool_matcher = if tool == "*" {
                ToolMatcher::Glob("*".into())
            } else if tool.contains('*') || tool.contains('?') || tool.contains('[') {
                ToolMatcher::Glob(tool.clone())
            } else {
                ToolMatcher::Exact(tool.clone())
            };

            let arg_matcher = match args {
                None => ArgMatcher::Any,
                Some(Value::Object(obj)) => {
                    let conditions: Vec<FieldCondition> = obj
                        .iter()
                        .map(|(key, val)| {
                            let value_str = match val {
                                Value::String(s) => s.clone(),
                                other => other.to_string(),
                            };
                            FieldCondition {
                                path: vec![PathSegment::Field(key.clone())],
                                op: MatchOp::Glob,
                                value: value_str,
                            }
                        })
                        .collect();
                    if conditions.is_empty() {
                        ArgMatcher::Any
                    } else {
                        ArgMatcher::Fields(conditions)
                    }
                }
                _ => ArgMatcher::Any,
            };

            Ok(ToolCallPattern {
                tool: tool_matcher,
                args: arg_matcher,
            })
        }
    }
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
        ContentEntry::Text { text } => Ok(ContentMatcher::Text {
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
            "reminders": [
                {
                    "name": "deletion-warning",
                    "pattern": { "tool": "Bash", "args": { "command": "rm *" } },
                    "output": { "status": "success" },
                    "message": {
                        "target": "suffix_system",
                        "content": "Just executed a deletion"
                    }
                }
            ]
        }"#;

        let config = ReminderConfig::from_json(json).unwrap();
        assert_eq!(config.reminders.len(), 1);
        assert_eq!(config.reminders[0].name, "deletion-warning");
    }

    #[test]
    fn config_into_rules() {
        let json = r#"{
            "reminders": [
                {
                    "name": "test-rule",
                    "pattern": "*",
                    "output": "any",
                    "message": {
                        "target": "system",
                        "content": "Remember to be careful"
                    }
                }
            ]
        }"#;

        let config = ReminderConfig::from_json(json).unwrap();
        let rules = config.into_rules().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].name, "test-rule");
    }

    #[test]
    fn config_with_cooldown() {
        let json = r#"{
            "reminders": [
                {
                    "name": "config-check",
                    "pattern": { "tool": "Edit", "args": { "file_path": "*.toml" } },
                    "output": "any",
                    "message": {
                        "target": "system",
                        "content": "Remember to cargo check",
                        "cooldown_turns": 3
                    }
                }
            ]
        }"#;

        let config = ReminderConfig::from_json(json).unwrap();
        let rules = config.into_rules().unwrap();
        assert_eq!(rules[0].message.cooldown_turns, 3);
    }

    #[test]
    fn config_with_error_status_and_content() {
        let json = r#"{
            "reminders": [
                {
                    "name": "error-guidance",
                    "pattern": "*",
                    "output": {
                        "status": "error",
                        "content": { "text": "*permission denied*" }
                    },
                    "message": {
                        "target": "suffix_system",
                        "content": "Consider using sudo"
                    }
                }
            ]
        }"#;

        let config = ReminderConfig::from_json(json).unwrap();
        let rules = config.into_rules().unwrap();
        assert_eq!(rules.len(), 1);
    }

    #[test]
    fn config_invalid_json() {
        let result = ReminderConfig::from_json("not json");
        assert!(result.is_err());
    }

    #[test]
    fn config_invalid_target() {
        let json = r#"{
            "reminders": [
                {
                    "name": "bad",
                    "pattern": "*",
                    "output": "any",
                    "message": {
                        "target": "invalid_target",
                        "content": "text"
                    }
                }
            ]
        }"#;

        let config = ReminderConfig::from_json(json).unwrap();
        let result = config.into_rules();
        assert!(result.is_err());
    }

    #[test]
    fn config_structured_pattern_glob_tool() {
        let json = r#"{
            "reminders": [
                {
                    "name": "mcp-watch",
                    "pattern": { "tool": "mcp__*" },
                    "output": "any",
                    "message": {
                        "target": "system",
                        "content": "MCP tool used"
                    }
                }
            ]
        }"#;

        let config = ReminderConfig::from_json(json).unwrap();
        let rules = config.into_rules().unwrap();
        assert!(matches!(rules[0].pattern.tool, ToolMatcher::Glob(_)));
    }

    #[test]
    fn config_with_json_fields_content() {
        let json = r#"{
            "reminders": [
                {
                    "name": "field-match",
                    "pattern": "*",
                    "output": {
                        "content": {
                            "fields": [
                                { "path": "error.code", "op": "exact", "value": "PERM_DENIED" }
                            ]
                        }
                    },
                    "message": {
                        "target": "suffix_system",
                        "content": "Permission error detected"
                    }
                }
            ]
        }"#;

        let config = ReminderConfig::from_json(json).unwrap();
        let rules = config.into_rules().unwrap();
        assert_eq!(rules.len(), 1);
    }
}
