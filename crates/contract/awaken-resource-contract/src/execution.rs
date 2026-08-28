//! Protocol-neutral execution ports into the Resources authority.
//!
//! `C` is the caller-owned fencing context. The Resources contract never knows
//! whether it is a durable Run claim, an embedded marker, or another transport
//! capability; remote adapters interpret it and local adapters may ignore it.

use async_trait::async_trait;
use sha2::{Digest as _, Sha256};

use crate::{ConfigVersion, FileRecord, ResourceAccess, content_id, harvest_idempotency_key};

/// Why immutable File bytes are being opened. A remote Resources adapter uses
/// this value only to select the corresponding trusted-reference proof; it does
/// not accept the requested `file_id` itself as authority.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FileReadPurpose {
    SessionResource,
    ModelContent { thread_id: String },
}

/// One exact logical File resolved to immutable content. Metadata travels with
/// the bytes so a model-content adapter never guesses a MIME type or filename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFileContent {
    pub file_id: String,
    pub content_id: String,
    pub filename: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("file content source: {0}")]
pub struct FileContentSourceError(String);

impl FileContentSourceError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

#[async_trait]
pub trait FileContentSource<C: Sync = ()>: Send + Sync {
    async fn read(
        &self,
        workspace_id: &str,
        file_id: &str,
        purpose: &FileReadPurpose,
        fence: Option<&C>,
    ) -> Result<Option<ResolvedFileContent>, FileContentSourceError>;
}

pub struct UnavailableFileContentSource;

#[async_trait]
impl<C: Sync> FileContentSource<C> for UnavailableFileContentSource {
    async fn read(
        &self,
        _workspace_id: &str,
        _file_id: &str,
        _purpose: &FileReadPurpose,
        _fence: Option<&C>,
    ) -> Result<Option<ResolvedFileContent>, FileContentSourceError> {
        Err(FileContentSourceError::new(
            "File content source is not configured by the composition root",
        ))
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("Repository binding verifier: {0}")]
pub struct RepositoryBindingVerifierError(String);

impl RepositoryBindingVerifierError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Deployment-selected transport for one already-authorized Repository.
///
/// `Direct` preserves the self-hosted Worker injection path. `GatewayMediated`
/// carries only a short platform capability; it never carries the upstream PAT.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RepositoryTransport {
    Direct,
    GatewayMediated {
        remote_url: String,
        capability: RepositoryGatewayCapability,
    },
}

impl std::fmt::Debug for RepositoryTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Direct => formatter.write_str("Direct"),
            Self::GatewayMediated { remote_url, .. } => formatter
                .debug_struct("GatewayMediated")
                .field("remote_url", remote_url)
                .field("capability", &"[redacted]")
                .finish(),
        }
    }
}

/// Short-lived Gateway capability. Debug output is always redacted.
#[derive(Clone, PartialEq, Eq, serde::Serialize)]
#[serde(transparent)]
pub struct RepositoryGatewayCapability(String);

impl RepositoryGatewayCapability {
    pub fn new(value: impl Into<String>) -> Result<Self, RepositoryBindingVerifierError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(RepositoryBindingVerifierError::new(
                "Gateway capability must not be empty",
            ));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for RepositoryGatewayCapability {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RepositoryGatewayCapability([redacted])")
    }
}

impl<'de> serde::Deserialize<'de> for RepositoryGatewayCapability {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <String as serde::Deserialize>::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[async_trait]
pub trait RepositoryBindingVerifier<C: Sync = ()>: Send + Sync {
    async fn verify(
        &self,
        workspace_id: &str,
        repository_id: &str,
        config_version: ConfigVersion,
        fence: Option<&C>,
    ) -> Result<RepositoryTransport, RepositoryBindingVerifierError>;
}

#[derive(Debug, Clone)]
pub struct ArtifactPublication<C> {
    /// Stable effect identity authored by the harvesting boundary before I/O.
    pub effect_id: String,
    pub workspace_id: String,
    pub session_id: String,
    pub logical_path: String,
    pub mime_type: String,
    /// Digest observed while the Sandbox was still fenced and live.
    pub content_id: String,
    pub bytes: Vec<u8>,
    pub fence: Option<C>,
}

impl<C> ArtifactPublication<C> {
    pub fn verify(&self) -> Result<(), ArtifactPublicationError> {
        let actual = content_id(&self.bytes);
        let expected_effect =
            harvest_idempotency_key(&self.session_id, &self.logical_path, &self.content_id);
        if self.content_id != actual {
            return Err(ArtifactPublicationError::new(
                "artifact bytes do not match the asserted content id",
            ));
        }
        if self.effect_id != expected_effect {
            return Err(ArtifactPublicationError::new(
                "artifact effect id does not match its exact publication intent",
            ));
        }
        Ok(())
    }
}

/// Durable Resources-owned evidence for one artifact publication effect.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArtifactPublicationReceipt {
    pub effect_id: String,
    pub content_id: String,
    pub record: FileRecord,
}

/// Why an exact three-artifact completion bundle was authored. Skill export
/// and terminal PatchBundle publication share the same Resources-owned receipt
/// and differ only in their bounded-context purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactBundlePurpose {
    SkillExport,
    PatchBundle,
}

/// Complete, immutable receipt for `patch + manifest + SHA256SUMS`.
/// Artifact ids remain the existing File aggregate identities; this value owns
/// no storage, queue, or lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArtifactBundleCompletionReceipt {
    pub purpose: ArtifactBundlePurpose,
    pub patch_sha256: String,
    pub patch_artifact_id: String,
    pub manifest_artifact_id: String,
    pub checksum_artifact_id: String,
    pub receipt_fingerprint: String,
}

#[must_use]
const fn artifact_bundle_completion_admitted(evidence: ArtifactBundleEvidence) -> bool {
    evidence.exact_receipts
        && evidence.same_workspace
        && evidence.same_session
        && evidence.distinct_effects
        && evidence.expected_paths
        && evidence.manifest_nonempty
        && evidence.checksum_exact
        && evidence.downloadable
}

#[derive(Clone, Copy)]
struct ArtifactBundleEvidence {
    exact_receipts: bool,
    same_workspace: bool,
    same_session: bool,
    distinct_effects: bool,
    expected_paths: bool,
    manifest_nonempty: bool,
    checksum_exact: bool,
    downloadable: bool,
}

fn artifact_file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Verify and complete one Artifact bundle only after all three ordinary
/// Artifact publications have exact FileStore receipts.
pub fn complete_artifact_bundle<C>(
    purpose: ArtifactBundlePurpose,
    patch: (&ArtifactPublication<C>, &ArtifactPublicationReceipt),
    manifest: (&ArtifactPublication<C>, &ArtifactPublicationReceipt),
    checksum: (&ArtifactPublication<C>, &ArtifactPublicationReceipt),
) -> Result<ArtifactBundleCompletionReceipt, ArtifactPublicationError> {
    let publications = [patch.0, manifest.0, checksum.0];
    let receipts = [patch.1, manifest.1, checksum.1];
    let exact_receipts = receipts
        .iter()
        .zip(publications.iter())
        .all(|(receipt, publication)| receipt.verify(*publication).is_ok());
    let same_workspace = publications
        .iter()
        .all(|publication| publication.workspace_id == patch.0.workspace_id);
    let same_session = publications
        .iter()
        .all(|publication| publication.session_id == patch.0.session_id);
    let distinct_effects = {
        let effects = [
            receipts[0].effect_id.as_str(),
            receipts[1].effect_id.as_str(),
            receipts[2].effect_id.as_str(),
        ];
        effects[0] != effects[1] && effects[0] != effects[2] && effects[1] != effects[2]
    };
    let patch_name = artifact_file_name(&patch.0.logical_path);
    let expected_paths = patch_name.ends_with(".patch")
        && artifact_file_name(&manifest.0.logical_path) == "manifest.json"
        && artifact_file_name(&checksum.0.logical_path) == "SHA256SUMS";
    let patch_sha256 = format!("sha256:{:x}", Sha256::digest(&patch.0.bytes));
    let expected_checksum = format!("{}  {}\n", patch_sha256, patch_name);
    let checksum_exact = checksum.0.bytes == expected_checksum.as_bytes();
    let downloadable = receipts
        .iter()
        .all(|receipt| receipt.record.downloadable && !receipt.record.deleted);
    if !artifact_bundle_completion_admitted(ArtifactBundleEvidence {
        exact_receipts,
        same_workspace,
        same_session,
        distinct_effects,
        expected_paths,
        manifest_nonempty: !manifest.0.bytes.is_empty(),
        checksum_exact,
        downloadable,
    }) {
        return Err(ArtifactPublicationError::new(
            "artifact bundle is incomplete or does not match its exact publications",
        ));
    }
    let ids = [
        receipts[0].record.id.as_str(),
        receipts[1].record.id.as_str(),
        receipts[2].record.id.as_str(),
    ];
    let mut fingerprint = blake3::Hasher::new();
    for component in [
        match purpose {
            ArtifactBundlePurpose::SkillExport => "skill_export",
            ArtifactBundlePurpose::PatchBundle => "patch_bundle",
        },
        patch_sha256.as_str(),
        ids[0],
        ids[1],
        ids[2],
    ] {
        fingerprint.update(&(component.len() as u64).to_be_bytes());
        fingerprint.update(component.as_bytes());
    }
    Ok(ArtifactBundleCompletionReceipt {
        purpose,
        patch_sha256,
        patch_artifact_id: ids[0].to_string(),
        manifest_artifact_id: ids[1].to_string(),
        checksum_artifact_id: ids[2].to_string(),
        receipt_fingerprint: fingerprint.finalize().to_hex().to_string(),
    })
}

#[cfg(kani)]
#[kani::proof]
fn artifact_bundle_completion_requires_every_evidence_axis() {
    let exact_receipts: bool = kani::any();
    let same_workspace: bool = kani::any();
    let same_session: bool = kani::any();
    let distinct_effects: bool = kani::any();
    let expected_paths: bool = kani::any();
    let manifest_nonempty: bool = kani::any();
    let checksum_exact: bool = kani::any();
    let downloadable: bool = kani::any();
    assert_eq!(
        artifact_bundle_completion_admitted(ArtifactBundleEvidence {
            exact_receipts,
            same_workspace,
            same_session,
            distinct_effects,
            expected_paths,
            manifest_nonempty,
            checksum_exact,
            downloadable,
        }),
        exact_receipts
            && same_workspace
            && same_session
            && distinct_effects
            && expected_paths
            && manifest_nonempty
            && checksum_exact
            && downloadable
    );
}

impl ArtifactPublicationReceipt {
    pub fn verify<C>(
        &self,
        publication: &ArtifactPublication<C>,
    ) -> Result<(), ArtifactPublicationError> {
        publication.verify()?;
        if self.effect_id == publication.effect_id
            && self.content_id == publication.content_id
            && self.record.blob_id == publication.content_id
            && self.record.workspace_id == publication.workspace_id
            && self.record.scope_id.as_deref() == Some(publication.session_id.as_str())
            && self.record.logical_path.as_deref() == Some(publication.logical_path.as_str())
            && self.record.harvest_key.as_deref() == Some(publication.effect_id.as_str())
        {
            Ok(())
        } else {
            Err(ArtifactPublicationError::new(
                "artifact receipt does not match its exact publication intent",
            ))
        }
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("artifact publication error: {0}")]
pub struct ArtifactPublicationError(String);

impl ArtifactPublicationError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

#[async_trait]
pub trait ArtifactPublisher<C: Send + Sync + 'static = ()>: Send + Sync {
    async fn publish(
        &self,
        publication: ArtifactPublication<C>,
    ) -> Result<ArtifactPublicationReceipt, ArtifactPublicationError>;
}

pub struct UnavailableArtifactPublisher;

#[async_trait]
impl<C: Send + Sync + 'static> ArtifactPublisher<C> for UnavailableArtifactPublisher {
    async fn publish(
        &self,
        _publication: ArtifactPublication<C>,
    ) -> Result<ArtifactPublicationReceipt, ArtifactPublicationError> {
        Err(ArtifactPublicationError::new(
            "artifact publisher is not configured by the composition root",
        ))
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[error("Memory materialization reference: {0}")]
pub struct MemoryMaterializationReferenceError(String);

impl MemoryMaterializationReferenceError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Encode one process-local Memory mount reference from a caller-owned fence.
/// Only a transport adapter defines its wire representation.
pub trait MemoryMaterializationReferenceEncoder<C>: Send + Sync {
    fn encode(
        &self,
        workspace_id: &str,
        memory_store_id: &str,
        config_version: ConfigVersion,
        access: ResourceAccess,
        fence: &C,
    ) -> Result<String, MemoryMaterializationReferenceError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact(
        path: &str,
        bytes: &[u8],
        id: &str,
    ) -> (ArtifactPublication<()>, ArtifactPublicationReceipt) {
        let content_id = crate::content_id(bytes);
        let effect_id = crate::harvest_idempotency_key("session-1", path, &content_id);
        let publication = ArtifactPublication {
            effect_id: effect_id.clone(),
            workspace_id: "workspace-1".into(),
            session_id: "session-1".into(),
            logical_path: path.into(),
            mime_type: "application/octet-stream".into(),
            content_id: content_id.clone(),
            bytes: bytes.to_vec(),
            fence: None,
        };
        let receipt = ArtifactPublicationReceipt {
            effect_id: effect_id.clone(),
            content_id: content_id.clone(),
            record: FileRecord {
                id: id.into(),
                workspace_id: "workspace-1".into(),
                blob_id: content_id,
                filename: artifact_file_name(path).into(),
                mime_type: "application/octet-stream".into(),
                size_bytes: bytes.len() as u64,
                created_at: "2026-08-28T00:00:00Z".into(),
                expires_at: None,
                downloadable: true,
                scope_id: Some("session-1".into()),
                logical_path: Some(path.into()),
                harvest_key: Some(effect_id),
                deleted: false,
            },
        };
        (publication, receipt)
    }

    #[test]
    fn artifact_bundle_completion_is_all_or_nothing() {
        // Cause/effect graph: C1 patch/manifest/checksum publications each have
        // an exact File receipt; C2 workspace/session/effect identities agree;
        // C3 filenames and SHA256SUMS are canonical; C4 every artifact remains
        // downloadable. Effects: E1 a completion receipt contains SHA-256 and
        // all three Artifact ids; E2 omission, substitution, or checksum drift
        // fails explicitly. Rule B1 all causes=>E1; B2 any cause false=>E2.
        let patch = artifact(
            "exports/change.patch",
            b"diff --git a/a b/a\n",
            "file-patch",
        );
        let digest = format!("sha256:{:x}", Sha256::digest(&patch.0.bytes));
        let manifest = artifact(
            "exports/manifest.json",
            b"{\"base\":\"abc\"}\n",
            "file-manifest",
        );
        let checksum = artifact(
            "exports/SHA256SUMS",
            format!("{digest}  change.patch\n").as_bytes(),
            "file-checksum",
        );
        let completed = complete_artifact_bundle(
            ArtifactBundlePurpose::SkillExport,
            (&patch.0, &patch.1),
            (&manifest.0, &manifest.1),
            (&checksum.0, &checksum.1),
        )
        .expect("B1/E1");
        assert_eq!(completed.patch_sha256, digest, "B1/E1");
        assert_eq!(completed.patch_artifact_id, "file-patch", "B1/E1");
        assert_eq!(completed.manifest_artifact_id, "file-manifest", "B1/E1");
        assert_eq!(completed.checksum_artifact_id, "file-checksum", "B1/E1");

        let bad_checksum = artifact(
            "exports/SHA256SUMS",
            b"sha256:substituted  change.patch\n",
            "file-bad-checksum",
        );
        assert!(
            complete_artifact_bundle(
                ArtifactBundlePurpose::PatchBundle,
                (&patch.0, &patch.1),
                (&manifest.0, &manifest.1),
                (&bad_checksum.0, &bad_checksum.1),
            )
            .is_err(),
            "B2/E2"
        );
    }

    #[test]
    fn repository_gateway_capability_is_nonempty_and_redacted() {
        // Cause/effect decision table: R1 nonempty wire value -> strong
        // capability with redacted Debug; R2 empty/blank value -> reject at
        // decode so no Worker or Gateway caller can acquire a weak token.
        let capability: RepositoryGatewayCapability =
            serde_json::from_str("\"short-lived-capability\"").expect("R1");
        assert_eq!(capability.expose(), "short-lived-capability");
        assert!(!format!("{capability:?}").contains("short-lived-capability"));
        for invalid in ["\"\"", "\"   \""] {
            assert!(
                serde_json::from_str::<RepositoryGatewayCapability>(invalid).is_err(),
                "R2"
            );
        }
    }
}
