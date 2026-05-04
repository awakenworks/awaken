//! Metadata envelope wrapping any spec stored in ConfigStore.
//!
//! Today most ConfigStore entries are bare specs (e.g. `AgentSpec` JSON).
//! Phase 1 introduces this envelope so we can carry provenance (was this
//! seeded by the binary, or written by a user?) and lifecycle flags
//! (`hidden`) without breaking existing on-disk data. The decoder accepts
//! both shapes; the encoder always emits the envelope.

use serde::{Deserialize, Serialize};

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
    /// Milliseconds since UNIX epoch (see `crate::time::now_ms`).
    /// `0` is a sentinel meaning "unknown / pre-envelope legacy entry".
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub updated_at: u64,
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
            created_at: 0,
            updated_at: 0,
        }
    }

    /// Construct a fresh User record with current timestamps.
    pub fn new_user() -> Self {
        let now = crate::time::now_ms();
        Self {
            source: RecordSource::User,
            hidden: false,
            created_at: now,
            updated_at: now,
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
            created_at: now,
            updated_at: now,
        }
    }
}

fn is_envelope(value: &serde_json::Value) -> bool {
    matches!(value, serde_json::Value::Object(map) if map.contains_key("spec") && map.contains_key("meta"))
}
