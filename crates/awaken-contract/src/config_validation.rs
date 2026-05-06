use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::agent_spec_patch::AgentSpecPatch;
use crate::config_record::{ConfigRecord, ConfigRecordError, ConfigRecordMerge};
use crate::registry_spec::{AgentSpec, ModelBindingSpec, ProviderSpec};

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

const PROVIDER_SPEC_FIELDS: &[&str] = &[
    "id",
    "adapter",
    "api_key",
    "base_url",
    "timeout_secs",
    "adapter_options",
];
const MODEL_BINDING_SPEC_FIELDS: &[&str] = &["id", "provider_id", "upstream_model"];

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
}
