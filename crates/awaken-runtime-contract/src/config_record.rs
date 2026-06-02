//! Metadata envelope wrapping any spec stored in ConfigStore.
//!
//! Today most ConfigStore entries are bare specs (e.g. `AgentSpec` JSON).
//! This envelope carries provenance (was this seeded by the binary, or written
//! by a user?) and lifecycle flags (`hidden`) without breaking existing on-disk
//! data. The decoder accepts both shapes; the encoder always emits the envelope.

use serde::{Deserialize, Serialize};

use crate::agent_spec_patch::{AgentSpecPatch, merge_agent_spec};
use crate::registry_spec::{
    AgentSpec, BackendConfigError, McpServerSpec, ModelPoolSpec, ModelSpec, ProviderSpec,
};
use crate::skill_spec::SkillSpec;
use crate::skill_spec_patch::{SkillSpecPatch, merge_skill_spec};
use crate::tool_spec::ToolSpec;
use crate::tool_spec_patch::{ToolSpecPatch, merge_tool_spec};

/// Wrapper carrying a spec plus provenance + lifecycle metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConfigRecord<T> {
    pub spec: T,
    pub meta: RecordMeta,
}

/// Provenance + lifecycle metadata for a stored spec.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecordMeta {
    pub source: RecordSource,
    #[serde(default)]
    pub hidden: bool,
    /// Field-level overrides for Builtin records.
    /// Decoded by spec-type-specific helpers downstream; opaque at this layer.
    /// `None` for User records and for Builtin records that have not been customized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_overrides: Option<serde_json::Value>,
    /// Milliseconds since UNIX epoch (see `crate::time::now_ms`).
    /// `0` is a sentinel meaning "unknown / pre-envelope legacy entry".
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub updated_at: u64,
    /// Monotonic revision number for optimistic concurrency control.
    /// Bumped by `ConfigStore::put_if_revision` on each successful CAS write.
    /// Legacy records deserialise as 0; first `put_if_revision(... expected=0)`
    /// promotes them to 1.
    #[serde(default)]
    pub revision: u64,
}

/// Who wrote this record into ConfigStore.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecordSource {
    /// Written by binary startup seed; `binary_version` lets the next boot
    /// detect upgrades and refresh non-user-touched fields.
    Builtin { binary_version: String },
    /// Written by a user via UI/HTTP (or a script). Never overwritten by seed.
    User,
}

/// Empty patch for spec types that do not support field-level overrides.
///
/// The empty patch rejects unknown fields, so a non-empty `user_overrides`
/// payload on a non-patchable spec fails validation instead of being silently
/// ignored.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NoConfigPatch {}

/// Spec types that can apply `RecordMeta::user_overrides` at read time.
pub trait ConfigRecordMerge: Sized {
    type Patch: serde::de::DeserializeOwned;

    fn merge_patch(self, patch: Self::Patch) -> Result<Self, ConfigRecordError>;
}

impl ConfigRecordMerge for AgentSpec {
    type Patch = AgentSpecPatch;

    fn merge_patch(self, patch: AgentSpecPatch) -> Result<Self, ConfigRecordError> {
        merge_agent_spec(self, patch).map_err(ConfigRecordError::BackendConfig)
    }
}

impl ConfigRecordMerge for ToolSpec {
    type Patch = ToolSpecPatch;

    fn merge_patch(self, patch: ToolSpecPatch) -> Result<Self, ConfigRecordError> {
        Ok(merge_tool_spec(self, patch))
    }
}

impl ConfigRecordMerge for SkillSpec {
    type Patch = SkillSpecPatch;

    fn merge_patch(self, patch: SkillSpecPatch) -> Result<Self, ConfigRecordError> {
        Ok(merge_skill_spec(self, patch))
    }
}

impl ConfigRecordMerge for ProviderSpec {
    type Patch = NoConfigPatch;

    fn merge_patch(self, _patch: NoConfigPatch) -> Result<Self, ConfigRecordError> {
        Ok(self)
    }
}

impl ConfigRecordMerge for ModelSpec {
    type Patch = NoConfigPatch;

    fn merge_patch(self, _patch: NoConfigPatch) -> Result<Self, ConfigRecordError> {
        Ok(self)
    }
}

impl ConfigRecordMerge for ModelPoolSpec {
    type Patch = NoConfigPatch;

    fn merge_patch(self, _patch: NoConfigPatch) -> Result<Self, ConfigRecordError> {
        Ok(self)
    }
}

impl ConfigRecordMerge for McpServerSpec {
    type Patch = NoConfigPatch;

    fn merge_patch(self, _patch: NoConfigPatch) -> Result<Self, ConfigRecordError> {
        Ok(self)
    }
}

/// Error returned while decoding a [`ConfigRecord`] or applying its overrides.
#[derive(Debug, thiserror::Error)]
pub enum ConfigRecordError {
    #[error("invalid config record: {0}")]
    Decode(#[source] serde_json::Error),
    #[error("invalid config record overrides: {0}")]
    Overrides(#[source] serde_json::Error),
    #[error("invalid config record backend: {0}")]
    BackendConfig(#[source] BackendConfigError),
}

impl<T: serde::de::DeserializeOwned> ConfigRecord<T> {
    /// Decode a JSON value, accepting either the new envelope shape OR a
    /// legacy bare-spec shape (in which case the record is synthesized as
    /// `RecordSource::User`, `hidden = false`, timestamps = `0`).
    ///
    /// Detection rule: a value is the envelope if it is an object containing
    /// both `"spec"` and `"meta"` keys.
    pub fn from_value(value: serde_json::Value) -> Result<Self, serde_json::Error> {
        if is_envelope(&value) {
            serde_json::from_value(value)
        } else {
            let spec: T = serde_json::from_value(value)?;
            Ok(Self {
                spec,
                meta: RecordMeta::legacy_user(),
            })
        }
    }
}

/// Decode a value into [`ConfigRecord<T>`], accepting either an envelope or a
/// legacy bare spec. This does not validate `RecordMeta::user_overrides`.
pub fn decode_config_record<T>(
    value: serde_json::Value,
) -> Result<ConfigRecord<T>, ConfigRecordError>
where
    T: serde::de::DeserializeOwned,
{
    ConfigRecord::from_value(value).map_err(ConfigRecordError::Decode)
}

/// Decode a [`ConfigRecord<T>`] and validate its `RecordMeta::user_overrides`
/// against the patch type for `T`.
pub fn validate_config_record<T>(
    value: serde_json::Value,
) -> Result<ConfigRecord<T>, ConfigRecordError>
where
    T: serde::de::DeserializeOwned + ConfigRecordMerge,
{
    let record = decode_config_record::<T>(value)?;
    validate_config_record_overrides::<T>(&record)?;
    Ok(record)
}

/// Validate `RecordMeta::user_overrides` for an already decoded record.
pub fn validate_config_record_overrides<T>(
    record: &ConfigRecord<T>,
) -> Result<(), ConfigRecordError>
where
    T: ConfigRecordMerge,
{
    if let Some(overrides) = &record.meta.user_overrides {
        serde_json::from_value::<T::Patch>(overrides.clone())
            .map_err(ConfigRecordError::Overrides)?;
    }
    Ok(())
}

/// Apply `RecordMeta::user_overrides` to the record's base spec.
pub fn effective_config_record<T>(record: ConfigRecord<T>) -> Result<T, ConfigRecordError>
where
    T: ConfigRecordMerge,
{
    let Some(overrides) = record.meta.user_overrides else {
        return Ok(record.spec);
    };
    let patch: T::Patch =
        serde_json::from_value(overrides).map_err(ConfigRecordError::Overrides)?;
    record.spec.merge_patch(patch)
}

/// Decode visible records and return their effective specs.
///
/// Hidden records are skipped. Legacy bare specs are accepted and treated as
/// user-source records with no overrides.
pub fn effective_visible_config_records<T, I>(records: I) -> Result<Vec<T>, ConfigRecordError>
where
    T: serde::de::DeserializeOwned + ConfigRecordMerge,
    I: IntoIterator<Item = serde_json::Value>,
{
    let mut out = Vec::new();
    for value in records {
        let record = validate_config_record::<T>(value)?;
        if record.meta.hidden {
            continue;
        }
        out.push(effective_config_record(record)?);
    }
    Ok(out)
}

impl<T: Serialize> ConfigRecord<T> {
    /// Encode as the new envelope JSON. Always emits the envelope shape.
    pub fn to_value(&self) -> Result<serde_json::Value, serde_json::Error> {
        serde_json::to_value(self)
    }
}

impl RecordMeta {
    /// Synthesize metadata for a legacy bare-spec entry. Timestamps are `0`
    /// to mark them as unknown.
    pub fn legacy_user() -> Self {
        Self {
            source: RecordSource::User,
            hidden: false,
            user_overrides: None,
            created_at: 0,
            updated_at: 0,
            revision: 0,
        }
    }

    /// Construct a fresh User record with current timestamps.
    pub fn new_user() -> Self {
        let now = crate::time::now_ms();
        Self {
            source: RecordSource::User,
            hidden: false,
            user_overrides: None,
            created_at: now,
            updated_at: now,
            revision: 0,
        }
    }

    /// Construct a fresh Builtin record with current timestamps.
    pub fn new_builtin(binary_version: impl Into<String>) -> Self {
        let now = crate::time::now_ms();
        Self {
            source: RecordSource::Builtin {
                binary_version: binary_version.into(),
            },
            hidden: false,
            user_overrides: None,
            created_at: now,
            updated_at: now,
            revision: 0,
        }
    }
}

fn is_envelope(value: &serde_json::Value) -> bool {
    matches!(value, serde_json::Value::Object(map) if map.contains_key("spec") && map.contains_key("meta"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_json_without_revision_deserialises_to_zero() {
        // Simulate a legacy RecordMeta JSON that has no `revision` field.
        let json = serde_json::json!({
            "source": {"kind": "user"},
            "hidden": false,
            "created_at": 1000,
            "updated_at": 2000
        });
        let meta: RecordMeta = serde_json::from_value(json).unwrap();
        assert_eq!(meta.revision, 0);
        assert_eq!(meta.created_at, 1000);
        assert_eq!(meta.updated_at, 2000);
    }

    #[test]
    fn round_trip_preserves_revision() {
        let meta = RecordMeta {
            source: RecordSource::User,
            hidden: false,
            user_overrides: None,
            created_at: 100,
            updated_at: 200,
            revision: 7,
        };
        let serialized = serde_json::to_value(&meta).unwrap();
        let deserialized: RecordMeta = serde_json::from_value(serialized).unwrap();
        assert_eq!(deserialized.revision, 7);
    }

    #[test]
    fn constructors_default_revision_to_zero() {
        assert_eq!(RecordMeta::legacy_user().revision, 0);
        assert_eq!(RecordMeta::new_user().revision, 0);
        assert_eq!(RecordMeta::new_builtin("1.0.0").revision, 0);
    }
}
