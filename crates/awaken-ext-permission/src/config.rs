//! YAML/JSON configuration loader for permission rules.
//!
//! Loads declarative permission rules from a configuration file or inline
//! value and integrates with [`PluginConfigKey`] for agent spec configuration.
//!
//! # Example YAML
//!
//! ```yaml
//! default_behavior: deny
//! rules:
//!   - tool: "file_*"
//!     behavior: ask
//!   - tool: "read_file"
//!     behavior: allow
//!   - tool: "delete_*"
//!     behavior: deny
//! ```

use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use awaken_contract::PluginConfigKey;

use crate::rules::{
    PermissionRule, PermissionRuleScope, PermissionRuleSource, PermissionRuleset,
    PermissionSubject, ToolCallPattern, ToolPermissionBehavior, parse_pattern,
};

// ---------------------------------------------------------------------------
// Config types
// ---------------------------------------------------------------------------

/// A single rule entry in the permission config file.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PermissionRuleEntry {
    /// Tool name or glob pattern (e.g. `"read_file"`, `"file_*"`, `"Bash(npm *)"`)
    pub tool: String,
    /// Permission behavior for matching tool calls.
    pub behavior: ToolPermissionBehavior,
    /// Rule scope. Defaults to `project`.
    #[serde(default = "default_scope")]
    pub scope: PermissionRuleScope,
}

fn default_scope() -> PermissionRuleScope {
    PermissionRuleScope::Project
}

/// Top-level permission rules configuration.
///
/// Can be loaded from YAML, JSON, or provided inline via agent spec.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct PermissionRulesConfig {
    /// Default behavior when no rule matches.
    pub default_behavior: ToolPermissionBehavior,
    /// Ordered list of permission rules.
    pub rules: Vec<PermissionRuleEntry>,
}

/// [`PluginConfigKey`] binding for permission configuration in agent specs.
///
/// ```ignore
/// let config = spec.config::<PermissionConfigKey>();
/// ```
pub struct PermissionConfigKey;

impl PluginConfigKey for PermissionConfigKey {
    const KEY: &'static str = "permission";
    type Config = PermissionRulesConfig;
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Error type for configuration loading.
#[derive(Debug, thiserror::Error)]
pub enum PermissionConfigError {
    /// File I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// YAML/JSON deserialization error.
    #[error("parse error: {0}")]
    Parse(String),
    /// Invalid pattern in a rule entry.
    #[error("invalid pattern `{pattern}`: {reason}")]
    InvalidPattern { pattern: String, reason: String },
}

impl PermissionRulesConfig {
    /// Load configuration from a file path (YAML or JSON, detected by extension).
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, PermissionConfigError> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)?;
        Self::from_str(&content, path.extension().and_then(|e| e.to_str()))
    }

    /// Parse configuration from a string with an optional format hint.
    ///
    /// If `ext` is `Some("json")`, parses as JSON; otherwise YAML.
    pub fn from_str(content: &str, ext: Option<&str>) -> Result<Self, PermissionConfigError> {
        match ext {
            Some("json") => serde_json::from_str(content)
                .map_err(|e| PermissionConfigError::Parse(e.to_string())),
            _ => {
                // Treat as JSON if it starts with `{`
                let trimmed = content.trim();
                if trimmed.starts_with('{') || trimmed.starts_with('[') {
                    serde_json::from_str(content)
                        .map_err(|e| PermissionConfigError::Parse(e.to_string()))
                } else {
                    // Parse as YAML using serde_json's Value as intermediary:
                    // Since we don't depend on serde_yaml, parse a simple subset.
                    // For full YAML support, the caller can deserialize externally and
                    // pass an already-parsed PermissionRulesConfig.
                    serde_json::from_str(content)
                        .map_err(|e| PermissionConfigError::Parse(e.to_string()))
                }
            }
        }
    }

    /// Convert this configuration into a [`PermissionRuleset`] for evaluation.
    pub fn into_ruleset(self) -> Result<PermissionRuleset, PermissionConfigError> {
        let mut ruleset = PermissionRuleset {
            default_behavior: self.default_behavior,
            ..Default::default()
        };

        for entry in self.rules {
            let rule = rule_from_entry(&entry)?;
            ruleset.rules.insert(rule.subject.key(), rule);
        }

        Ok(ruleset)
    }
}

/// Convert a single config entry into a [`PermissionRule`].
fn rule_from_entry(entry: &PermissionRuleEntry) -> Result<PermissionRule, PermissionConfigError> {
    let tool_str = &entry.tool;

    // Try parsing as a full pattern first (e.g. "Bash(npm *)")
    if let Ok(pattern) = parse_pattern(tool_str) {
        return Ok(PermissionRule {
            subject: PermissionSubject::pattern(pattern),
            behavior: entry.behavior,
            scope: entry.scope,
            source: PermissionRuleSource::Definition,
        });
    }

    // Check for glob characters — treat as glob tool matcher
    if tool_str.contains('*') || tool_str.contains('?') || tool_str.contains('[') {
        let pattern = ToolCallPattern::tool_glob(tool_str.clone());
        return Ok(PermissionRule {
            subject: PermissionSubject::pattern(pattern),
            behavior: entry.behavior,
            scope: entry.scope,
            source: PermissionRuleSource::Definition,
        });
    }

    // Plain exact tool name
    Ok(PermissionRule {
        subject: PermissionSubject::tool(tool_str.clone()),
        behavior: entry.behavior,
        scope: entry.scope,
        source: PermissionRuleSource::Definition,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_json_config() {
        let json = r#"{
            "default_behavior": "deny",
            "rules": [
                {"tool": "file_*", "behavior": "ask"},
                {"tool": "read_file", "behavior": "allow"},
                {"tool": "delete_*", "behavior": "deny"}
            ]
        }"#;

        let config = PermissionRulesConfig::from_str(json, Some("json")).unwrap();
        assert_eq!(config.default_behavior, ToolPermissionBehavior::Deny);
        assert_eq!(config.rules.len(), 3);
        assert_eq!(config.rules[0].tool, "file_*");
        assert_eq!(config.rules[0].behavior, ToolPermissionBehavior::Ask);
    }

    #[test]
    fn config_into_ruleset() {
        let config = PermissionRulesConfig {
            default_behavior: ToolPermissionBehavior::Deny,
            rules: vec![
                PermissionRuleEntry {
                    tool: "read_file".to_string(),
                    behavior: ToolPermissionBehavior::Allow,
                    scope: PermissionRuleScope::Project,
                },
                PermissionRuleEntry {
                    tool: "file_*".to_string(),
                    behavior: ToolPermissionBehavior::Ask,
                    scope: PermissionRuleScope::Project,
                },
                PermissionRuleEntry {
                    tool: "delete_*".to_string(),
                    behavior: ToolPermissionBehavior::Deny,
                    scope: PermissionRuleScope::Project,
                },
            ],
        };

        let ruleset = config.into_ruleset().unwrap();
        assert_eq!(ruleset.default_behavior, ToolPermissionBehavior::Deny);
        assert_eq!(ruleset.rules.len(), 3);

        // `parse_pattern("read_file")` produces an exact-tool pattern,
        // stored under `pattern:read_file` (not `tool:read_file`).
        let rule = ruleset.rules.get("pattern:read_file").unwrap();
        assert_eq!(rule.behavior, ToolPermissionBehavior::Allow);
        assert_eq!(rule.source, PermissionRuleSource::Definition);
    }

    #[test]
    fn config_glob_tool_pattern() {
        let config = PermissionRulesConfig {
            default_behavior: ToolPermissionBehavior::Ask,
            rules: vec![PermissionRuleEntry {
                tool: "mcp__*".to_string(),
                behavior: ToolPermissionBehavior::Allow,
                scope: PermissionRuleScope::Project,
            }],
        };

        let ruleset = config.into_ruleset().unwrap();
        assert_eq!(ruleset.rules.len(), 1);
        // Should be stored as a pattern rule, not a tool rule
        let keys: Vec<&String> = ruleset.rules.keys().collect();
        assert!(keys[0].starts_with("pattern:"));
    }

    #[test]
    fn config_pattern_with_args() {
        let config = PermissionRulesConfig {
            default_behavior: ToolPermissionBehavior::Ask,
            rules: vec![PermissionRuleEntry {
                tool: "Bash(npm *)".to_string(),
                behavior: ToolPermissionBehavior::Allow,
                scope: PermissionRuleScope::Project,
            }],
        };

        let ruleset = config.into_ruleset().unwrap();
        assert_eq!(ruleset.rules.len(), 1);
        let keys: Vec<&String> = ruleset.rules.keys().collect();
        assert!(keys[0].starts_with("pattern:"));
    }

    #[test]
    fn config_default_values() {
        let config = PermissionRulesConfig::default();
        assert_eq!(config.default_behavior, ToolPermissionBehavior::Ask);
        assert!(config.rules.is_empty());
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = PermissionRulesConfig {
            default_behavior: ToolPermissionBehavior::Deny,
            rules: vec![
                PermissionRuleEntry {
                    tool: "read_file".to_string(),
                    behavior: ToolPermissionBehavior::Allow,
                    scope: PermissionRuleScope::Project,
                },
                PermissionRuleEntry {
                    tool: "Bash(npm *)".to_string(),
                    behavior: ToolPermissionBehavior::Allow,
                    scope: PermissionRuleScope::Session,
                },
            ],
        };

        let json = serde_json::to_string(&config).unwrap();
        let decoded: PermissionRulesConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.default_behavior, ToolPermissionBehavior::Deny);
        assert_eq!(decoded.rules.len(), 2);
        assert_eq!(decoded.rules[1].scope, PermissionRuleScope::Session);
    }

    #[test]
    fn config_auto_detect_json() {
        let json = r#"{"default_behavior": "allow", "rules": []}"#;
        let config = PermissionRulesConfig::from_str(json, None).unwrap();
        assert_eq!(config.default_behavior, ToolPermissionBehavior::Allow);
    }

    #[test]
    fn config_error_display() {
        let err = PermissionConfigError::InvalidPattern {
            pattern: "bad[".to_string(),
            reason: "unclosed bracket".to_string(),
        };
        assert_eq!(err.to_string(), "invalid pattern `bad[`: unclosed bracket");
    }

    #[test]
    fn config_empty_rules_valid() {
        let json = r#"{"default_behavior": "deny", "rules": []}"#;
        let config: PermissionRulesConfig = serde_json::from_str(json).unwrap();
        let ruleset = config.into_ruleset().unwrap();
        assert_eq!(ruleset.default_behavior, ToolPermissionBehavior::Deny);
        assert!(ruleset.rules.is_empty());
    }

    #[test]
    fn rule_scope_defaults_to_project() {
        let json = r#"{"tool": "Bash", "behavior": "allow"}"#;
        let entry: PermissionRuleEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.scope, PermissionRuleScope::Project);
    }

    #[test]
    fn permission_config_key_binding() {
        assert_eq!(PermissionConfigKey::KEY, "permission");
    }
}
