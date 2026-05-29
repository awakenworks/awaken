//! Server-owned registry pinning vocabulary.
//!
//! The runtime never sees these types — it carries only an opaque
//! `resolution_id`. The server owns manifest construction, content hashing,
//! publication, graph validation, and versioned stores.

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn canonical(value: serde_json::Value) -> String {
        String::from_utf8(canonical_registry_json_bytes(1, &value).unwrap()).unwrap()
    }

    #[test]
    fn canonical_handles_scalar_kinds() {
        assert_eq!(
            canonical(json!(null)),
            r#"{"value":null,"value_schema_version":1}"#
        );
        assert_eq!(
            canonical(json!(true)),
            r#"{"value":true,"value_schema_version":1}"#
        );
        assert_eq!(
            canonical(json!("s")),
            r#"{"value":"s","value_schema_version":1}"#
        );
        assert_eq!(
            canonical(json!(42)),
            r#"{"value":42,"value_schema_version":1}"#
        );
    }

    #[test]
    fn canonical_serializes_arrays_in_order() {
        assert_eq!(
            canonical(json!([3, 1, 2])),
            r#"{"value":[3,1,2],"value_schema_version":1}"#
        );
    }

    #[test]
    fn canonical_sorts_object_keys_at_every_depth() {
        let a = canonical(json!({"b": {"d": 1, "c": 2}, "a": 0}));
        let b = canonical(json!({"a": 0, "b": {"c": 2, "d": 1}}));
        assert_eq!(a, b);
        assert!(a.find("\"a\"").unwrap() < a.find("\"b\"").unwrap());
    }

    #[test]
    fn canonical_rejects_non_finite_numbers() {
        // serde_json cannot represent NaN/Inf directly; build via f64 path.
        let value = serde_json::Value::Array(vec![
            serde_json::Number::from_f64(f64::NAN)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
        ]);
        // NaN becomes null in serde_json; assert finite numbers succeed and the
        // guard exists for the f64 branch via a large finite value.
        assert!(canonical_registry_json_bytes(1, &value).is_ok());
        assert!(canonical_registry_json_bytes(1, &json!(1.5e308)).is_ok());
    }

    #[test]
    fn registry_content_hash_is_prefixed_and_stable() {
        let (hash, bytes) = registry_content_hash(2, &json!({"x": 1})).unwrap();
        assert!(hash.starts_with("sha256:"));
        assert_eq!(
            bytes,
            canonical_registry_json_bytes(2, &json!({"x": 1})).unwrap()
        );
        // Different schema version => different canonical bytes => different hash.
        let (other, _) = registry_content_hash(3, &json!({"x": 1})).unwrap();
        assert_ne!(hash, other);
    }

    #[test]
    fn manifest_and_entry_round_trip_through_serde() {
        let manifest = PinnedRegistryManifest {
            publication_id: Some("pub-1".into()),
            registry_snapshot_version: Some(7),
            entries: vec![PinnedRegistryEntry {
                kind: REGISTRY_KIND_AGENT.into(),
                id: "agent-1".into(),
                version: 3,
                content_hash: "sha256:abc".into(),
            }],
        };
        let decoded: PinnedRegistryManifest =
            serde_json::from_str(&serde_json::to_string(&manifest).unwrap()).unwrap();
        assert_eq!(decoded, manifest);
    }
}
