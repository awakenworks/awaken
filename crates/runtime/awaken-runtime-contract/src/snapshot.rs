use serde::{Deserialize, Serialize};

use crate::resolution::ResolutionManifest;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExecutableAgentSnapshotId(pub String);

/// The exact mutable source revision an executable snapshot was resolved from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentConfigRevisionRef {
    pub agent_id: AgentId,
    pub revision: u64,
}

/// Externally addressable immutable publication number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentPublicationVersion(pub u64);

/// Content identity of the complete executable snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentSnapshotFingerprint(pub String);

/// Provenance carried by the next snapshot wire shape while the existing snapshot
/// is migrated without introducing a second execution path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSnapshotMetadata {
    pub source: AgentConfigRevisionRef,
    pub publication_version: AgentPublicationVersion,
    pub resolution: ResolutionManifest,
    pub fingerprint: AgentSnapshotFingerprint,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutableAgentSnapshot {
    pub id: ExecutableAgentSnapshotId,
    pub root_agent_id: AgentId,
    pub resolved_spec: crate::resolved::ResolvedSpec,
    pub fingerprint: crate::resolved::CatalogFingerprint,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId(pub String);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AgentSnapshotInput {
    /// The full snapshot inline. Boxed because it dwarfs the by-id variant, the
    /// common path, which should not carry the inline payload's stack size.
    Inline(Box<ExecutableAgentSnapshot>),
    ById(ExecutableAgentSnapshotId),
}

#[cfg(test)]
mod metadata_tests {
    use super::*;
    use crate::resolution::{ResolvedInputRef, ResolvedInputVersion};

    #[test]
    fn metadata_keeps_revision_version_and_content_identity_distinct() {
        let resolution = ResolutionManifest::new([ResolvedInputRef {
            kind: "agent_config".into(),
            id: "agent-1".into(),
            version: ResolvedInputVersion::Revision(7),
        }])
        .unwrap();
        let metadata = AgentSnapshotMetadata {
            source: AgentConfigRevisionRef {
                agent_id: AgentId("agent-1".into()),
                revision: 7,
            },
            publication_version: AgentPublicationVersion(3),
            resolution,
            fingerprint: AgentSnapshotFingerprint("sha256:abc".into()),
        };
        let wire = serde_json::to_value(&metadata).unwrap();
        assert_eq!(wire["source"]["revision"], 7);
        assert_eq!(wire["publication_version"], 3);
        assert_eq!(wire["fingerprint"], "sha256:abc");
        assert_eq!(
            wire["resolution"]["inputs"][0]["version"]["type"],
            "revision"
        );
    }
}
