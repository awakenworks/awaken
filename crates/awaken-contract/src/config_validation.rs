use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::agent_spec_patch::AgentSpecPatch;
use crate::config_record::{ConfigRecord, ConfigRecordError, ConfigRecordMerge};
use crate::registry_spec::{AgentSpec, ModelBindingSpec, ProviderSpec};
use crate::skill_spec::SkillSpec;

/// Unknown-field behavior for a serializable config surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownFieldPolicy {
    Reject,
    Ignore,
}

/// `AgentSpec` and `AgentSpecPatch` reject unknown fields.
pub const AGENT_SPEC_UNKNOWN_FIELD_POLICY: UnknownFieldPolicy = UnknownFieldPolicy::Reject;
pub const AGENT_SPEC_PATCH_UNKNOWN_FIELD_POLICY: UnknownFieldPolicy = UnknownFieldPolicy::Reject;
/// `ProviderSpec`'s serde implementation is intentionally lenient for
/// read-time compatibility, but config write/validate surfaces reject unknown
/// fields so operators do not persist silently ignored provider settings.
pub const PROVIDER_SPEC_UNKNOWN_FIELD_POLICY: UnknownFieldPolicy = UnknownFieldPolicy::Reject;
pub const MODEL_BINDING_SPEC_UNKNOWN_FIELD_POLICY: UnknownFieldPolicy = UnknownFieldPolicy::Reject;
pub const SKILL_SPEC_UNKNOWN_FIELD_POLICY: UnknownFieldPolicy = UnknownFieldPolicy::Reject;

const PROVIDER_SPEC_FIELDS: &[&str] = &[
    "id",
    "adapter",
    "api_key",
    "base_url",
    "timeout_secs",
    "adapter_options",
];
const MODEL_BINDING_SPEC_FIELDS: &[&str] = &["id", "provider_id", "upstream_model"];
const SKILL_SPEC_FIELDS: &[&str] = &[
    "id",
    "name",
    "description",
    "instructions_md",
    "allowed_tools",
    "when_to_use",
    "arguments",
    "argument_hint",
    "user_invocable",
    "model_invocable",
    "model_override",
    "context",
    "paths",
];

#[derive(Debug, thiserror::Error)]
pub enum ConfigValidationError {
    #[error("invalid agent spec: {0}")]
    AgentSpec(#[source] serde_json::Error),
    #[error("invalid agent spec patch: {0}")]
    AgentSpecPatch(#[source] serde_json::Error),
    #[error("invalid provider spec: {0}")]
    ProviderSpec(#[source] serde_json::Error),
    #[error("invalid model binding spec: {0}")]
    ModelBindingSpec(#[source] serde_json::Error),
    #[error("invalid skill spec: {0}")]
    SkillSpec(#[source] serde_json::Error),
    #[error("invalid {surface}: unknown field '{field}'")]
    UnknownField {
        surface: &'static str,
        field: String,
    },
    #[error("invalid {surface}: field '{field}' cannot be empty")]
    EmptyField {
        surface: &'static str,
        field: &'static str,
    },
    #[error("invalid config record: {0}")]
    ConfigRecord(#[from] ConfigRecordError),
    #[error("invalid {surface}: {message}")]
    Invalid {
        surface: &'static str,
        message: String,
    },
}

/// Validate and decode an `AgentSpec`.
///
/// Unknown fields are rejected by `AgentSpec`'s serde definition.
pub fn validate_agent_spec(value: Value) -> Result<AgentSpec, ConfigValidationError> {
    serde_json::from_value(value).map_err(ConfigValidationError::AgentSpec)
}

/// Validate and decode an `AgentSpecPatch`.
///
/// Unknown fields are rejected by `AgentSpecPatch`'s serde definition.
pub fn validate_agent_spec_patch(value: Value) -> Result<AgentSpecPatch, ConfigValidationError> {
    serde_json::from_value(value).map_err(ConfigValidationError::AgentSpecPatch)
}

/// Validate and decode a `ProviderSpec` for config write surfaces.
///
/// Unknown fields are rejected here even though `ProviderSpec` deserialization
/// remains lenient for read-time compatibility with future/older envelopes.
/// Adapter support is intentionally not hard-coded in `awaken-contract`;
/// runtime/server builders validate whether the linked provider backend
/// supports a non-empty adapter string.
pub fn validate_provider_spec(value: Value) -> Result<ProviderSpec, ConfigValidationError> {
    reject_unknown_fields(&value, "provider spec", PROVIDER_SPEC_FIELDS)?;
    let spec: ProviderSpec =
        serde_json::from_value(value).map_err(ConfigValidationError::ProviderSpec)?;
    reject_empty("provider spec", "id", &spec.id)?;
    reject_empty("provider spec", "adapter", &spec.adapter)?;
    Ok(spec)
}

/// Validate and decode a `ModelBindingSpec` for config write surfaces.
pub fn validate_model_binding_spec(
    value: Value,
) -> Result<ModelBindingSpec, ConfigValidationError> {
    reject_unknown_fields(&value, "model binding spec", MODEL_BINDING_SPEC_FIELDS)?;
    let spec: ModelBindingSpec =
        serde_json::from_value(value).map_err(ConfigValidationError::ModelBindingSpec)?;
    reject_empty("model binding spec", "id", &spec.id)?;
    reject_empty("model binding spec", "provider_id", &spec.provider_id)?;
    reject_empty("model binding spec", "upstream_model", &spec.upstream_model)?;
    Ok(spec)
}

/// Validate and decode a `SkillSpec` for config write surfaces.
pub fn validate_skill_spec(value: Value) -> Result<SkillSpec, ConfigValidationError> {
    reject_unknown_fields(&value, "skill spec", SKILL_SPEC_FIELDS)?;
    let spec: SkillSpec =
        serde_json::from_value(value).map_err(ConfigValidationError::SkillSpec)?;
    validate_skill_id("skill spec", &spec.id)?;
    reject_empty("skill spec", "name", &spec.name)?;
    reject_empty("skill spec", "description", &spec.description)?;
    reject_empty("skill spec", "instructions_md", &spec.instructions_md)?;
    reject_max_chars("skill spec", "name", &spec.name, 128)?;
    reject_max_chars("skill spec", "description", &spec.description, 1024)?;
    if let Some(value) = &spec.when_to_use {
        reject_empty("skill spec", "when_to_use", value)?;
    }
    if let Some(value) = &spec.argument_hint {
        reject_empty("skill spec", "argument_hint", value)?;
    }
    if let Some(value) = &spec.model_override {
        reject_empty("skill spec", "model_override", value)?;
    }
    for argument in &spec.arguments {
        reject_empty("skill spec", "arguments.name", &argument.name)?;
        if let Some(description) = &argument.description {
            reject_empty("skill spec", "arguments.description", description)?;
        }
    }
    for tool in &spec.allowed_tools {
        validate_allowed_tool_token(tool)?;
    }
    for path in &spec.paths {
        reject_empty("skill spec", "paths", path)?;
    }
    Ok(spec)
}

/// Validate and decode a config record envelope, accepting legacy bare specs.
/// `RecordMeta::user_overrides` must decode as the patch type for `T`.
pub fn validate_config_record<T>(value: Value) -> Result<ConfigRecord<T>, ConfigValidationError>
where
    T: DeserializeOwned + ConfigRecordMerge,
{
    crate::config_record::validate_config_record(value).map_err(ConfigValidationError::ConfigRecord)
}

fn reject_unknown_fields(
    value: &Value,
    surface: &'static str,
    allowed: &[&str],
) -> Result<(), ConfigValidationError> {
    let Some(object) = value.as_object() else {
        return Ok(());
    };
    if let Some(field) = object
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        return Err(ConfigValidationError::UnknownField {
            surface,
            field: field.clone(),
        });
    }
    Ok(())
}

fn reject_empty(
    surface: &'static str,
    field: &'static str,
    value: &str,
) -> Result<(), ConfigValidationError> {
    if value.trim().is_empty() {
        Err(ConfigValidationError::EmptyField { surface, field })
    } else {
        Ok(())
    }
}

fn reject_max_chars(
    surface: &'static str,
    field: &'static str,
    value: &str,
    max_chars: usize,
) -> Result<(), ConfigValidationError> {
    if value.chars().count() > max_chars {
        Err(ConfigValidationError::Invalid {
            surface,
            message: format!("field '{field}' must be <= {max_chars} characters"),
        })
    } else {
        Ok(())
    }
}

fn validate_skill_id(surface: &'static str, value: &str) -> Result<(), ConfigValidationError> {
    let id = value.trim();
    reject_empty(surface, "id", id)?;
    if id != value {
        return Err(ConfigValidationError::Invalid {
            surface,
            message: "field 'id' must not contain leading or trailing whitespace".into(),
        });
    }
    let len = id.chars().count();
    if len > 64 {
        return Err(ConfigValidationError::Invalid {
            surface,
            message: "field 'id' must be <= 64 characters".into(),
        });
    }
    if id != id.to_lowercase() {
        return Err(ConfigValidationError::Invalid {
            surface,
            message: "field 'id' must be lowercase".into(),
        });
    }
    if id.starts_with('-') || id.ends_with('-') || id.contains("--") {
        return Err(ConfigValidationError::Invalid {
            surface,
            message: "field 'id' must not start/end with '-' or contain consecutive '-'".into(),
        });
    }
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(ConfigValidationError::Invalid {
            surface,
            message: "field 'id' contains invalid characters".into(),
        });
    }
    Ok(())
}

fn validate_allowed_tool_token(value: &str) -> Result<(), ConfigValidationError> {
    let token = value.trim();
    if token.is_empty() {
        return Err(ConfigValidationError::Invalid {
            surface: "skill spec",
            message: "allowed_tools entries must be non-empty".into(),
        });
    }
    if token != value {
        return Err(ConfigValidationError::Invalid {
            surface: "skill spec",
            message: format!(
                "allowed_tools entry '{token}' must not contain surrounding whitespace"
            ),
        });
    }
    if token.chars().any(char::is_whitespace) {
        return Err(ConfigValidationError::Invalid {
            surface: "skill spec",
            message: format!("allowed_tools entry '{token}' must not contain whitespace"),
        });
    }
    let opens = token.chars().filter(|c| *c == '(').count();
    let closes = token.chars().filter(|c| *c == ')').count();
    if opens != closes {
        return Err(ConfigValidationError::Invalid {
            surface: "skill spec",
            message: format!("allowed_tools entry '{token}' has unbalanced parentheses"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn validate_agent_spec_rejects_unknown_fields() {
        let err = validate_agent_spec(json!({
            "id": "a",
            "model_id": "m",
            "system_prompt": "s",
            "model": "legacy"
        }))
        .expect_err("unknown field must be rejected");
        assert!(err.to_string().contains("invalid agent spec"));
    }

    #[test]
    fn validate_agent_spec_patch_rejects_unknown_fields() {
        let err = validate_agent_spec_patch(json!({"bogus": true}))
            .expect_err("unknown patch field must be rejected");
        assert!(err.to_string().contains("invalid agent spec patch"));
    }

    #[test]
    fn validate_config_record_accepts_legacy_bare_spec() {
        let record = validate_config_record::<AgentSpec>(json!({
            "id": "a",
            "model_id": "m",
            "system_prompt": "s"
        }))
        .expect("legacy bare spec must decode");
        assert_eq!(record.spec.id, "a");
    }

    #[test]
    fn validate_config_record_rejects_invalid_user_overrides() {
        let err = validate_config_record::<AgentSpec>(json!({
            "spec": {
                "id": "a",
                "model_id": "m",
                "system_prompt": "s"
            },
            "meta": {
                "source": {"kind": "builtin", "binary_version": "test"},
                "user_overrides": {"unknown_patch_field": true}
            }
        }))
        .expect_err("invalid overrides must fail validation");
        assert!(err.to_string().contains("invalid config record"));
    }

    #[test]
    fn validate_provider_spec_rejects_unknown_and_empty_fields() {
        let err = validate_provider_spec(json!({
            "id": "p",
            "adapter": "openai",
            "future_top_level": true
        }))
        .expect_err("unknown provider fields must be rejected on write surfaces");
        assert!(err.to_string().contains("unknown field 'future_top_level'"));

        let err = validate_provider_spec(json!({
            "id": " ",
            "adapter": "openai"
        }))
        .expect_err("empty provider id must be rejected");
        assert!(err.to_string().contains("field 'id' cannot be empty"));

        let err = validate_provider_spec(json!({
            "id": "p",
            "adapter": ""
        }))
        .expect_err("empty provider adapter must be rejected");
        assert!(err.to_string().contains("field 'adapter' cannot be empty"));
    }

    #[test]
    fn validate_model_binding_spec_rejects_unknown_and_empty_fields() {
        let err = validate_model_binding_spec(json!({
            "id": "m",
            "provider_id": "p",
            "upstream_model": "gpt-4",
            "future_top_level": true
        }))
        .expect_err("unknown model fields must be rejected");
        assert!(err.to_string().contains("unknown field 'future_top_level'"));

        let err = validate_model_binding_spec(json!({
            "id": "m",
            "provider_id": " ",
            "upstream_model": "gpt-4"
        }))
        .expect_err("empty provider_id must be rejected");
        assert!(
            err.to_string()
                .contains("field 'provider_id' cannot be empty")
        );
    }

    #[test]
    fn validate_skill_spec_accepts_valid_spec() {
        let spec = validate_skill_spec(json!({
            "id": "db-management",
            "name": "Database Management",
            "description": "Helps with database operations",
            "instructions_md": "Inspect schema before running SQL.",
            "allowed_tools": ["db_query", "mcp__db__*"],
            "arguments": [{"name": "dialect", "required": false}]
        }))
        .expect("valid skill spec");
        assert_eq!(spec.id, "db-management");
    }

    #[test]
    fn validate_skill_spec_rejects_invalid_id_and_tools() {
        let err = validate_skill_spec(json!({
            "id": "DB",
            "name": "Database Management",
            "description": "Helps with database operations",
            "instructions_md": "Inspect schema before running SQL."
        }))
        .expect_err("uppercase id must fail");
        assert!(err.to_string().contains("must be lowercase"));

        let err = validate_skill_spec(json!({
            "id": "db-management",
            "name": "Database Management",
            "description": "Helps with database operations",
            "instructions_md": "Inspect schema before running SQL.",
            "allowed_tools": ["bad token"]
        }))
        .expect_err("whitespace in tool token must fail");
        assert!(err.to_string().contains("must not contain whitespace"));
    }
}
