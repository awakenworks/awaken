//! Protocol-neutral execution ports into the Resources authority.
//!
//! `C` is the caller-owned fencing context. The Resources contract never knows
//! whether it is a durable Run claim, an embedded marker, or another transport
//! capability; remote adapters interpret it and local adapters may ignore it.

use crate::{ConfigVersion, FileRecord, ResourceAccess, content_id, harvest_idempotency_key};
use async_trait::async_trait;

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
        /// Exact issuer-owned expiry. Older one-shot host-operation consumers
        /// may omit it; long-lived workload consumers must require it at their
        /// own boundary and fail closed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_at_unix_ms: Option<RepositoryGatewayCapabilityExpiry>,
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

/// Issuer-reported wall-clock expiry of one Gateway capability.
///
/// Zero is rejected at construction and deserialization. Dynamic liveness and
/// the current claim/lease upper bound are enforced by the Worker authority
/// boundary because only that boundary owns the current clock and fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(transparent)]
pub struct RepositoryGatewayCapabilityExpiry(u64);

impl RepositoryGatewayCapabilityExpiry {
    pub fn new(value: u64) -> Result<Self, RepositoryBindingVerifierError> {
        if value == 0 {
            return Err(RepositoryBindingVerifierError::new(
                "Gateway capability expiry must be positive",
            ));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub const fn unix_ms(self) -> u64 {
        self.0
    }

    #[must_use]
    pub const fn is_live_at(self, now_unix_ms: u64) -> bool {
        self.0 > now_unix_ms
    }
}

impl<'de> serde::Deserialize<'de> for RepositoryGatewayCapabilityExpiry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <u64 as serde::Deserialize>::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
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
    /// Optional caller-owned idempotency namespace. Ordinary Run harvests keep
    /// this absent and therefore retain their v1 identity byte-for-byte;
    /// terminal cleanup supplies its durable cleanup operation id.
    pub idempotency_scope: Option<String>,
    pub fence: Option<C>,
}

/// One terminal-only readback request for already-durable artifact receipts.
///
/// The fence stays opaque to Resources. Driving adapters must admit only the
/// exact terminal effect whose operation id equals `idempotency_scope` before
/// invoking the File application. The application remains the sole owner of
/// the durable catalog read and receipt reconstruction.
#[derive(Debug, Clone)]
pub struct ArtifactRecovery<C> {
    pub workspace_id: String,
    pub session_id: String,
    pub idempotency_scope: String,
    pub fence: C,
}

impl<C> ArtifactPublication<C> {
    /// Project the canonical execution publication into the Resources File
    /// application. Execution fencing remains owned by the driving adapter;
    /// every File-owned fact is preserved without introducing another command
    /// value or argument list.
    #[must_use]
    pub fn into_file_application(self) -> ArtifactPublication<()> {
        ArtifactPublication {
            effect_id: self.effect_id,
            workspace_id: self.workspace_id,
            session_id: self.session_id,
            logical_path: self.logical_path,
            mime_type: self.mime_type,
            content_id: self.content_id,
            bytes: self.bytes,
            idempotency_scope: self.idempotency_scope,
            fence: None,
        }
    }

    pub fn verify(&self) -> Result<(), ArtifactPublicationError> {
        let actual = content_id(&self.bytes);
        let expected_effect =
            harvest_idempotency_key(&self.session_id, &self.logical_path, &self.content_id);
        if self
            .idempotency_scope
            .as_deref()
            .is_some_and(|scope| scope.trim().is_empty())
        {
            return Err(ArtifactPublicationError::new(
                "artifact idempotency scope must be nonempty",
            ));
        }
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

impl<C> ArtifactRecovery<C> {
    pub fn verify(&self) -> Result<(), ArtifactPublicationError> {
        if self.workspace_id.trim().is_empty()
            || self.session_id.trim().is_empty()
            || self.idempotency_scope.trim().is_empty()
        {
            return Err(ArtifactPublicationError::new(
                "artifact recovery requires an exact Workspace, Session, and idempotency scope",
            ));
        }
        Ok(())
    }

    /// Reconstruct canonical receipts from the existing File aggregate view.
    /// Nonmatching ordinary/older-terminal records are ignored; conflicting
    /// duplicate evidence fails closed instead of choosing one arbitrarily.
    pub fn receipts_from_records(
        &self,
        records: impl IntoIterator<Item = FileRecord>,
    ) -> Result<Vec<ArtifactPublicationReceipt>, ArtifactPublicationError> {
        self.verify()?;
        let mut receipts = records
            .into_iter()
            .filter_map(|mut record| {
                let logical_path = record.logical_path.as_deref()?;
                let effect_id =
                    harvest_idempotency_key(&self.session_id, logical_path, &record.blob_id);
                (record.workspace_id == self.workspace_id
                    && record.scope_id.as_deref() == Some(self.session_id.as_str())
                    && record.harvest_key.as_deref() == Some(effect_id.as_str())
                    && record.artifact_idempotency_scope.as_deref()
                        == Some(self.idempotency_scope.as_str()))
                .then(|| {
                    // `deleted` describes the current logical File lifecycle,
                    // not the original publication effect. Normalize it to the
                    // creation receipt so response-loss readback is byte-for-byte
                    // equal before and after a later logical delete.
                    record.deleted = false;
                    ArtifactPublicationReceipt {
                        effect_id,
                        content_id: record.blob_id.clone(),
                        record,
                    }
                })
            })
            .collect::<Vec<_>>();
        if !ArtifactPublicationReceipt::canonicalize(&mut receipts) {
            return Err(ArtifactPublicationError::new(
                "artifact recovery found duplicate durable effect evidence",
            ));
        }
        Ok(receipts)
    }

    pub fn verify_receipts(
        &self,
        receipts: &[ArtifactPublicationReceipt],
    ) -> Result<(), ArtifactPublicationError> {
        let canonical = self.receipts_from_records(
            receipts
                .iter()
                .map(|receipt| receipt.record.clone())
                .collect::<Vec<_>>(),
        )?;
        if canonical == receipts {
            Ok(())
        } else {
            Err(ArtifactPublicationError::new(
                "artifact recovery response is not canonical",
            ))
        }
    }
}

/// Durable Resources-owned evidence for one artifact publication effect.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArtifactPublicationReceipt {
    pub effect_id: String,
    pub content_id: String,
    pub record: FileRecord,
}

#[derive(Clone, Copy)]
struct ArtifactReceiptEvidence {
    exact_effect: bool,
    exact_content: bool,
    exact_blob: bool,
    same_workspace: bool,
    same_session: bool,
    same_path: bool,
    exact_harvest_key: bool,
    exact_terminal_scope: bool,
}

#[must_use]
const fn artifact_receipt_admitted(evidence: ArtifactReceiptEvidence) -> bool {
    evidence.exact_effect
        && evidence.exact_content
        && evidence.exact_blob
        && evidence.same_workspace
        && evidence.same_session
        && evidence.same_path
        && evidence.exact_harvest_key
        && evidence.exact_terminal_scope
}

#[cfg(kani)]
#[kani::proof]
fn artifact_publication_receipt_requires_every_identity_axis() {
    let evidence = ArtifactReceiptEvidence {
        exact_effect: kani::any(),
        exact_content: kani::any(),
        exact_blob: kani::any(),
        same_workspace: kani::any(),
        same_session: kani::any(),
        same_path: kani::any(),
        exact_harvest_key: kani::any(),
        exact_terminal_scope: kani::any(),
    };
    assert_eq!(
        artifact_receipt_admitted(evidence),
        evidence.exact_effect
            && evidence.exact_content
            && evidence.exact_blob
            && evidence.same_workspace
            && evidence.same_session
            && evidence.same_path
            && evidence.exact_harvest_key
            && evidence.exact_terminal_scope
    );
}

impl ArtifactPublicationReceipt {
    /// Canonicalize one receipt set at its domain owner. Every caller uses the
    /// same effect-id order and duplicate rule; transport recovery and Session
    /// completion must not maintain parallel ordering policies.
    pub fn canonicalize(receipts: &mut [Self]) -> bool {
        receipts.sort_by(|left, right| left.effect_id.cmp(&right.effect_id));
        receipts
            .windows(2)
            .all(|pair| pair[0].effect_id != pair[1].effect_id)
    }

    pub fn from_publication_record<C>(
        publication: &ArtifactPublication<C>,
        mut record: FileRecord,
    ) -> Result<Self, ArtifactPublicationError> {
        // A tombstone is a later File lifecycle fact. Receipt evidence always
        // represents the original successful publication, so response-loss
        // readback must normalize it to the creation shape.
        record.deleted = false;
        let receipt = Self {
            effect_id: publication.effect_id.clone(),
            content_id: publication.content_id.clone(),
            record,
        };
        receipt.verify(publication)?;
        Ok(receipt)
    }

    pub fn verify<C>(
        &self,
        publication: &ArtifactPublication<C>,
    ) -> Result<(), ArtifactPublicationError> {
        publication.verify()?;
        if artifact_receipt_admitted(ArtifactReceiptEvidence {
            exact_effect: self.effect_id == publication.effect_id,
            exact_content: self.content_id == publication.content_id,
            exact_blob: self.record.blob_id == publication.content_id,
            same_workspace: self.record.workspace_id == publication.workspace_id,
            same_session: self.record.scope_id.as_deref() == Some(publication.session_id.as_str()),
            same_path: self.record.logical_path.as_deref()
                == Some(publication.logical_path.as_str()),
            exact_harvest_key: self.record.harvest_key.as_deref()
                == Some(publication.effect_id.as_str()),
            exact_terminal_scope: publication
                .idempotency_scope
                .as_deref()
                .is_none_or(|scope| {
                    self.record.artifact_idempotency_scope.as_deref() == Some(scope)
                }),
        }) {
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

    async fn recover(
        &self,
        _recovery: ArtifactRecovery<C>,
    ) -> Result<Vec<ArtifactPublicationReceipt>, ArtifactPublicationError> {
        Err(ArtifactPublicationError::new(
            "artifact recovery is not configured by the composition root",
        ))
    }
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

    fn artifact() -> (ArtifactPublication<()>, ArtifactPublicationReceipt) {
        let bytes = b"ordinary output".to_vec();
        let content_id = crate::content_id(&bytes);
        let effect_id =
            crate::harvest_idempotency_key("session-1", "outputs/change.patch", &content_id);
        let publication = ArtifactPublication {
            effect_id: effect_id.clone(),
            workspace_id: "workspace-1".into(),
            session_id: "session-1".into(),
            logical_path: "outputs/change.patch".into(),
            mime_type: "text/x-diff".into(),
            content_id: content_id.clone(),
            bytes,
            idempotency_scope: None,
            fence: None,
        };
        let receipt = ArtifactPublicationReceipt {
            effect_id: effect_id.clone(),
            content_id: content_id.clone(),
            record: FileRecord {
                id: "file-1".into(),
                workspace_id: "workspace-1".into(),
                blob_id: content_id,
                filename: "change.patch".into(),
                mime_type: "text/x-diff".into(),
                size_bytes: publication.bytes.len() as u64,
                created_at: "2026-08-29T00:00:00Z".into(),
                expires_at: None,
                downloadable: true,
                scope_id: Some("session-1".into()),
                logical_path: Some("outputs/change.patch".into()),
                harvest_key: Some(effect_id),
                artifact_idempotency_scope: None,
                deleted: false,
            },
        };
        (publication, receipt)
    }

    #[test]
    fn ordinary_artifact_receipt_requires_the_exact_publication() {
        // Causes: C1 bytes still match their content id; C2 the publication's
        // effect id is the canonical Session/path/content key; C3 optional
        // terminal scope is absent or nonblank; C4 every receipt identity axis
        // (effect, content, blob, Workspace, Session, path, and harvest key)
        // matches. Effect E1 is an admitted ordinary File receipt; E2 is an
        // explicit rejection with no secondary aggregate completion. Decision
        // rules: A1=C1+C2+C3+C4=>E1; A2=any false cause=>E2.
        let (publication, receipt) = artifact();
        receipt.verify(&publication).expect("A1/E1");

        let mut invalid_publication = publication.clone();
        invalid_publication.bytes.push(b'!');
        assert!(receipt.verify(&invalid_publication).is_err(), "A2/!C1");
        let mut invalid_publication = publication.clone();
        invalid_publication.effect_id = "noncanonical-effect".into();
        assert!(receipt.verify(&invalid_publication).is_err(), "A2/!C2");
        let mut invalid_publication = publication.clone();
        invalid_publication.idempotency_scope = Some("  ".into());
        assert!(receipt.verify(&invalid_publication).is_err(), "A2/!C3");

        let mut mismatches = Vec::new();
        let mut candidate = receipt.clone();
        candidate.effect_id = "other-effect".into();
        mismatches.push(("effect", candidate));
        let mut candidate = receipt.clone();
        candidate.content_id = "sha256:other".into();
        mismatches.push(("content", candidate));
        let mut candidate = receipt.clone();
        candidate.record.blob_id = "sha256:other".into();
        mismatches.push(("blob", candidate));
        let mut candidate = receipt.clone();
        candidate.record.workspace_id = "workspace-2".into();
        mismatches.push(("Workspace", candidate));
        let mut candidate = receipt.clone();
        candidate.record.scope_id = Some("session-2".into());
        mismatches.push(("Session", candidate));
        let mut candidate = receipt.clone();
        candidate.record.logical_path = Some("outputs/other.patch".into());
        mismatches.push(("path", candidate));
        let mut candidate = receipt.clone();
        candidate.record.harvest_key = Some("other-harvest".into());
        mismatches.push(("harvest key", candidate));
        for (axis, candidate) in mismatches {
            assert!(candidate.verify(&publication).is_err(), "A2/!C4 {axis}");
        }
    }

    #[test]
    fn file_application_projection_preserves_every_resource_fact_and_drops_only_the_fence() {
        // Cause/effect decision table: C1 the execution publication carries a
        // fence; C2 every Resources-owned fact (effect, Workspace, Session,
        // path, media type, content identity, bytes, and optional terminal
        // scope) is present. P1 C1+C2 projects one unfenced publication with
        // every C2 fact byte-for-byte unchanged; P2 an absent fence follows
        // the same projection. The File application therefore receives the
        // existing canonical value object, never a second parallel command.
        let (base, _) = artifact();
        let publication = ArtifactPublication {
            effect_id: base.effect_id.clone(),
            workspace_id: base.workspace_id.clone(),
            session_id: base.session_id.clone(),
            logical_path: base.logical_path.clone(),
            mime_type: base.mime_type.clone(),
            content_id: base.content_id.clone(),
            bytes: base.bytes.clone(),
            idempotency_scope: Some("cleanup-current".into()),
            fence: Some("claim"),
        };

        let expected = publication.clone();
        let projected = publication.into_file_application();
        assert_eq!(projected.effect_id, expected.effect_id, "P1 effect");
        assert_eq!(
            projected.workspace_id, expected.workspace_id,
            "P1 Workspace"
        );
        assert_eq!(projected.session_id, expected.session_id, "P1 Session");
        assert_eq!(projected.logical_path, expected.logical_path, "P1 path");
        assert_eq!(projected.mime_type, expected.mime_type, "P1 media type");
        assert_eq!(projected.content_id, expected.content_id, "P1 content");
        assert_eq!(projected.bytes, expected.bytes, "P1 bytes");
        assert_eq!(
            projected.idempotency_scope, expected.idempotency_scope,
            "P1 terminal scope"
        );
        assert_eq!(projected.fence, None, "P1 fence");

        let mut unfenced = expected;
        unfenced.fence = None;
        assert_eq!(unfenced.into_file_application().fence, None, "P2");
    }

    #[test]
    fn terminal_recovery_filters_exact_file_association_and_normalizes_tombstones() {
        // Cause/effect table: R1 terminal scope on the canonical v1 harvest key
        // preserves the ordinary effect identity; R2 exact Workspace/Session/key
        // + exact scope reconstructs one receipt; R3 ordinary/older scope/foreign
        // records are ignored; R4 a tombstone reconstructs the original
        // `deleted=false` publication receipt; R5 duplicate exact evidence fails.
        let (mut publication, _) = artifact();
        publication.idempotency_scope = Some("cleanup-current".into());
        let mut current = ArtifactPublicationReceipt::from_publication_record(
            &publication,
            FileRecord {
                id: "file-current".into(),
                workspace_id: publication.workspace_id.clone(),
                blob_id: publication.content_id.clone(),
                filename: publication.logical_path.clone(),
                mime_type: publication.mime_type.clone(),
                size_bytes: publication.bytes.len() as u64,
                created_at: "2026-08-29T00:00:00Z".into(),
                expires_at: None,
                downloadable: true,
                scope_id: Some(publication.session_id.clone()),
                logical_path: Some(publication.logical_path.clone()),
                harvest_key: Some(publication.effect_id.clone()),
                artifact_idempotency_scope: Some("cleanup-current".into()),
                deleted: false,
            },
        )
        .expect("R1/R2");
        assert_eq!(
            publication.effect_id,
            crate::harvest_idempotency_key(
                &publication.session_id,
                &publication.logical_path,
                &publication.content_id,
            ),
            "R1"
        );
        let recovery = ArtifactRecovery {
            workspace_id: publication.workspace_id.clone(),
            session_id: publication.session_id.clone(),
            idempotency_scope: "cleanup-current".into(),
            fence: (),
        };
        let mut tombstone = current.record.clone();
        tombstone.deleted = true;
        let mut ordinary = current.record.clone();
        ordinary.id = "file-ordinary".into();
        ordinary.artifact_idempotency_scope = None;
        let mut old_terminal = current.record.clone();
        old_terminal.id = "file-old-terminal".into();
        old_terminal.artifact_idempotency_scope = Some("cleanup-old".into());
        let mut foreign = current.record.clone();
        foreign.id = "file-foreign".into();
        foreign.workspace_id = "workspace-foreign".into();
        assert_eq!(
            recovery
                .receipts_from_records([ordinary, old_terminal, foreign, tombstone.clone()])
                .expect("R2-R4"),
            vec![current.clone()],
            "R2-R4"
        );
        current.record.deleted = false;
        assert!(
            recovery
                .receipts_from_records([tombstone.clone(), tombstone])
                .is_err(),
            "R5"
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

    #[test]
    fn repository_gateway_expiry_wire_is_additive_and_strongly_validated() {
        /* Gateway expiry wire decision table:
         * C1=legacy Gateway wire omits expiry; C2=new wire supplies a positive
         * issuer expiry; C3=expiry is zero. E1=decode legacy as None and omit it
         * again; E2=lossless typed round-trip; E3=reject invalid evidence.
         * Rules: W1 C1=>E1; W2 C2=>E2; W3 C3=>E3. Liveness and authority upper
         * bounds remain the Worker boundary's dynamic responsibility.
         */
        let legacy = r#"{"type":"gateway_mediated","remote_url":"https://gateway.invalid/git/repository","capability":"cap"}"#;
        let transport: RepositoryTransport = serde_json::from_str(legacy).expect("W1/E1");
        assert!(matches!(
            transport,
            RepositoryTransport::GatewayMediated {
                expires_at_unix_ms: None,
                ..
            }
        ));
        assert!(
            !serde_json::to_string(&transport)
                .unwrap()
                .contains("expires_at_unix_ms")
        );

        let expiry = RepositoryGatewayCapabilityExpiry::new(123).expect("W2/E2");
        let current = RepositoryTransport::GatewayMediated {
            remote_url: "https://gateway.invalid/git/repository".into(),
            capability: RepositoryGatewayCapability::new("cap").unwrap(),
            expires_at_unix_ms: Some(expiry),
        };
        let decoded: RepositoryTransport =
            serde_json::from_str(&serde_json::to_string(&current).unwrap()).expect("W2/E2");
        assert_eq!(decoded, current);
        assert!(RepositoryGatewayCapabilityExpiry::new(0).is_err(), "W3/E3");
        assert!(serde_json::from_str::<RepositoryGatewayCapabilityExpiry>("0").is_err());
    }
}
