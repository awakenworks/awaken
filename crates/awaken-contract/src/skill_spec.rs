//! Skill registry record managed through ConfigStore.
//!
//! `SkillSpec` is the structured, database-managed representation of a skill.
//! It deliberately exposes editable skill fields rather than raw `SKILL.md`
//! frontmatter so HTTP/config callers can validate and patch records without
//! reparsing markdown.

use serde::{Deserialize, Serialize};

/// Execution mode for a config-managed skill.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SkillSpecContext {
    /// Inject the skill into the current conversation context.
    #[default]
    Inline,
    /// Reserve the skill for forked/sub-context execution.
    Fork,
}

/// Formal argument metadata for a skill activation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillArgumentSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
}

/// ConfigStore representation of a skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillSpec {
    /// Canonical skill id. Must be a valid skill-name token.
    pub id: String,
    /// Human-facing display name.
    pub name: String,
    /// Catalog description shown to the model/user.
    pub description: String,
    /// Markdown instructions injected when the skill is activated. This is the
    /// SKILL.md body, not a full frontmatter document.
    pub instructions_md: String,
    /// Tool ids or matcher patterns granted when the skill is activated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<SkillArgumentSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
    #[serde(default = "default_true")]
    pub user_invocable: bool,
    #[serde(default = "default_true")]
    pub model_invocable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_override: Option<String>,
    #[serde(default)]
    pub context: SkillSpecContext,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

fn default_true() -> bool {
    true
}

impl Default for SkillSpec {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            description: String::new(),
            instructions_md: String::new(),
            allowed_tools: Vec::new(),
            when_to_use: None,
            arguments: Vec::new(),
            argument_hint: None,
            user_invocable: true,
            model_invocable: true,
            model_override: None,
            context: SkillSpecContext::Inline,
            paths: Vec::new(),
        }
    }
}

/// Sink used by runtime/config managers that publish DB-managed skill specs to
/// a live skill registry without depending on the concrete skills extension.
pub trait SkillSpecSink: Send + Sync {
    /// Validate a candidate spec set without changing the live registry.
    fn validate_skill_specs(&self, specs: &[SkillSpec]) -> Result<(), String>;

    /// Replace the live registry with the provided specs.
    fn replace_skill_specs(&self, specs: Vec<SkillSpec>) -> Result<(), String>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trip_preserves_fields() {
        let spec = SkillSpec {
            id: "db-management".into(),
            name: "Database Management".into(),
            description: "Helps with database operations".into(),
            instructions_md: "Inspect schema before writing queries.".into(),
            allowed_tools: vec!["db_query".into()],
            when_to_use: Some("When the user asks about a database".into()),
            arguments: vec![SkillArgumentSpec {
                name: "dialect".into(),
                description: Some("SQL dialect".into()),
                required: false,
            }],
            argument_hint: Some("dialect=postgres".into()),
            user_invocable: true,
            model_invocable: false,
            model_override: Some("fast".into()),
            context: SkillSpecContext::Fork,
            paths: vec!["migrations/**".into()],
        };
        let value = serde_json::to_value(&spec).unwrap();
        let back: SkillSpec = serde_json::from_value(value).unwrap();
        assert_eq!(spec, back);
    }

    #[test]
    fn serde_defaults_match_runtime_defaults() {
        let spec: SkillSpec = serde_json::from_value(json!({
            "id": "db-management",
            "name": "Database Management",
            "description": "Helps with database operations",
            "instructions_md": "Inspect schema before writing queries."
        }))
        .unwrap();
        assert!(spec.user_invocable);
        assert!(spec.model_invocable);
        assert_eq!(spec.context, SkillSpecContext::Inline);
        assert!(spec.allowed_tools.is_empty());
        assert!(spec.paths.is_empty());
    }

    #[test]
    fn unknown_field_is_rejected() {
        let bad = json!({
            "id": "x",
            "name": "x",
            "description": "x",
            "instructions_md": "x",
            "garbage": true
        });
        assert!(serde_json::from_value::<SkillSpec>(bad).is_err());
    }
}
