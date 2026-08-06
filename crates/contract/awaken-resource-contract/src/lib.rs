//! Contract-only vocabulary for the Resources context (files, memory, repositories, skills).
//!
//! The mountable-resource **SPIs** — [`FileStore`] + [`FileCatalog`] as the two
//! capabilities of one File aggregate, [`MemoryRepository`], and [`SkillStore`]
//! — plus the value/error types in their signatures,
//! and **nothing else**: no backend, no SQL driver, no filesystem. It mirrors
//! [`awaken-provisioning-contract`](https://docs.rs/awaken-provisioning-contract):
//! an adapter (or a consumer reusing these stores inside its own database) can
//! depend on the traits alone without pulling `sqlx`/`rusqlite`/`object_store` or
//! any concrete store.
//!
//! The backend crates (`awaken-file-store`, `awaken-memory-store`,
//! `awaken-skill-store`) implement these SPIs and **re-export** every item here,
//! so existing paths like `awaken_file_store::FileStore` keep resolving unchanged.
//!
//! Note there is deliberately **no `SecretSource` port**: a secret is resolved by
//! reference through `awaken_provisioning_contract::BlobSource` straight into the
//! sandbox, never surfaced as a value here.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

mod catalog;
mod component;
mod input;
mod lifecycle;
mod memory_application;

pub use catalog::{
    ClonePolicy, ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition,
    RepositoryConfigVersion, RepositoryDefinition, ResourceBindingValidator, ResourceCatalog,
    ResourceCatalogError, ResourceCatalogRules, ResourceConfigSource, ResourceState,
    ResourceTimestamps, RetentionPolicy,
};
pub use component::{ResourceComponent, ResourceDependencies, build_resource_component};
pub use input::{
    BindingId, FileId, InputBinding, InputResourceId, MemoryStoreId, RepositoryId, ResourceAccess,
    SkillId, SkillVersionId,
};
pub use lifecycle::{
    AcquireResourceReclamationOutcome, AgentResourceReferenceSource, PutResourcePurgeOutcome,
    ResourceKind, ResourceLifecycleRepository, ResourcePhysicalReclaimer, ResourcePurgeError,
    ResourcePurgeEvidence, ResourcePurgeGuard, ResourcePurgeIntent, ResourcePurgeReceipt,
    ResourcePurgeRepository, ResourcePurgeScheduler, ResourcePurgeStatus, ResourceReclamationFence,
    ResourceReference, ResourceReferenceIndex, ResourceReferenceKind, ResourceReferenceRecord,
    ResourceTarget,
};
pub use memory_application::{
    CreateMemoryStoreCommand, MemoryStoreApplicationError, MemoryStoreApplicationService,
    UpdateMemoryStoreCommand,
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

/// A blob store failure.
#[derive(Debug, thiserror::Error)]
#[error("file store error: {0}")]
pub struct FileStoreError(pub String);

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
    pub downloadable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logical_path: Option<String>,
    /// Stable internal idempotency key for Sandbox-output harvest. Uploads have
    /// no key because every upload creates an independent logical File.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harvest_key: Option<String>,
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

#[derive(Debug, thiserror::Error)]
pub enum FileCatalogError {
    #[error("invalid file record: {0}")]
    Invalid(String),
    #[error("file catalog storage error: {0}")]
    Storage(String),
}

/// Durable logical Files-API catalog. It owns metadata, Workspace visibility,
/// Session scope, and harvest idempotency; bytes remain exclusively in
/// [`FileStore`]. Lists are newest-first with `id` as the deterministic tie-break.
#[async_trait]
pub trait FileCatalog: Send + Sync {
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

    async fn create_uploaded_file(
        &self,
        workspace_id: &str,
        filename: String,
        mime_type: String,
        bytes: &[u8],
    ) -> Result<FileRecord, ResourcePurgeError>;

    async fn create_generated_file(
        &self,
        workspace_id: &str,
        filename: String,
        mime_type: String,
        bytes: &[u8],
        idempotency_key: String,
    ) -> Result<FileRecord, ResourcePurgeError>;

    async fn create_artifact(
        &self,
        workspace_id: &str,
        session_id: &str,
        logical_path: String,
        mime_type: String,
        bytes: &[u8],
        idempotency_key: String,
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
    #[error("invalid skill resource: {0}")]
    Invalid(String),
    #[error("io: {0}")]
    Io(String),
    #[error("storage: {0}")]
    Storage(String),
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

/// A durable, workspace-scoped repository for the complete Skill aggregate.
/// Authorization happens before this port is invoked; every operation is scoped by
/// the trusted Workspace and can only observe resources owned by that Workspace.
#[async_trait]
pub trait SkillStore: Send + Sync {
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

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        bytes.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
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

/// A directory-listing entry (no content). Serializes with the same field names as a
/// content-less [`Memory`], so a `MemoryRepository::list` result crosses the managed/HTTP
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
    pub redacted_unix_nanos: Option<u128>,
}

/// Idempotent whole-store physical reclamation result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryPurgeSummary {
    pub heads_deleted: u64,
    pub versions_deleted: u64,
}

/// A [`MemoryRepository`] failure.
#[derive(Debug, thiserror::Error)]
pub enum MemErr {
    #[error("memory not found: {0}")]
    NotFound(String),
    /// Optimistic-concurrency failure: the base sha no longer matches. `current`
    /// carries the server's live memory (sha + content) for diagnostics; the caller
    /// must not clobber.
    #[error("cas conflict on {}: base sha stale", .current.path)]
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

/// A path-addressed, CAS memory aggregate repository — the seam a write-through
/// FUSE mount and the managed API share (ADR-0053/ADR-0063). All methods are
/// store-scoped by an opaque, globally-unique `store` id.
///
/// Every successful, state-changing create/update/rename/delete appends exactly
/// one or more [`MemoryVersion`] rows in the same atomic repository operation.
/// Idempotent updates/deletes append nothing. This invariant is what keeps API
/// versions, runtime recall/extraction, and mounted content on one source of truth.
#[async_trait]
pub trait MemoryRepository: Send + Sync {
    /// Atomically read every current file head, including content, from one
    /// store. The returned vector is path-ordered. This is the canonical frozen
    /// input primitive for operations such as Dream; callers must
    /// not emulate it with `list` followed by per-path reads because concurrent
    /// writes could create a mixed-generation snapshot.
    async fn snapshot_heads(&self, store: &str) -> Result<Vec<Memory>, MemErr>;
    /// Memories whose path is at or under `prefix` (`"/"` or `""` = all).
    async fn list(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr>;
    /// The memory at `path`, or `None`.
    async fn get_by_path(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr>;
    /// Create a memory at `path`. `PathConflict` if it already exists.
    async fn create(&self, store: &str, path: &str, content: &str) -> Result<Memory, MemErr>;
    /// Atomically compare-and-swap the head of memory `id`, optionally moving it to
    /// `target_path` in the same repository operation. Content, path, displaced-target
    /// deletion, and history either all commit or all roll back. A stale `base_sha`
    /// is idempotent only when both requested content and path are already current;
    /// otherwise it returns [`MemErr::Conflict`] carrying the live memory.
    async fn update_head(
        &self,
        store: &str,
        id: &str,
        content: &str,
        base_sha: &str,
        target_path: Option<&str>,
    ) -> Result<Memory, MemErr>;
    /// Content-only convenience over [`MemoryRepository::update_head`].
    async fn update(
        &self,
        store: &str,
        id: &str,
        content: &str,
        base_sha: &str,
    ) -> Result<Memory, MemErr> {
        self.update_head(store, id, content, base_sha, None).await
    }
    /// Move `from` → `to` atomically, **replacing** `to` if it exists (POSIX rename
    /// semantics). The memory keeps its id (so an open FUSE fd stays valid) and bumps
    /// its version.
    async fn rename(&self, store: &str, from: &str, to: &str) -> Result<Memory, MemErr>;
    /// Delete the memory at `path` (idempotent — deleting an absent path is `Ok`).
    async fn delete_by_path(&self, store: &str, path: &str) -> Result<(), MemErr>;
    /// Delete `path` only while it is still the exact head observed by a caller.
    /// Returns `false` when the path is already absent (the requested outcome is
    /// already true). A different id or sha returns [`MemErr::Conflict`] carrying
    /// the live head and never deletes it. This closes the create/delete ABA window
    /// for materialized-copy reconciliation.
    async fn delete_if_match(
        &self,
        store: &str,
        path: &str,
        base_id: &str,
        base_sha: &str,
    ) -> Result<bool, MemErr>;
    /// Ordered version history for this store.
    async fn list_versions(&self, store: &str) -> Result<Vec<MemoryVersion>, MemErr>;
    /// Redact one historical version's content. Returns `None` when the version
    /// does not belong to this store. Repeated redaction is idempotent.
    async fn redact_version(
        &self,
        store: &str,
        version_id: &str,
    ) -> Result<Option<MemoryVersion>, MemErr>;
    /// Physically remove every live head and immutable history row in `store`.
    /// The lifecycle reclaimer calls this only after tombstone, retention,
    /// activation and extraction guards pass. Repeated calls return zero counts.
    async fn purge_store(&self, store: &str) -> Result<MemoryPurgeSummary, MemErr>;
}
