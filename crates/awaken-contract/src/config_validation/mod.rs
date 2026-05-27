use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashSet;

use crate::agent_spec_patch::AgentSpecPatch;
use crate::config_record::{ConfigRecord, ConfigRecordError, ConfigRecordMerge};
use crate::registry_spec::{
    AgentSpec, Modality, ModelPoolSpec, ModelSpec, PoolMemberRole, ProviderSpec,
};
use crate::skill_allowed_tools::{
    is_skill_allowed_tool_pattern, parse_skill_allowed_tools, validate_skill_allowed_tool_pattern,
};
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
pub const MODEL_SPEC_UNKNOWN_FIELD_POLICY: UnknownFieldPolicy = UnknownFieldPolicy::Reject;
pub const MODEL_POOL_SPEC_UNKNOWN_FIELD_POLICY: UnknownFieldPolicy = UnknownFieldPolicy::Reject;
pub const SKILL_SPEC_UNKNOWN_FIELD_POLICY: UnknownFieldPolicy = UnknownFieldPolicy::Reject;

const PROVIDER_SPEC_FIELDS: &[&str] = &[
    "id",
    "adapter",
    "api_key",
    "base_url",
    "timeout_secs",
    "adapter_options",
];
const MODEL_SPEC_FIELDS: &[&str] = &[
    "id",
    "provider_id",
    "upstream_model",
    "context_window",
    "max_output_tokens",
    "modalities",
    "knowledge_cutoff",
    "input_token_price_per_million_usd",
    "output_token_price_per_million_usd",
];
const MODEL_POOL_SPEC_FIELDS: &[&str] = &["id", "members", "routing", "switch"];
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
    #[error("invalid model spec: {0}")]
    ModelSpec(#[source] serde_json::Error),
    #[error("invalid model pool spec: {0}")]
    ModelPoolSpec(#[source] serde_json::Error),
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
    #[error("duplicate model id '{id}'")]
    DuplicateModelId { id: String },
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
    validate_provider_adapter_options(&value)?;
    let spec: ProviderSpec =
        serde_json::from_value(value).map_err(ConfigValidationError::ProviderSpec)?;
    reject_empty("provider spec", "id", &spec.id)?;
    reject_empty("provider spec", "adapter", &spec.adapter)?;
    Ok(spec)
}

fn validate_provider_adapter_options(value: &Value) -> Result<(), ConfigValidationError> {
    let Some(options) = value
        .get("adapter_options")
        .and_then(|value| value.as_object())
    else {
        return Ok(());
    };

    if let Some(schema) = options.get("model_discovery_schema") {
        let Some(schema) = schema.as_str() else {
            return Err(ConfigValidationError::Invalid {
                surface: "provider spec",
                message: "'adapter_options.model_discovery_schema' must be a string".into(),
            });
        };
        let normalized = schema.to_ascii_lowercase();
        if !matches!(
            normalized.as_str(),
            "openai" | "openai-compatible" | "openrouter" | "gemini" | "google"
        ) {
            return Err(ConfigValidationError::Invalid {
                surface: "provider spec",
                message: format!(
                    "'adapter_options.model_discovery_schema' must be one of openai, \
                     openai-compatible, openrouter, gemini, google; got '{schema}'"
                ),
            });
        }
    }

    if let Some(auth) = options.get("model_discovery_auth") {
        let Some(auth) = auth.as_str() else {
            return Err(ConfigValidationError::Invalid {
                surface: "provider spec",
                message: "'adapter_options.model_discovery_auth' must be a string".into(),
            });
        };
        let normalized = auth.to_ascii_lowercase();
        if !matches!(
            normalized.as_str(),
            "bearer"
                | "authorization-bearer"
                | "x-goog-api-key"
                | "google-api-key"
                | "gemini-api-key"
                | "none"
                | "no-auth"
                | "disabled"
        ) {
            return Err(ConfigValidationError::Invalid {
                surface: "provider spec",
                message: format!(
                    "'adapter_options.model_discovery_auth' must be one of bearer, \
                     authorization-bearer, x-goog-api-key, google-api-key, gemini-api-key, \
                     none, no-auth, disabled; got '{auth}'"
                ),
            });
        }
    }

    Ok(())
}

/// Validate and decode a `ModelSpec` from JSON for config write surfaces.
///
/// Rejects unknown fields (read-time deserialization stays lenient), then
/// delegates every semantic rule to [`validate_model_spec_struct`] so the
/// JSON path, the runtime builder, and the model registry all share one
/// definition of a valid `ModelSpec`.
pub fn validate_model_spec(value: Value) -> Result<ModelSpec, ConfigValidationError> {
    reject_unknown_fields(&value, "model spec", MODEL_SPEC_FIELDS)?;
    let spec: ModelSpec =
        serde_json::from_value(value).map_err(ConfigValidationError::ModelSpec)?;
    validate_model_spec_struct(&spec)?;
    Ok(spec)
}

/// Validate an already-constructed `ModelSpec`.
///
/// This is the single source of truth for `ModelSpec` validity. Both the
/// JSON config surface ([`validate_model_spec`]) and in-memory construction
/// paths (the runtime builder and model registry) call it so a `ModelSpec`
/// cannot enter any registry with values the config API would reject.
pub fn validate_model_spec_struct(spec: &ModelSpec) -> Result<(), ConfigValidationError> {
    reject_empty("model spec", "id", &spec.id)?;
    reject_empty("model spec", "provider_id", &spec.provider_id)?;
    reject_empty("model spec", "upstream_model", &spec.upstream_model)?;
    if let Some(cutoff) = spec.knowledge_cutoff.as_deref() {
        reject_empty("model spec", "knowledge_cutoff", cutoff)?;
        validate_knowledge_cutoff_format(cutoff)?;
    }
    reject_zero_capability("context_window", spec.context_window)?;
    reject_zero_capability("max_output_tokens", spec.max_output_tokens)?;
    if let (Some(ctx), Some(out)) = (spec.context_window, spec.max_output_tokens)
        && out > ctx
    {
        return Err(ConfigValidationError::Invalid {
            surface: "model spec",
            message: format!("max_output_tokens ({out}) must not exceed context_window ({ctx})"),
        });
    }
    reject_invalid_price(
        "input_token_price_per_million_usd",
        spec.input_token_price_per_million_usd,
    )?;
    reject_invalid_price(
        "output_token_price_per_million_usd",
        spec.output_token_price_per_million_usd,
    )?;
    reject_duplicate_modalities("input", &spec.modalities.input)?;
    reject_duplicate_modalities("output", &spec.modalities.output)?;
    Ok(())
}

fn reject_invalid_price(field: &str, value: Option<f64>) -> Result<(), ConfigValidationError> {
    if let Some(price) = value
        && (!price.is_finite() || price < 0.0)
    {
        return Err(ConfigValidationError::Invalid {
            surface: "model spec",
            message: format!("'{field}' must be a finite non-negative number, got {price}"),
        });
    }
    Ok(())
}

fn validate_knowledge_cutoff_format(value: &str) -> Result<(), ConfigValidationError> {
    let bytes = value.as_bytes();
    let valid_shape = match bytes.len() {
        7 => {
            bytes[4] == b'-'
                && bytes[..4].iter().all(|b| b.is_ascii_digit())
                && bytes[5..].iter().all(|b| b.is_ascii_digit())
        }
        10 => {
            bytes[4] == b'-'
                && bytes[7] == b'-'
                && bytes[..4].iter().all(|b| b.is_ascii_digit())
                && bytes[5..7].iter().all(|b| b.is_ascii_digit())
                && bytes[8..].iter().all(|b| b.is_ascii_digit())
        }
        _ => false,
    };
    if !valid_shape {
        return Err(ConfigValidationError::Invalid {
            surface: "model spec",
            message: format!(
                "'knowledge_cutoff' must be ISO date 'YYYY-MM' or 'YYYY-MM-DD', got '{value}'"
            ),
        });
    }
    let year: i32 = value[..4].parse().unwrap_or(0);
    let month: u32 = value[5..7].parse().unwrap_or(0);
    if !(1..=12).contains(&month) {
        return Err(ConfigValidationError::Invalid {
            surface: "model spec",
            message: format!("'knowledge_cutoff' month must be 01-12, got '{value}'"),
        });
    }
    if bytes.len() == 10 {
        let day: u32 = value[8..10].parse().unwrap_or(0);
        // Real calendar validity, not just 01-31 shape: rejects 2026-02-31,
        // 2026-04-31, etc., and honors leap years for February.
        let max_day = days_in_month(year, month);
        if day < 1 || day > max_day {
            return Err(ConfigValidationError::Invalid {
                surface: "model spec",
                message: format!(
                    "'knowledge_cutoff' day must be 01-{max_day:02} for {year:04}-{month:02}, got '{value}'"
                ),
            });
        }
    }
    Ok(())
}

/// Days in a Gregorian month. `month` is assumed already validated to 1..=12.
fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
            if leap { 29 } else { 28 }
        }
        _ => 31,
    }
}

fn reject_duplicate_modalities(
    field: &str,
    items: &[Modality],
) -> Result<(), ConfigValidationError> {
    let mut seen = HashSet::new();
    for m in items {
        if !seen.insert(*m) {
            return Err(ConfigValidationError::Invalid {
                surface: "model spec",
                message: format!("'modalities.{field}' contains duplicate '{m:?}'"),
            });
        }
    }
    Ok(())
}

fn reject_zero_capability(
    field: &'static str,
    value: Option<u32>,
) -> Result<(), ConfigValidationError> {
    match value {
        Some(0) => Err(ConfigValidationError::Invalid {
            surface: "model spec",
            message: format!("field '{field}' must be greater than zero"),
        }),
        _ => Ok(()),
    }
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
    let mut argument_names = HashSet::new();
    for argument in &spec.arguments {
        reject_empty("skill spec", "arguments.name", &argument.name)?;
        let argument_name = argument.name.trim();
        if argument_name != argument.name {
            return Err(ConfigValidationError::Invalid {
                surface: "skill spec",
                message: format!(
                    "argument name '{}' must not contain surrounding whitespace",
                    argument.name
                ),
            });
        }
        if !argument_names.insert(argument_name.to_string()) {
            return Err(ConfigValidationError::Invalid {
                surface: "skill spec",
                message: format!("duplicate argument name '{}'", argument.name),
            });
        }
        if let Some(description) = &argument.description {
            reject_empty("skill spec", "arguments.description", description)?;
        }
    }
    for tool in &spec.allowed_tools {
        validate_allowed_tool_token(tool)?;
    }
    if !spec.paths.is_empty() {
        return Err(ConfigValidationError::Invalid {
            surface: "skill spec",
            message: "paths are not supported for DB-managed skills until resources are persisted"
                .into(),
        });
    }
    Ok(spec)
}

/// Validate and decode a `ModelPoolSpec` from JSON for config write surfaces.
///
/// Rejects unknown fields, then delegates every semantic rule to
/// [`validate_model_pool_spec_struct`] so the JSON path and in-memory
/// construction share one definition of a valid pool.
pub fn validate_model_pool_spec(value: Value) -> Result<ModelPoolSpec, ConfigValidationError> {
    reject_unknown_fields(&value, "model pool spec", MODEL_POOL_SPEC_FIELDS)?;
    let spec: ModelPoolSpec =
        serde_json::from_value(value).map_err(ConfigValidationError::ModelPoolSpec)?;
    validate_model_pool_spec_struct(&spec)?;
    Ok(spec)
}

/// Validate an already-constructed `ModelPoolSpec`.
///
/// Single source of truth for pool validity. Member `model_id` references are
/// checked against the surrounding registry elsewhere (resolution); this
/// validates the pool in isolation.
pub fn validate_model_pool_spec_struct(spec: &ModelPoolSpec) -> Result<(), ConfigValidationError> {
    reject_empty("model pool spec", "id", &spec.id)?;
    if spec.members.is_empty() {
        return Err(ConfigValidationError::Invalid {
            surface: "model pool spec",
            message: "must declare at least one member".into(),
        });
    }
    let mut seen = HashSet::new();
    let mut has_home_member = false;
    for member in &spec.members {
        reject_empty("model pool spec", "members.model_id", &member.model_id)?;
        if member.weight == Some(0) {
            return Err(ConfigValidationError::Invalid {
                surface: "model pool spec",
                message: format!(
                    "member '{}' weight must be greater than zero",
                    member.model_id
                ),
            });
        }
        if !seen.insert(member.model_id.as_str()) {
            return Err(ConfigValidationError::Invalid {
                surface: "model pool spec",
                message: format!("duplicate member model_id '{}'", member.model_id),
            });
        }
        if member.role == PoolMemberRole::Member {
            has_home_member = true;
        }
    }
    if !has_home_member {
        return Err(ConfigValidationError::Invalid {
            surface: "model pool spec",
            message: "at least one member must be home-eligible (role 'member'); \
                      a pool of only 'failover_only' members has no home target"
                .into(),
        });
    }
    Ok(())
}

/// Validate that a slice of `ModelSpec` contains no duplicate `id` values.
///
/// Returns the first duplicate id encountered in input order. Intended for
/// collection-holders (e.g. `ManagedConfig.models`) to call at write time so
/// downstream registry assembly never observes shadowed entries.
pub fn validate_unique_model_ids(specs: &[ModelSpec]) -> Result<(), ConfigValidationError> {
    let mut seen = HashSet::new();
    for spec in specs {
        if !seen.insert(spec.id.as_str()) {
            return Err(ConfigValidationError::DuplicateModelId {
                id: spec.id.clone(),
            });
        }
    }
    Ok(())
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
    if !id.chars().all(|c| c.is_alphanumeric() || c == '-') {
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
    let parsed =
        parse_skill_allowed_tools(token).map_err(|error| ConfigValidationError::Invalid {
            surface: "skill spec",
            message: format!("invalid allowed_tools entry '{token}': {error}"),
        })?;
    if parsed.len() != 1 || parsed[0].raw != token {
        return Err(ConfigValidationError::Invalid {
            surface: "skill spec",
            message: format!("allowed_tools entry '{token}' must contain exactly one token"),
        });
    }
    if parsed[0].scope.is_some() {
        return Err(ConfigValidationError::Invalid {
            surface: "skill spec",
            message: format!(
                "scoped allowed_tools entry '{token}' is not supported for DB-managed skills"
            ),
        });
    }
    if is_skill_allowed_tool_pattern(&parsed[0].tool_id) {
        validate_skill_allowed_tool_pattern(&parsed[0].tool_id).map_err(|error| {
            ConfigValidationError::Invalid {
                surface: "skill spec",
                message: error.to_string(),
            }
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
