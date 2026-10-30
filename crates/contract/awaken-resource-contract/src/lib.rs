// @formal entrypoint memory.stale_conditional_delete_never_removes_live_head
// @formal evidence memory.rename_replace_preserves_source_identity
// @formal evidence skill.pinned_version_survives_retirement
//! Contract-only vocabulary for the Resources context (files, memory, repositories, skills).
//!
//! The mountable-resource **SPIs** — [`FileStore`] + [`FileCatalog`] as the two
//! capabilities of one File aggregate, [`MemoryWriter`], and [`SkillStore`]
//! — plus the value/error types in their signatures,
//! and **nothing else**: no backend, no SQL driver, no filesystem. It mirrors
//! [`awaken-provisioning-contract`](https://docs.rs/awaken-provisioning-contract):
//! an adapter (or a consumer reusing these stores inside its own database) can
//! depend on the traits alone without pulling `sqlx`/`object_store` or
//! any concrete store.
//!
//! The backend crates (`awaken-file-store`, `awaken-memory-store`,
//! `awaken-skill-store`) implement these SPIs and **re-export** every item here,
//! so existing paths like `awaken_file_store::FileStore` keep resolving unchanged.
//!
//! Note there is deliberately **no `SecretSource` port**: a secret is resolved by
//! reference through `awaken_provisioning_contract::BlobSource` straight into the
//! sandbox, never surfaced as a value here.
//!
//! # Boundary
//!
//! Context: `resources`.
//!
//! Implemented by: `awaken-control`, `awaken-coordinator`, `awaken-file-store`,
//! `awaken-memory-store`, `awaken-resource-application`, `awaken-resource-store`,
//! `awaken-resource-worker-http`, `awaken-sandbox-container`, `awaken-sandbox-local`,
//! `awaken-sandbox-memoryd`, `awaken-session-application`, `awaken-session-configuration-contract`,
//! `awaken-session-executor`, `awaken-skill-store`.
//!
//! Called by: `awaken-agent-config`, `awaken-cli`, `awaken-config-resolver`, `awaken-control`,
//! `awaken-coordinator`, `awaken-deployment-contract`, `awaken-eval`,
//! `awaken-executable-agent-catalog`, `awaken-executable-agent-contract`, `awaken-file-store`,
//! `awaken-memory-store`, `awaken-process-startup`, `awaken-protocol-managed`,
//! `awaken-protocol-managed-testkit`, `awaken-provisioning-contract`,
//! `awaken-resource-application`, `awaken-resource-persistence`, `awaken-resource-reclaimer`,
//! `awaken-resource-store`, `awaken-resource-worker-http`, `awaken-sandbox-container`,
//! `awaken-sandbox-local`, `awaken-sandbox-memoryd`, `awaken-scenario-host`,
//! `awaken-session-application`, `awaken-session-configuration-contract`,
//! `awaken-session-contract`, `awaken-session-environment`, `awaken-session-execution-contract`,
//! `awaken-session-executor`, `awaken-session-executor-testkit`, `awaken-session-realization`,
//! `awaken-session-run-delivery`, `awaken-skill-store`, `awaken-store-conformance`,
//! `awaken-worker`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::sync::Arc;

mod artifact_capture;
mod execution;
mod generation;
pub use generation::{CompleteGeneration, generation_matches, sealed};
mod input;
mod lifecycle;
mod memory_application;
mod memory_copy;
pub use memory_copy::{MemoryCopyBase, MemoryCopyConflict};
mod memory_invalidation;
pub use memory_invalidation::{Invalidation, LocalInvalidator};
mod registry;
mod repository_application;
pub use repository_application::{
    CreateRepositoryCommand, RepositoryApplicationService, RepositoryCredentialValidator,
    RepositoryDetails, SESSION_REPOSITORY_OWNER_KIND, SESSION_REPOSITORY_OWNER_SESSION,
    UnavailableRepositoryCredentialValidator, is_session_owned_repository,
};

pub use artifact_capture::{
    ArtifactCapture, ArtifactCaptureRunReference, ReplaceArtifactCapture,
    ReplaceArtifactCaptureOutcome,
};
pub use execution::{
    ArtifactPublication, ArtifactPublicationError, ArtifactPublicationReceipt, ArtifactPublisher,
    ArtifactRecovery, FileContentSource, FileContentSourceError, FileReadPurpose,
    MemoryMaterializationReferenceEncoder, MemoryMaterializationReferenceError,
    RepositoryBindingVerifier, RepositoryBindingVerifierError, RepositoryGatewayCapability,
    RepositoryGatewayCapabilityExpiry, RepositoryTransport, ResolvedFileContent,
    UnavailableArtifactPublisher, UnavailableFileContentSource,
};
pub use input::{
    BindingId, FileId, InputBinding, InputResourceId, MemoryStoreId, RepositoryId, ResourceAccess,
    SkillId, SkillVersionId,
};
pub use lifecycle::{
    AcquireResourceReclamationOutcome, PutResourcePurgeOutcome, ResourceKind,
    ResourcePhysicalReclaimer, ResourcePurgeError, ResourcePurgeEvidence, ResourcePurgeGuard,
    ResourcePurgeIntent, ResourcePurgePreparation, ResourcePurgeReceipt, ResourcePurgeRepository,
    ResourcePurgeScheduler, ResourcePurgeStatus, ResourceReclamationFence,
    ResourceReclamationRepository, ResourceReference, ResourceReferenceIndex,
    ResourceReferenceKind, ResourceReferenceRecord, ResourceTarget,
};
pub use memory_application::{
    CreateMemoryStoreCommand, MemoryStoreApplicationError, MemoryStoreApplicationService,
    UpdateMemoryStoreCommand,
};
pub use registry::{
    AggregateRevision, ChangeMemoryStoreState, ChangeRepositoryState, ClonePolicy, ConfigVersion,
    ExecutionResourceResolver, InsertOutcome, LiveResourceBindingVerifier, MemoryStoreAggregate,
    MemoryStoreConfigVersion, MemoryStoreDefinition, PublishMemoryStoreConfig,
    PublishRepositoryConfig, RegisterMemoryStore, RegisterRepository, RegistryRepositoryError,
    ReplaceOutcome, RepositoryAggregate, RepositoryConfigVersion, RepositoryDefinition,
    RepositoryPreparation, RepositoryRegistration, RepositoryRegistrationMode,
    ResourceAdministration, ResourceInventory, ResourceRegistry, ResourceRegistryError,
    ResourceRegistryRepository, ResourceState, ResourceTimestamps, RetentionPolicy, Stored,
    UpdateMemoryStoreProfile,
};

// ---------------------------------------------------------------------------
// File store (ADR-0041): content-addressed, immutable blob port.
// ---------------------------------------------------------------------------

/// The canonical content-addressed identity for immutable Resource bytes.
///
/// File stores, Worker transport verification, and Sandbox mount verification
/// must all call this function so a byte sequence has one identity on every
/// node and through every adapter.
#[must_use]
pub fn content_id(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Stable SHA-256 used by the Memory CAS contract. Keeping the digest beside
/// the value/port types prevents execution and storage adapters from carrying
/// competing implementations.
#[must_use]
pub fn memory_sha256_hex(content: &str) -> String {
    use sha2::{Digest as _, Sha256};

    format!("{:x}", Sha256::digest(content.as_bytes()))
}

/// Stable, database-portable identity for one Sandbox artifact harvest.
/// Length framing prevents tuple ambiguity; the digest keeps internal tuple
/// components and PostgreSQL-forbidden separators out of persistence.
#[must_use]
pub fn harvest_idempotency_key(thread: &str, logical_path: &str, content_id: &str) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(b"awaken-file-harvest-v1\0");
    for component in [thread, logical_path, content_id] {
        hash.update(&(component.len() as u64).to_be_bytes());
        hash.update(component.as_bytes());
    }
    hash.finalize().to_hex().to_string()
}

/// Maximum size of one logical File accepted by every driving adapter.
pub const MAX_MANAGED_FILE_SIZE_BYTES: u64 = 500 * 1024 * 1024;

/// Maximum active logical File bytes owned by one Workspace.
pub const MAX_WORKSPACE_FILE_BYTES: u64 = 500 * 1024 * 1024 * 1024;

#[cfg(test)]
mod content_id_tests {
    use super::{content_id, harvest_idempotency_key};

    #[test]
    fn canonical_content_id_is_stable_and_content_sensitive() {
        // Cause/effect decision table:
        // | Rule | same bytes | different bytes | Effect |
        // | H1 | yes | no | identical cross-adapter identity |
        // | H2 | no | yes | distinct identity |
        // | H3 | empty input | no | fixed BLAKE3 compatibility vector |
        let empty = content_id(b"");
        assert_eq!(
            empty, "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
            "H3"
        );
        assert_eq!(content_id(b"same"), content_id(b"same"), "H1");
        assert_ne!(content_id(b"same"), content_id(b"different"), "H2");
    }

    #[test]
    fn harvest_key_is_framed_portable_and_sensitive_to_every_component() {
        // Harvest-identity FMECA and cause/effect table: C1 the same ordered
        // tuple is retried; C2 one tuple component changes; C3 components contain
        // delimiter-like bytes. Effects: E1 stable idempotency, E2 distinct
        // logical versions, E3 printable database-safe identity. Rules: K1
        // C1=>E1; K2 C2=>E2; K3 C3=>E3. Length framing, not a delimiter, owns
        // tuple identity.
        let key = harvest_idempotency_key("thread", "a\0b", "content");
        assert_eq!(
            key,
            harvest_idempotency_key("thread", "a\0b", "content"),
            "K1"
        );
        assert_ne!(
            key,
            harvest_idempotency_key("thread-2", "a\0b", "content"),
            "K2"
        );
        assert_ne!(
            key,
            harvest_idempotency_key("thread", "a\0b-2", "content"),
            "K2"
        );
        assert_ne!(
            key,
            harvest_idempotency_key("thread", "a\0b", "content-2"),
            "K2"
        );
        assert!(!key.contains('\0'), "K3");
        assert!(key.bytes().all(|byte| byte.is_ascii_hexdigit()), "K3");
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FileStoreError {
    #[error("file store error: invalid configuration: {detail}")]
    InvalidConfiguration { detail: String },
    #[error("file store error: corrupt content-addressed blob `{content_id}`")]
    CorruptContent { content_id: String },
    #[error("file store error: authority unavailable: {detail}")]
    AuthorityUnavailable { detail: String },
    #[error("file store error: {detail}")]
    Io {
        kind: std::io::ErrorKind,
        detail: String,
    },
    /// The adapter could not determine the operation's result. This does not
    /// prove absence, rejection or retry safety; reconciliation must inspect
    /// the same content identity before repeating a mutation.
    #[error("file store error: {detail}")]
    Indeterminate { detail: String },
}

impl From<std::io::Error> for FileStoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            detail: error.to_string(),
        }
    }
}

/// A logical Files-API record. `id` is the public opaque `file_...` identity;
/// `blob_id` is the private content-addressed identity in [`FileStore`]. Keeping
/// these identities separate lets equal bytes deduplicate physically without
/// merging filenames, scopes, download policy, or deletion lifecycles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRecord {
    pub id: String,
    pub workspace_id: String,
    pub blob_id: String,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub created_at: String,
    /// RFC 3339 download-expiry instant. Metadata remains visible after expiry;
    /// immutable bytes are no longer served.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    pub downloadable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_path: Option<String>,
    /// Stable internal idempotency key for one logical File creation command.
    /// Public uploads may supply the standard `Idempotency-Key` header while
    /// Runtime harvest derives a content-addressed key. The value is never
    /// projected by a public Files response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harvest_key: Option<String>,
    /// Optional terminal-cleanup operation which durably incorporated this
    /// existing artifact publication. It is an association on the same logical
    /// File, never a second harvest identity or receipt registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_idempotency_scope: Option<String>,
    #[serde(default)]
    pub deleted: bool,
}

/// Whether an idempotent FileCatalog create inserted the candidate or recovered
/// the already-committed record for the same harvest key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateFileRecordOutcome {
    Inserted(FileRecord),
    Existing(FileRecord),
}

impl CreateFileRecordOutcome {
    #[must_use]
    pub fn record(&self) -> &FileRecord {
        match self {
            Self::Inserted(record) | Self::Existing(record) => record,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FileCatalogError {
    #[error("invalid file record: {detail}")]
    InvalidRecord { detail: String },
    #[error("invalid artifact capture: {detail}")]
    InvalidCapture { detail: String },
    #[error("file identity conflict: {detail}")]
    IdentityConflict { detail: String },
    #[error("artifact association conflict: {detail}")]
    AssociationConflict { detail: String },
    #[error("file catalog contains corrupt data: {detail}")]
    CorruptData { detail: String },
    #[error("file catalog authority unavailable: {detail}")]
    AuthorityUnavailable { detail: String },
    /// No evidence that a write failed to commit. Reconcile using its original
    /// identity before retrying; never lower an unread catalog to absence.
    #[error("file catalog outcome is indeterminate: {detail}")]
    Indeterminate { detail: String },
}

/// Durable logical Files-API catalog. It owns metadata, Workspace visibility,
/// Session scope, and harvest idempotency; bytes remain exclusively in
/// [`FileStore`]. Lists are newest-first with `id` as the deterministic tie-break.
#[async_trait]
pub trait FileCatalog: Send + Sync {
    /// Latest complete output observation. None means no capture was committed;
    /// Some with no ids is a positively observed empty output directory.
    async fn artifact_capture(
        &self,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<Option<ArtifactCapture>, FileCatalogError>;
    async fn replace_artifact_capture(
        &self,
        request: ReplaceArtifactCapture,
    ) -> Result<ReplaceArtifactCaptureOutcome, FileCatalogError>;
    async fn create_file(
        &self,
        record: FileRecord,
    ) -> Result<CreateFileRecordOutcome, FileCatalogError>;
    async fn get_file(
        &self,
        workspace_id: &str,
        file_id: &str,
        include_deleted: bool,
    ) -> Result<Option<FileRecord>, FileCatalogError>;
    async fn list_files(
        &self,
        workspace_id: &str,
        scope_id: Option<&str>,
    ) -> Result<Vec<FileRecord>, FileCatalogError>;
    /// Recovery-only projection which retains tombstoned logical evidence.
    /// Public Files lists remain active-only; implementations that cannot
    /// prove deleted rows must fail closed instead of silently dropping them.
    async fn list_files_including_deleted(
        &self,
        workspace_id: &str,
        scope_id: Option<&str>,
    ) -> Result<Vec<FileRecord>, FileCatalogError>;
    /// Mark one logical File deleted and return its retained cleanup metadata.
    async fn mark_file_deleted(
        &self,
        workspace_id: &str,
        file_id: &str,
    ) -> Result<Option<FileRecord>, FileCatalogError>;
    /// Logical active-byte accounting used by the Workspace Files quota.
    async fn active_size_bytes(&self, workspace_id: &str) -> Result<u64, FileCatalogError>;
}

/// A content-addressed, immutable blob store. `put` returns the content id and is
/// idempotent (equal bytes → same id → no-op if present, so retries are safe). The
/// id is computed in the store core (BLAKE3), never in a backend, so it is identical
/// across every implementation.
#[async_trait]
pub trait FileStore: Send + Sync {
    /// Store `bytes`, returning the content id.
    async fn put(&self, bytes: &[u8]) -> Result<String, FileStoreError>;
    /// Fetch by id, `None` if absent.
    async fn get(&self, id: &str) -> Result<Option<Vec<u8>>, FileStoreError>;
    /// List all ids, **sorted ascending**. Every backend returns the same stable
    /// order, so two stores' listings are directly comparable (mirror/migration diff).
    async fn list(&self) -> Result<Vec<String>, FileStoreError>;
    /// Delete by id; returns whether it existed. GC/admin only — not a mutation.
    async fn delete(&self, id: &str) -> Result<bool, FileStoreError>;
}

/// Resources application port shared by public HTTP adapters and Runtime
/// artifact workflows. Implementations own command ordering across FileStore,
/// FileCatalog, and lifecycle persistence; consumers cannot reproduce it.
#[async_trait]
pub trait FileApplicationService: Send + Sync {
    async fn artifact_capture(
        &self,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<Option<ArtifactCapture>, ResourcePurgeError>;
    async fn replace_artifact_capture(
        &self,
        request: ReplaceArtifactCapture,
    ) -> Result<ReplaceArtifactCaptureOutcome, ResourcePurgeError>;
    async fn get(
        &self,
        workspace_id: &str,
        file_id: &str,
    ) -> Result<Option<FileRecord>, ResourcePurgeError>;

    async fn list(
        &self,
        workspace_id: &str,
        scope_id: Option<&str>,
    ) -> Result<Vec<FileRecord>, ResourcePurgeError>;

    /// Internal response-loss readback. Unlike the public list, this includes
    /// tombstoned records so a later logical delete cannot erase proof of a
    /// previously committed publication.
    async fn list_including_deleted(
        &self,
        workspace_id: &str,
        scope_id: Option<&str>,
    ) -> Result<Vec<FileRecord>, ResourcePurgeError>;

    async fn create_uploaded_file_with_expiry_and_idempotency(
        &self,
        workspace_id: &str,
        filename: String,
        mime_type: String,
        bytes: &[u8],
        expires_at: Option<String>,
        idempotency_key: Option<String>,
    ) -> Result<FileRecord, ResourcePurgeError>;

    async fn create_uploaded_file_with_expiry(
        &self,
        workspace_id: &str,
        filename: String,
        mime_type: String,
        bytes: &[u8],
        expires_at: Option<String>,
    ) -> Result<FileRecord, ResourcePurgeError> {
        self.create_uploaded_file_with_expiry_and_idempotency(
            workspace_id,
            filename,
            mime_type,
            bytes,
            expires_at,
            None,
        )
        .await
    }

    async fn create_uploaded_file(
        &self,
        workspace_id: &str,
        filename: String,
        mime_type: String,
        bytes: &[u8],
    ) -> Result<FileRecord, ResourcePurgeError> {
        self.create_uploaded_file_with_expiry(workspace_id, filename, mime_type, bytes, None)
            .await
    }

    /// Create or atomically associate a terminal operation with the same
    /// canonical artifact File. The existing [`ArtifactPublication`] is the
    /// sole command value for ordinary and terminal artifacts; execution
    /// adapters remove their opaque fence through
    /// [`ArtifactPublication::into_file_application`] before invoking this
    /// Resources-owned port.
    async fn create_artifact(
        &self,
        publication: &ArtifactPublication<()>,
    ) -> Result<FileRecord, ResourcePurgeError>;

    async fn bytes(
        &self,
        workspace_id: &str,
        file_id: &str,
    ) -> Result<Option<(FileRecord, Vec<u8>)>, ResourcePurgeError>;

    async fn delete(
        &self,
        workspace_id: &str,
        file_id: &str,
        requested_at_unix_ms: u64,
    ) -> Result<Option<FileRecord>, ResourcePurgeError>;
}

// ---------------------------------------------------------------------------
// Skill repository: durable, workspace-scoped Skill aggregate port.
// ---------------------------------------------------------------------------

/// A skill-store failure.
#[derive(Debug, thiserror::Error)]
pub enum SkillStoreError {
    #[error("skill `{0}` already exists")]
    AlreadyExists(String),
    #[error("skill `{0}` was not found in this Workspace")]
    NotFound(String),
    #[error("skill version `{0}` already exists")]
    VersionConflict(String),
    #[error("invalid Skill definition: {detail}")]
    InvalidDefinition { detail: String },
    #[error("invalid Skill version: {detail}")]
    InvalidVersion { detail: String },
    #[error("Skill bundle does not match its frozen digest: {detail}")]
    BundleMismatch { detail: String },
    #[error("Skill lifecycle transition was rejected: {detail}")]
    LifecycleConflict { detail: String },
    #[error("invalid Skill selection: {detail}")]
    InvalidSelection { detail: String },
    #[error("io: {detail}")]
    Io {
        kind: std::io::ErrorKind,
        detail: String,
    },
    #[error("corrupt Skill data: {detail}")]
    CorruptData { detail: String },
    #[error("Skill outcome is indeterminate: {detail}")]
    Indeterminate { detail: String },
    #[error("Skill authority unavailable: {detail}")]
    AuthorityUnavailable { detail: String },
    #[error("Skill encoding failed before publication: {detail}")]
    EncodingFailure { detail: String },
    #[error(transparent)]
    Reference(#[from] ResourcePurgeError),
}

impl From<std::io::Error> for SkillStoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io {
            kind: error.kind(),
            detail: error.to_string(),
        }
    }
}

/// One binary-safe file in an immutable Skill version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillBundleFile {
    /// Normalized relative path inside the bundle. Validation belongs to the
    /// resource application service; materializers must validate again before IO.
    pub path: String,
    #[serde(with = "skill_bytes")]
    pub content: Vec<u8>,
    /// Whether the runtime may execute this regular file directly. The bundle
    /// format intentionally models only this safe bit rather than arbitrary
    /// owner/group/mode metadata from an untrusted archive.
    #[serde(default)]
    pub executable: bool,
}

/// Reduce a Skill name/id to the single canonical filesystem-safe stem used by
/// persistence and execution materializers.
#[must_use]
pub fn skill_stem(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for character in name.chars() {
        if character.is_ascii_alphanumeric() || character == '_' {
            out.push(character);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.truncate(120);
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "skill".to_string()
    } else {
        trimmed
    }
}

/// Stable tagged catalog identity derived from the canonical Skill stem.
///
/// Keeping identity derivation in the contract makes HTTP ingestion, durable
/// stores, and runtime advertisement use one algorithm without depending on an
/// infrastructure crate.
#[must_use]
pub fn skill_catalog_id(name: &str) -> String {
    let stem = skill_stem(name);
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in stem.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("skill_{hash:016x}")
}

/// One immutable version of a Skill bundle. The version freezes authored Skill
/// content; it contains no principal, role, policy, API key, or runtime host path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillVersion {
    pub id: SkillVersionId,
    pub skill_id: SkillId,
    pub version: u64,
    pub name: String,
    pub description: String,
    pub directory: String,
    pub bundle_sha256: String,
    pub files: Vec<SkillBundleFile>,
    #[serde(default)]
    pub created_unix_nanos: u64,
}

/// Canonical SHA-256 identity of a complete immutable Skill bundle.
///
/// The Resources contract owns this algorithm because stores, HTTP transports,
/// execution materializers, and Managed projections must reject exactly the
/// same mutations. Paths are sorted and every variable-length field is framed.
#[must_use]
pub fn skill_bundle_sha256(files: &[SkillBundleFile]) -> String {
    let mut ordered = files.iter().collect::<Vec<_>>();
    ordered.sort_by(|a, b| a.path.cmp(&b.path));
    let mut hash = Sha256::new();
    for file in ordered {
        hash.update((file.path.len() as u64).to_be_bytes());
        hash.update(file.path.as_bytes());
        hash.update((file.content.len() as u64).to_be_bytes());
        hash.update(&file.content);
        if file.executable {
            hash.update(b"\0awaken-skill-executable\0");
        }
    }
    format!("sha256:{:x}", hash.finalize())
}

impl SkillVersion {
    /// The exact `SKILL.md` bytes, when present at the bundle root or below a
    /// single uploaded directory.
    #[must_use]
    pub fn skill_md(&self) -> Option<&[u8]> {
        self.files
            .iter()
            .find(|file| file.path == "SKILL.md" || file.path.ends_with("/SKILL.md"))
            .map(|file| file.content.as_slice())
    }
}

/// Stable Skill resource identity and its current immutable-version pointer.
/// Workspace ownership is an intrinsic resource invariant, not an authorization
/// policy decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillDefinition {
    pub id: SkillId,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_title: Option<String>,
    pub latest_version: u64,
    /// Highest version number ever assigned. It never decreases or reuses a
    /// retired version number.
    pub last_version: u64,
    #[serde(default)]
    pub timestamps: ResourceTimestamps,
}

impl SkillDefinition {
    /// The next immutable ordinal follows the highest version ever assigned,
    /// even when authoring visibility has retired a later version. Exhaustion
    /// is a conflict; it never wraps or reuses that version's immutable bytes.
    pub fn checked_next_version(&self) -> Result<u64, SkillStoreError> {
        self.last_version
            .checked_add(1)
            .ok_or_else(|| SkillStoreError::VersionConflict(self.last_version.to_string()))
    }

    /// Validate the complete initial frame before any reference-index or
    /// repository write. Adapters and the application reuse this one law;
    /// direct backend callers cannot bypass it by omitting the decorator.
    pub fn validate_initial_version(&self, version: &SkillVersion) -> Result<(), SkillStoreError> {
        if self.id.as_str().trim().is_empty() || self.workspace_id.as_str().trim().is_empty() {
            return Err(SkillStoreError::InvalidDefinition {
                detail: "skill and Workspace ids must be non-empty".into(),
            });
        }
        if self.latest_version != version.version
            || self.last_version != version.version
            || version.skill_id != self.id
            || version.version == 0
            || version.files.is_empty()
            || version.skill_md().is_none()
        {
            return Err(SkillStoreError::InvalidVersion {
                detail: "definition and initial Skill version are inconsistent".into(),
            });
        }
        if version.bundle_sha256 != skill_bundle_sha256(&version.files) {
            return Err(SkillStoreError::BundleMismatch {
                detail: "Skill bundle SHA-256 does not match its files".into(),
            });
        }
        Ok(())
    }
}

/// A durable, workspace-scoped repository for the complete Skill aggregate.
/// Authorization happens before this port is invoked; every operation is scoped by
/// the trusted Workspace and can only observe resources owned by that Workspace.
#[async_trait]
pub trait SkillStore: Send + Sync {
    /// Workspace partitions with persisted Skill aggregates. Used by startup
    /// lifecycle projection; callers still fetch definitions through the
    /// Workspace-scoped operation below.
    async fn workspace_ids(&self) -> Result<Vec<String>, SkillStoreError>;
    /// Atomically create a Skill and its first version.
    async fn create(
        &self,
        definition: SkillDefinition,
        initial_version: SkillVersion,
    ) -> Result<(), SkillStoreError>;
    /// Append one immutable version and atomically advance `latest_version`.
    async fn append_version(
        &self,
        workspace_id: &str,
        skill_id: &str,
        version: SkillVersion,
    ) -> Result<(), SkillStoreError>;
    async fn definition(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<Option<SkillDefinition>, SkillStoreError>;
    /// Definitions owned by one Workspace, sorted by stable Skill id.
    async fn list_definitions(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<SkillDefinition>, SkillStoreError>;
    /// Return one atomic, Skill-id-sorted snapshot of every visible aggregate's
    /// current immutable version. Callers must not reconstruct this view with
    /// `list_definitions` followed by per-Skill reads.
    async fn snapshot_latest_versions(
        &self,
        workspace_id: &str,
    ) -> Result<Vec<SkillVersion>, SkillStoreError>;
    async fn version(
        &self,
        workspace_id: &str,
        skill_id: &str,
        version: u64,
    ) -> Result<Option<SkillVersion>, SkillStoreError>;
    async fn list_versions(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<Vec<SkillVersion>, SkillStoreError>;
    /// Retire a visible version. Ordinary listings/retrieval hide it and the latest
    /// pointer moves, while immutable bytes remain addressable by an existing
    /// Session pin. The only visible version cannot be retired.
    async fn delete_version(
        &self,
        workspace_id: &str,
        skill_id: &str,
        version: u64,
    ) -> Result<bool, SkillStoreError>;
    /// Tombstone the Skill aggregate. Ordinary authoring/resolution hides it but
    /// pinned immutable versions remain readable until safe physical reclamation.
    async fn delete_skill(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<bool, SkillStoreError>;
    /// Physically remove a tombstoned Skill. Returns the number of immutable
    /// versions reclaimed; repeated calls return zero.
    async fn purge_skill(&self, workspace_id: &str, skill_id: &str)
    -> Result<u64, SkillStoreError>;
}

/// JSON keeps binary files lossless without making the port depend on a wire
/// encoding. This private adapter serializes bytes as integer arrays and rejects
/// values outside the byte range on decode.
mod skill_bytes {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub(crate) fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        bytes.serialize(serializer)
    }

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Vec::<u8>::deserialize(deserializer)
    }
}

// ---------------------------------------------------------------------------
// Memory repository (ADR-0053): path-addressed CAS aggregate port + value types.
// ---------------------------------------------------------------------------

/// Hard cap on a single memory's content (Anthropic parity, ADR-0057). Enforced on
/// every create/update before any allocation.
pub const MAX_MEMORY_BYTES: usize = 102_400;
/// Hard cap on live memory heads in one store. Historical versions do not count;
/// editing an existing head remains legal at capacity.
pub const MAX_MEMORIES_PER_STORE: usize = 2_000;
/// Hard cap on a memory path.
pub const MAX_PATH_BYTES: usize = 1024;

/// Reduce an opaque memory-store id to one bounded filesystem-safe stem.
/// Storage and Worker-side mount adapters share this implementation so path
/// safety cannot drift across the HTTP boundary.
#[must_use]
pub fn memory_store_stem(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for character in name.chars() {
        if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
            out.push(character);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.truncate(120);
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "memstore".to_string()
    } else {
        trimmed
    }
}

/// One path-addressed memory. `content` is present on `get`/`create`/`update`,
/// absent on listings. `created`/`updated` are Unix-epoch nanoseconds so a FUSE
/// `getattr` can report faithful timestamps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    pub id: String,
    pub path: String,
    pub content_sha256: String,
    pub content_size: u64,
    /// Monotonic per-path version: 1 on create, +1 on each write. The cache-validation
    /// oracle for the distributed model (ADR-0053 D5) and a diagnostic.
    pub version: u64,
    pub created_unix_nanos: u128,
    pub updated_unix_nanos: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// Complete optimistic-concurrency identity for one Memory head generation.
///
/// Content hashes alone are not generations: a path-only rename or an A -> B -> A
/// content cycle can retain the same hash while advancing the authoritative head.
/// Every conditional mutation therefore carries the immutable Memory identity,
/// its monotonic version, and the content digest observed together.
///
/// The coordinates are private: a witness is read from one head
/// ([`Memory::generation`], [`MemoryEntry::generation`]) or minted whole by
/// [`MemoryGeneration::new`], and is thereafter only carried, never edited one
/// coordinate at a time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryGeneration {
    memory_id: String,
    version: u64,
    content_sha256: String,
}

impl MemoryGeneration {
    /// The witness of Memory `memory_id` observed at `version` with content
    /// digest `content_sha256`, all read together from one head.
    #[must_use]
    pub fn new(
        memory_id: impl Into<String>,
        version: u64,
        content_sha256: impl Into<String>,
    ) -> Self {
        Self {
            memory_id: memory_id.into(),
            version,
            content_sha256: content_sha256.into(),
        }
    }

    #[must_use]
    pub fn memory_id(&self) -> &str {
        &self.memory_id
    }

    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    #[must_use]
    pub fn content_sha256(&self) -> &str {
        &self.content_sha256
    }
}

impl sealed::Sealed for MemoryGeneration {}

impl CompleteGeneration for MemoryGeneration {
    fn subject_id(&self) -> &str {
        &self.memory_id
    }

    fn version(&self) -> u64 {
        self.version
    }

    fn content_digest(&self) -> &str {
        &self.content_sha256
    }
}

impl Memory {
    #[must_use]
    pub fn generation(&self) -> MemoryGeneration {
        MemoryGeneration::new(self.id.clone(), self.version, self.content_sha256.clone())
    }
}

impl MemoryEntry {
    #[must_use]
    pub fn generation(&self) -> MemoryGeneration {
        MemoryGeneration::new(self.id.clone(), self.version, self.content_sha256.clone())
    }
}

/// A directory-listing entry (no content). Serializes with the same field names as a
/// content-less [`Memory`], so a `MemoryQueries::list` result crosses the managed/HTTP
/// surfaces directly rather than being re-projected through `Memory` first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub path: String,
    pub content_sha256: String,
    pub content_size: u64,
    pub version: u64,
    pub updated_unix_nanos: u128,
}

/// The kind of durable state transition recorded for a memory head.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryVersionOperation {
    Created,
    Modified,
    Deleted,
}

/// Neutral attribution captured on a Memory version mutation. Protocol adapters
/// lower authenticated identities into one of these variants; repositories do
/// not infer actors from paths or credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MemoryActor {
    ApiActor { api_key_id: String },
    SessionActor { session_id: String },
    UserActor { user_id: String },
    ServiceAccountActor { service_account_id: String },
}

/// One immutable audit/version row emitted by the same repository transaction
/// that mutates the live memory head. Redaction removes only the historical
/// content; it never rewrites the live head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryVersion {
    pub id: String,
    pub memory_id: String,
    pub operation: MemoryVersionOperation,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    pub created_unix_nanos: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<MemoryActor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redacted_unix_nanos: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redacted_by: Option<MemoryActor>,
}

/// Idempotent whole-store physical reclamation result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryPurgeSummary {
    pub heads_deleted: u64,
    pub versions_deleted: u64,
    pub copy_conflicts_deleted: u64,
}

/// A [`MemoryWriter`] failure.
#[derive(Debug, thiserror::Error)]
pub enum MemErr {
    #[error("memory not found: {0}")]
    NotFound(String),
    /// Optimistic-concurrency failure: the complete base generation no longer
    /// matches. `current` carries the server's live memory for diagnostics; the caller
    /// must not clobber.
    #[error("cas conflict on {}: base generation stale", .current.path)]
    Conflict { current: Box<Memory> },
    #[error("path already exists: {0}")]
    PathConflict(String),
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error("content exceeds {MAX_MEMORY_BYTES} bytes")]
    TooLarge,
    #[error("memory store contains the maximum {MAX_MEMORIES_PER_STORE} memories")]
    AtCapacity,
    #[error("storage: {0}")]
    Storage(String),
}

/// Contract-level path-length check — the port's single path-length predicate,
/// symmetric with how [`MAX_MEMORY_BYTES`] bounds content (via [`MemErr::TooLarge`]).
/// A path whose UTF-8 length exceeds [`MAX_PATH_BYTES`] is rejected with an
/// [`MemErr::InvalidPath`] that names the cap, so every backend enforces one
/// identical limit instead of each re-deriving its own.
pub fn validate_path_len(path: &str) -> Result<(), MemErr> {
    if path.len() > MAX_PATH_BYTES {
        Err(MemErr::InvalidPath(format!(
            "path exceeds {MAX_PATH_BYTES} bytes"
        )))
    } else {
        Ok(())
    }
}

/// Path-addressed, CAS memory capabilities — the seam a write-through
/// FUSE mount and the managed API share (ADR-0053/ADR-0063). All methods are
/// store-scoped by an opaque, globally-unique `store` id.
///
/// Every successful, state-changing create/update/rename/delete appends exactly
/// one or more [`MemoryVersion`] rows in the same atomic repository operation.
/// Idempotent updates/deletes append nothing. This invariant is what keeps API
/// versions, runtime recall/extraction, and mounted content on one source of truth.
/// Read path-addressed current heads without taking mutation authority.
///
/// ```compile_fail
/// use awaken_resource_contract::MemoryQueries;
/// async fn query_cannot_purge(queries: &dyn MemoryQueries) {
///     queries.purge_store("store").await;
/// }
/// ```
#[async_trait]
pub trait MemoryQueries: Send + Sync {
    async fn list(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr>;
    async fn get_by_path(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr>;
}

/// Read one atomic, path-ordered store snapshot including content. Never emulate with list plus reads.
///
/// ```compile_fail
/// use awaken_resource_contract::MemorySnapshotSource;
/// async fn snapshot_cannot_write(source: &dyn MemorySnapshotSource) {
///     source.create("store", "/path", "content").await;
/// }
/// ```
#[async_trait]
pub trait MemorySnapshotSource: Send + Sync {
    async fn snapshot_heads(&self, store: &str) -> Result<Vec<Memory>, MemErr>;
}

/// Author current heads with complete-generation CAS and authenticated attribution. History commits in the same transaction.
#[async_trait]
pub trait MemoryWriter: Send + Sync {
    async fn create_as(
        &self,
        store: &str,
        path: &str,
        content: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr>;
    async fn update_head_as(
        &self,
        store: &str,
        base: &MemoryGeneration,
        content: &str,
        target_path: Option<&str>,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr>;
    async fn delete_by_path_as(
        &self,
        store: &str,
        path: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<(), MemErr>;
    async fn delete_if_match_as(
        &self,
        store: &str,
        path: &str,
        base: &MemoryGeneration,
        actor: Option<&MemoryActor>,
    ) -> Result<bool, MemErr>;
    async fn create(&self, store: &str, path: &str, content: &str) -> Result<Memory, MemErr> {
        self.create_as(store, path, content, None).await
    }
    async fn update_head(
        &self,
        store: &str,
        base: &MemoryGeneration,
        content: &str,
        target_path: Option<&str>,
    ) -> Result<Memory, MemErr> {
        self.update_head_as(store, base, content, target_path, None)
            .await
    }
    async fn update(
        &self,
        store: &str,
        base: &MemoryGeneration,
        content: &str,
    ) -> Result<Memory, MemErr> {
        self.update_head(store, base, content, None).await
    }
    async fn delete_by_path(&self, store: &str, path: &str) -> Result<(), MemErr> {
        self.delete_by_path_as(store, path, None).await
    }
    async fn delete_if_match(
        &self,
        store: &str,
        path: &str,
        base: &MemoryGeneration,
    ) -> Result<bool, MemErr> {
        self.delete_if_match_as(store, path, base, None).await
    }
}

/// Live path operations used by mounted filesystems. Rename replaces atomically and open files follow stable ids.
#[async_trait]
pub trait MemoryFilesystem: Send + Sync {
    async fn list(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr>;
    async fn get_by_path(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr>;
    async fn create_as(
        &self,
        store: &str,
        path: &str,
        content: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr>;
    async fn update_head_as(
        &self,
        store: &str,
        base: &MemoryGeneration,
        content: &str,
        target_path: Option<&str>,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr>;
    /// Atomically move/replace, recording this caller on both moved and
    /// displaced history rows. None explicitly denotes an unattributed system
    /// operation; implementations never inherit the original creation author.
    async fn rename_as(
        &self,
        store: &str,
        from: &str,
        to: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr>;
    async fn delete_by_path_as(
        &self,
        store: &str,
        path: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<(), MemErr>;
}

/// Reconcile a materialized copy using atomic original-path admission and complete-generation CAS.
#[async_trait]
pub trait MemoryCopyWriteback: Send + Sync {
    async fn create_as(
        &self,
        store: &str,
        path: &str,
        content: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Memory, MemErr>;
    async fn update_materialized(
        &self,
        store: &str,
        path: &str,
        base: &MemoryGeneration,
        content: &str,
    ) -> Result<Memory, MemErr>;
    async fn delete_if_match_as(
        &self,
        store: &str,
        path: &str,
        base: &MemoryGeneration,
        actor: Option<&MemoryActor>,
    ) -> Result<bool, MemErr>;
}

/// Read preserved losing copy proposals without granting preservation authority.
///
/// The query port compiles independently of preservation:
/// ```
/// use awaken_resource_contract::{MemErr, MemoryCopyConflict, MemoryCopyConflictQueries};
/// async fn read(port: &dyn MemoryCopyConflictQueries) -> Result<Vec<MemoryCopyConflict>, MemErr> {
///     port.list_copy_conflicts("store").await
/// }
/// ```
/// A reader cannot acknowledge a copy proposal:
/// ```compile_fail
/// use awaken_resource_contract::{MemoryCopyConflict, MemoryCopyConflictQueries};
/// async fn cannot_preserve(port: &dyn MemoryCopyConflictQueries, candidate: &MemoryCopyConflict) {
///     port.preserve_copy_conflict("store", candidate).await;
/// }
/// ```
#[async_trait]
pub trait MemoryCopyConflictQueries: Send + Sync {
    async fn list_copy_conflicts(&self, store: &str) -> Result<Vec<MemoryCopyConflict>, MemErr>;
}

/// Preserve a losing copy proposal before its source may be disposed.
///
/// This operation does not manage a retention policy or TTL. `Ok(())` confirms
/// that the configured Memory authority has acknowledged the proposal under
/// its storage durability contract. An error leaves preservation unconfirmed:
/// the caller must keep the original source and must not dispose it. Preserving
/// a losing proposal never changes the successful head or its version history.
///
/// The preservation port compiles independently of listing:
/// ```
/// use awaken_resource_contract::{MemErr, MemoryCopyConflict, MemoryCopyConflictPreservation};
/// async fn preserve(port: &dyn MemoryCopyConflictPreservation, candidate: &MemoryCopyConflict)
///     -> Result<(), MemErr>
/// {
///     port.preserve_copy_conflict("store", candidate).await
/// }
/// ```
/// A preservation consumer receives no conflict inventory:
/// ```compile_fail
/// use awaken_resource_contract::MemoryCopyConflictPreservation;
/// async fn cannot_list(port: &dyn MemoryCopyConflictPreservation) {
///     port.list_copy_conflicts("store").await;
/// }
/// ```
#[async_trait]
pub trait MemoryCopyConflictPreservation: Send + Sync {
    async fn preserve_copy_conflict(
        &self,
        store: &str,
        candidate: &MemoryCopyConflict,
    ) -> Result<(), MemErr>;
}

/// Read immutable successful version history.
#[async_trait]
pub trait MemoryVersionQueries: Send + Sync {
    async fn list_versions(&self, store: &str) -> Result<Vec<MemoryVersion>, MemErr>;
}

/// Redact version content with attribution without changing current heads.
#[async_trait]
pub trait MemoryVersionRedaction: Send + Sync {
    async fn redact_version_as(
        &self,
        store: &str,
        version_id: &str,
        actor: Option<&MemoryActor>,
    ) -> Result<Option<MemoryVersion>, MemErr>;
    async fn redact_version(
        &self,
        store: &str,
        version_id: &str,
    ) -> Result<Option<MemoryVersion>, MemErr> {
        self.redact_version_as(store, version_id, None).await
    }
}

/// Physically purge content only after the resource lifecycle owner authorizes reclamation.
#[async_trait]
pub trait MemoryContentPurge: Send + Sync {
    async fn purge_store(&self, store: &str) -> Result<MemoryPurgeSummary, MemErr>;
}

/// Announce committed head changes to mounted caches. A composition root injects
/// the same sink into every role of the one backend decorator. It owns no data.
pub trait MemoryInvalidator: Send + Sync {
    /// `"/"` invalidates all cached paths in this store. Failed mutations publish nothing.
    fn publish(&self, store: &str, path: &str);
}

/// Required identity of the invalidator already selected by the write decorator.
/// This does not discover optional capabilities or create an invalidation owner.
pub trait MemoryInvalidationIdentity: Send + Sync {
    fn invalidator_identity(&self) -> Arc<dyn MemoryInvalidator>;
}

/// The four execution content views derived from one physical backend/client.
/// Fields are private so callers cannot pair A's reads with B's writes.
///
/// ```compile_fail
/// use std::sync::Arc;
/// use awaken_resource_contract::{MemoryExecutionContent, MemoryQueries,
///     MemorySnapshotSource, MemoryWriter, MemoryCopyWriteback};
/// fn cannot_mix(queries: Arc<dyn MemoryQueries>, snapshots: Arc<dyn MemorySnapshotSource>,
///     writer: Arc<dyn MemoryWriter>, copy_writeback: Arc<dyn MemoryCopyWriteback>) {
///     let _ = MemoryExecutionContent { queries, snapshots, writer, copy_writeback };
/// }
/// ```
#[derive(Clone)]
pub struct MemoryExecutionContent {
    queries: Arc<dyn MemoryQueries>,
    snapshots: Arc<dyn MemorySnapshotSource>,
    writer: Arc<dyn MemoryWriter>,
    copy_writeback: Arc<dyn MemoryCopyWriteback>,
}
impl MemoryExecutionContent {
    #[must_use]
    pub fn new<T>(backend: Arc<T>) -> Self
    where
        T: MemoryQueries + MemorySnapshotSource + MemoryWriter + MemoryCopyWriteback + 'static,
    {
        Self {
            queries: backend.clone(),
            snapshots: backend.clone(),
            writer: backend.clone(),
            copy_writeback: backend,
        }
    }
    #[must_use]
    pub fn queries(&self) -> Arc<dyn MemoryQueries> {
        self.queries.clone()
    }
    #[must_use]
    pub fn snapshots(&self) -> Arc<dyn MemorySnapshotSource> {
        self.snapshots.clone()
    }
    #[must_use]
    pub fn writer(&self) -> Arc<dyn MemoryWriter> {
        self.writer.clone()
    }
    #[must_use]
    pub fn copy_writeback(&self) -> Arc<dyn MemoryCopyWriteback> {
        self.copy_writeback.clone()
    }
}

/// Copy/FUSE views and the original invalidator identity of one decorated source.
/// A root derives this once from its actual backend; mounts cannot mix views.
///
/// ```compile_fail
/// use std::sync::Arc;
/// use awaken_resource_contract::{MemoryMountSource, MemoryQueries, MemorySnapshotSource,
///     MemoryCopyWriteback, MemoryCopyConflictPreservation, MemoryFilesystem, MemoryInvalidator};
/// fn cannot_mix(queries: Arc<dyn MemoryQueries>, snapshots: Arc<dyn MemorySnapshotSource>,
///     writeback: Arc<dyn MemoryCopyWriteback>, conflict_preservation: Arc<dyn MemoryCopyConflictPreservation>,
///     filesystem: Arc<dyn MemoryFilesystem>, invalidator: Arc<dyn MemoryInvalidator>) {
///     let _ = MemoryMountSource { queries, snapshots, writeback, conflict_preservation, filesystem, invalidator };
/// }
/// ```
#[derive(Clone)]
pub struct MemoryMountSource {
    queries: Arc<dyn MemoryQueries>,
    snapshots: Arc<dyn MemorySnapshotSource>,
    writeback: Arc<dyn MemoryCopyWriteback>,
    conflict_preservation: Arc<dyn MemoryCopyConflictPreservation>,
    filesystem: Arc<dyn MemoryFilesystem>,
    invalidator: Arc<dyn MemoryInvalidator>,
}
impl MemoryMountSource {
    #[must_use]
    pub fn new<T>(backend: Arc<T>) -> Self
    where
        T: MemoryQueries
            + MemorySnapshotSource
            + MemoryCopyWriteback
            + MemoryCopyConflictPreservation
            + MemoryFilesystem
            + MemoryInvalidationIdentity
            + 'static,
    {
        let invalidator = backend.invalidator_identity();
        Self {
            queries: backend.clone(),
            snapshots: backend.clone(),
            writeback: backend.clone(),
            conflict_preservation: backend.clone(),
            filesystem: backend,
            invalidator,
        }
    }
    #[must_use]
    pub fn queries(&self) -> Arc<dyn MemoryQueries> {
        self.queries.clone()
    }
    #[must_use]
    pub fn snapshots(&self) -> Arc<dyn MemorySnapshotSource> {
        self.snapshots.clone()
    }
    #[must_use]
    pub fn writeback(&self) -> Arc<dyn MemoryCopyWriteback> {
        self.writeback.clone()
    }
    #[must_use]
    pub fn conflict_preservation(&self) -> Arc<dyn MemoryCopyConflictPreservation> {
        self.conflict_preservation.clone()
    }
    #[must_use]
    pub fn filesystem(&self) -> Arc<dyn MemoryFilesystem> {
        self.filesystem.clone()
    }
    #[must_use]
    pub fn uses_invalidator(&self, candidate: &Arc<dyn MemoryInvalidator>) -> bool {
        Arc::ptr_eq(&self.invalidator, candidate)
    }
}

/// Managed content operations derived from the same concrete Memory authority.
/// Store metadata/lifecycle remains an independent application authority.
///
/// ```compile_fail
/// use std::sync::Arc;
/// use awaken_resource_contract::{MemoryManagementContent, MemorySnapshotSource,
///     MemoryWriter, MemoryVersionQueries, MemoryVersionRedaction, MemoryCopyConflictQueries};
/// fn cannot_mix(snapshots: Arc<dyn MemorySnapshotSource>, writer: Arc<dyn MemoryWriter>,
///     versions: Arc<dyn MemoryVersionQueries>, redaction: Arc<dyn MemoryVersionRedaction>,
///     conflict_queries: Arc<dyn MemoryCopyConflictQueries>) {
///     let _ = MemoryManagementContent { snapshots, writer, versions, redaction, conflict_queries };
/// }
/// ```
#[derive(Clone)]
pub struct MemoryManagementContent {
    snapshots: Arc<dyn MemorySnapshotSource>,
    writer: Arc<dyn MemoryWriter>,
    versions: Arc<dyn MemoryVersionQueries>,
    redaction: Arc<dyn MemoryVersionRedaction>,
    conflict_queries: Arc<dyn MemoryCopyConflictQueries>,
}
impl MemoryManagementContent {
    #[must_use]
    pub fn new<T>(backend: Arc<T>) -> Self
    where
        T: MemorySnapshotSource
            + MemoryWriter
            + MemoryVersionQueries
            + MemoryVersionRedaction
            + MemoryCopyConflictQueries
            + 'static,
    {
        Self {
            snapshots: backend.clone(),
            writer: backend.clone(),
            versions: backend.clone(),
            redaction: backend.clone(),
            conflict_queries: backend,
        }
    }
    #[must_use]
    pub fn snapshots(&self) -> Arc<dyn MemorySnapshotSource> {
        self.snapshots.clone()
    }
    #[must_use]
    pub fn writer(&self) -> Arc<dyn MemoryWriter> {
        self.writer.clone()
    }
    #[must_use]
    pub fn versions(&self) -> Arc<dyn MemoryVersionQueries> {
        self.versions.clone()
    }
    #[must_use]
    pub fn redaction(&self) -> Arc<dyn MemoryVersionRedaction> {
        self.redaction.clone()
    }
    #[must_use]
    pub fn conflict_queries(&self) -> Arc<dyn MemoryCopyConflictQueries> {
        self.conflict_queries.clone()
    }
}

#[cfg(test)]
mod memory_contract_tests {
    use super::{memory_store_stem, skill_catalog_id};

    /// Cause/effect rules: safe ASCII is preserved; each unsafe run collapses
    /// to one dash; edge dashes are trimmed; an all-unsafe id becomes the fixed
    /// non-empty stem; output is bounded to 120 bytes. Store and mount adapters
    /// both call this contract owner, so these cases cover both boundaries.
    #[test]
    fn memory_store_stem_path_safety_rules() {
        assert_eq!(memory_store_stem("store_A-1"), "store_A-1");
        assert_eq!(memory_store_stem("../tenant///memory"), "tenant-memory");
        assert_eq!(memory_store_stem("///"), "memstore");
        assert_eq!(memory_store_stem(&"a".repeat(200)).len(), 120);
    }

    #[test]
    fn skill_catalog_identity_rules() {
        // Cause/effect decision table:
        // | Rule | canonical stems equal | raw names equal | Effect |
        // | S1 | yes | either | one stable tagged id |
        // | S2 | no | no | distinct tagged ids |
        // | S3 | yes | yes | restart-stable id |
        // Identity is derived only after canonical stem normalization, so every
        // ingress/store/runtime adapter observes the same source of truth.
        assert_eq!(
            skill_catalog_id("My Skill"),
            skill_catalog_id("My-Skill"),
            "S1"
        );
        assert_ne!(
            skill_catalog_id("skill-a"),
            skill_catalog_id("skill-b"),
            "S2"
        );
        assert_eq!(skill_catalog_id("stable"), skill_catalog_id("stable"), "S3");
        assert!(skill_catalog_id("stable").starts_with("skill_"), "S1");
    }
}

#[cfg(test)]
mod content_identity_tests {
    use super::content_id;

    #[test]
    fn content_fingerprint_is_blake3_and_matches_file_store() {
        // The former Sandbox alias now has no implementation or public path:
        // File stores, Sandbox provisioning and transport verification all use
        // this owner. The retained test therefore checks an independent digest
        // instead of comparing the owner with a forwarding alias to itself.
        // | Rule | bytes | effect |
        // | H1 | empty | standard BLAKE3 golden identity, 64 hex digits |
        // | H2 | provisioned blob | 64 hex digits; cannot regress to the former 16-hex hash |
        assert_eq!(
            content_id(b""),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
            "H1"
        );
        assert_eq!(content_id(b"provisioned bytes").len(), 64, "H2");
    }
}
