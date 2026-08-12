use serde::{Deserialize, Serialize};

use crate::resolution::ResolutionManifest;
use crate::resolved::Backend;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExecutableAgentSnapshotId(pub String);

/// The exact mutable source revision an executable snapshot was resolved from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentConfigRevisionRef {
    pub agent_id: AgentId,
    pub revision: u64,
}

impl Default for AgentConfigRevisionRef {
    fn default() -> Self {
        Self {
            agent_id: AgentId(String::new()),
            revision: 0,
        }
    }
}

/// Externally addressable immutable publication number.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentPublicationVersion(pub String);

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

impl Default for AgentSnapshotMetadata {
    fn default() -> Self {
        Self {
            source: AgentConfigRevisionRef::default(),
            publication_version: AgentPublicationVersion(String::new()),
            resolution: ResolutionManifest::default(),
            fingerprint: AgentSnapshotFingerprint(String::new()),
        }
    }
}

impl AgentSnapshotMetadata {
    #[must_use]
    pub fn is_legacy_default(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutableAgentSnapshot {
    pub id: ExecutableAgentSnapshotId,
    /// Source identity and the complete set of configuration inputs resolved once
    /// by the configuration plane. Legacy/directly-built snapshots default this
    /// field; published snapshots must carry a non-empty manifest.
    #[serde(
        default,
        skip_serializing_if = "AgentSnapshotMetadata::is_legacy_default"
    )]
    pub metadata: AgentSnapshotMetadata,
    pub root_agent_id: AgentId,
    pub resolved_spec: crate::resolved::ResolvedSpec,
    pub fingerprint: crate::resolved::CatalogFingerprint,
}

impl ExecutableAgentSnapshot {
    /// The embedded SDK executes the same snapshot contract as the server, but
    /// only through the native Awaken loop. Validate every published fallback so
    /// an edit cannot accidentally retain an ACP/A2A execution branch.
    pub fn validate_embedded_native(&self) -> Result<(), String> {
        for candidate in std::iter::once(&self.resolved_spec.model_binding)
            .chain(self.resolved_spec.model_candidates.iter())
            .chain(
                self.resolved_spec
                    .plugin_config
                    .agent
                    .advisor
                    .as_ref()
                    .map(|advisor| &advisor.candidate),
            )
        {
            if !matches!(
                Backend::from_ref(&candidate.binding.backend_ref),
                Backend::Native
            ) {
                return Err(format!(
                    "embedded Awaken Runtime does not support backend `{}`",
                    candidate.binding.backend_ref
                ));
            }
        }
        Ok(())
    }

    /// Re-derive local content identity after an exported snapshot is edited.
    /// Cloud provenance remains in `source`/`publication_version`, while all
    /// executable-fingerprint fields move together to one local identity.
    pub fn recompute_fingerprint(&mut self) -> Result<(), serde_json::Error> {
        let mut content = self.clone();
        content.fingerprint.0.clear();
        content.resolved_spec.catalog_fingerprint.0.clear();
        content.metadata.fingerprint.0.clear();
        let fingerprint = crate::resolution::content_fingerprint(&content)?;
        self.fingerprint.0.clone_from(&fingerprint);
        self.resolved_spec
            .catalog_fingerprint
            .0
            .clone_from(&fingerprint);
        if !self.metadata.is_legacy_default() {
            self.metadata.fingerprint.0 = fingerprint;
        }
        Ok(())
    }
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
            publication_version: AgentPublicationVersion("v3".into()),
            resolution,
            fingerprint: AgentSnapshotFingerprint("sha256:abc".into()),
        };
        let wire = serde_json::to_value(&metadata).unwrap();
        assert_eq!(wire["source"]["revision"], 7);
        assert_eq!(wire["publication_version"], "v3");
        assert_eq!(wire["fingerprint"], "sha256:abc");
        assert_eq!(
            wire["resolution"]["inputs"][0]["version"]["type"],
            "revision"
        );
    }

    /// Cause/effect rules: R1 all-native candidates => embedded-compatible;
    /// R2 any ACP/A2A candidate => reject the entire snapshot; R3 model edit =>
    /// recompute one matching envelope/spec/metadata fingerprint.
    #[test]
    fn embedded_compatibility_and_local_identity_are_single_snapshot_operations() {
        let mut snapshot = ExecutableAgentSnapshot::builder("agent-1")
            .model(crate::resolved::ModelBinding {
                provider_identity_ref: "local".into(),
                model_ref: "model-a".into(),
                backend_ref: "genai".into(),
            })
            .build();
        snapshot.validate_embedded_native().unwrap();
        let before = snapshot.fingerprint.clone();
        snapshot.resolved_spec.model_binding.binding.model_ref = "model-b".into();
        snapshot.recompute_fingerprint().unwrap();
        assert_ne!(snapshot.fingerprint, before);
        assert_eq!(
            snapshot.fingerprint,
            snapshot.resolved_spec.catalog_fingerprint
        );

        snapshot.resolved_spec.model_binding.binding.backend_ref = "acp:claude".into();
        assert!(
            snapshot
                .validate_embedded_native()
                .unwrap_err()
                .contains("acp:claude")
        );
    }
}
