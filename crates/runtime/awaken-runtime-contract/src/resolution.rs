//! Provenance for one atomic agent-configuration resolution.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Stable SHA-256 content identity for one serializable resolver input.
pub fn content_fingerprint<T: Serialize + ?Sized>(value: &T) -> Result<String, serde_json::Error> {
    serde_json::to_vec(value).map(|bytes| format!("{:x}", Sha256::digest(bytes)))
}

/// The identity form used to pin one resolver input.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ResolvedInputVersion {
    /// Mutable authoring aggregate generation, used for optimistic concurrency.
    Revision(u64),
    /// Externally addressable immutable publication number.
    PublicationVersion(u64),
    /// Immutable content-addressed resource identity.
    ContentHash(String),
}

/// One exact input consumed while producing an executable agent snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResolvedInputRef {
    /// Extensible bounded-context name (`agent_config`, `skill`, `file`, ...).
    pub kind: String,
    pub id: String,
    pub version: ResolvedInputVersion,
}

/// Complete, canonical provenance of one resolution.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolutionManifest {
    pub inputs: Vec<ResolvedInputRef>,
}

impl ResolutionManifest {
    /// Canonicalize and reject two identities for the same logical input.
    pub fn new(
        inputs: impl IntoIterator<Item = ResolvedInputRef>,
    ) -> Result<Self, ResolutionManifestError> {
        let mut inputs: Vec<_> = inputs.into_iter().collect();
        inputs.sort_by(|left, right| (&left.kind, &left.id).cmp(&(&right.kind, &right.id)));
        for pair in inputs.windows(2) {
            if pair[0].kind == pair[1].kind && pair[0].id == pair[1].id {
                return Err(ResolutionManifestError::Duplicate {
                    kind: pair[0].kind.clone(),
                    id: pair[0].id.clone(),
                });
            }
        }
        Ok(Self { inputs })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolutionManifestError {
    #[error("resolution input {kind}/{id} is pinned more than once")]
    Duplicate { kind: String, id: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(kind: &str, id: &str, revision: u64) -> ResolvedInputRef {
        ResolvedInputRef {
            kind: kind.into(),
            id: id.into(),
            version: ResolvedInputVersion::Revision(revision),
        }
    }

    #[test]
    fn manifest_is_canonical_independent_of_repository_read_order() {
        let left =
            ResolutionManifest::new([input("skill", "s1", 2), input("agent_config", "a1", 7)])
                .unwrap();
        let right =
            ResolutionManifest::new([input("agent_config", "a1", 7), input("skill", "s1", 2)])
                .unwrap();
        assert_eq!(left, right);
    }

    #[test]
    fn manifest_rejects_mixed_versions_of_one_input() {
        let error = ResolutionManifest::new([
            input("agent_config", "a1", 7),
            input("agent_config", "a1", 8),
        ])
        .unwrap_err();
        assert_eq!(
            error,
            ResolutionManifestError::Duplicate {
                kind: "agent_config".into(),
                id: "a1".into(),
            }
        );
    }

    #[test]
    fn content_fingerprint_is_stable_and_content_sensitive() {
        assert_eq!(
            content_fingerprint(&serde_json::json!({"a": 1})).unwrap(),
            content_fingerprint(&serde_json::json!({"a": 1})).unwrap()
        );
        assert_ne!(
            content_fingerprint(&serde_json::json!({"a": 1})).unwrap(),
            content_fingerprint(&serde_json::json!({"a": 2})).unwrap()
        );
    }
}
