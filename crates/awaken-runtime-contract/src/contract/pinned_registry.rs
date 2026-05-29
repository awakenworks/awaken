//! Minimal runtime registry pinning vocabulary.
//!
//! Runtime only needs the immutable manifest attached to a run. Publication,
//! graph validation, and versioned stores live at the server boundary.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const REGISTRY_KIND_AGENT: &str = "agent";
pub const REGISTRY_KIND_MODEL: &str = "model";
pub const REGISTRY_KIND_MODEL_POOL: &str = "model_pool";
pub const REGISTRY_KIND_PROVIDER: &str = "provider";

/// Pinned published runtime-config graph attached to one run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PinnedRegistryManifest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_snapshot_version: Option<u64>,
    #[serde(default)]
    pub entries: Vec<PinnedRegistryEntry>,
}

/// One published runtime-config version pinned by a run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PinnedRegistryEntry {
    pub kind: String,
    pub id: String,
    pub version: u64,
    pub content_hash: String,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PinnedRegistryHashError {
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
}

pub fn canonical_registry_json_bytes(
    value_schema_version: u32,
    value: &Value,
) -> Result<Vec<u8>, PinnedRegistryHashError> {
    let mut buffer = Vec::with_capacity(64);
    buffer.extend_from_slice(b"{\"value\":");
    write_canonical_json(&mut buffer, value)?;
    buffer.extend_from_slice(b",\"value_schema_version\":");
    write_canonical_json(&mut buffer, &Value::from(value_schema_version))?;
    buffer.push(b'}');
    Ok(buffer)
}

pub fn registry_content_hash(
    value_schema_version: u32,
    value: &Value,
) -> Result<(String, Vec<u8>), PinnedRegistryHashError> {
    let canonical_json_bytes = canonical_registry_json_bytes(value_schema_version, value)?;
    let digest = Sha256::digest(&canonical_json_bytes);
    Ok((format!("sha256:{digest:x}"), canonical_json_bytes))
}

fn write_canonical_json(out: &mut Vec<u8>, value: &Value) -> Result<(), PinnedRegistryHashError> {
    match value {
        Value::Null | Value::Bool(_) | Value::String(_) => serde_json::to_writer(&mut *out, value)
            .map_err(|error| PinnedRegistryHashError::Serialization(error.to_string())),
        Value::Number(number) => {
            if let Some(float) = number.as_f64()
                && !float.is_finite()
            {
                return Err(PinnedRegistryHashError::InvalidRequest(format!(
                    "non-finite number cannot be canonicalized: {float}"
                )));
            }
            serde_json::to_writer(&mut *out, number)
                .map_err(|error| PinnedRegistryHashError::Serialization(error.to_string()))
        }
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_canonical_json(out, item)?;
            }
            out.push(b']');
            Ok(())
        }
        Value::Object(map) => {
            let mut entries: Vec<(Vec<u8>, &Value)> = Vec::with_capacity(map.len());
            for (key, val) in map {
                let mut encoded = Vec::with_capacity(key.len() + 2);
                serde_json::to_writer(&mut encoded, key)
                    .map_err(|error| PinnedRegistryHashError::Serialization(error.to_string()))?;
                entries.push((encoded, val));
            }
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            out.push(b'{');
            for (index, (key_bytes, val)) in entries.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(key_bytes);
                out.push(b':');
                write_canonical_json(out, val)?;
            }
            out.push(b'}');
            Ok(())
        }
    }
}
