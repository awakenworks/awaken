//! Port-only contract for the resources plane (files, memory, repositories, skills).
//!
//! The three mountable-resource **ports** — [`FileStore`], [`MemoryFs`],
//! [`SkillStore`] — plus the value/error types in their signatures,
//! and **nothing else**: no backend, no SQL driver, no filesystem. It mirrors
//! [`awaken-provisioning-contract`](https://docs.rs/awaken-provisioning-contract):
//! an adapter (or a consumer reusing these stores inside its own database) can
//! depend on the traits alone without pulling `sqlx`/`rusqlite`/`object_store` or
//! any concrete store.
//!
//! The backend crates (`awaken-file-store`, `awaken-memory-store`,
//! `awaken-skill-store`) implement these ports and **re-export** every item here,
//! so existing paths like `awaken_file_store::FileStore` keep resolving unchanged.
//!
//! Note there is deliberately **no `SecretSource` port**: a secret is resolved by
//! reference through `awaken_provisioning_contract::BlobSource` straight into the
//! sandbox, never surfaced as a value here.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

mod catalog;
mod input;

pub use catalog::{
    ClonePolicy, ConfigVersion, ExtractionPolicy, MemoryStoreConfigVersion, MemoryStoreDefinition,
    RecallPolicy, RepositoryConfigVersion, RepositoryDefinition, ResolvedMemoryStoreConfig,
    ResolvedRepositoryConfig, ResourceCatalog, ResourceCatalogError, ResourceConfigSource,
    ResourceState, RetentionPolicy,
};
pub use input::{
    BindingId, FileId, InputBinding, InputResourceId, MemoryStoreId, RepositoryId, ResourceAccess,
};

// ---------------------------------------------------------------------------
// File store (ADR-0041): content-addressed, immutable blob port.
// ---------------------------------------------------------------------------

/// A blob store failure.
#[derive(Debug, thiserror::Error)]
#[error("file store error: {0}")]
pub struct FileStoreError(pub String);

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
}

/// One immutable version of a Skill bundle. The version freezes authored Skill
/// content; it contains no principal, role, policy, API key, or runtime host path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillVersion {
    pub id: String,
    pub skill_id: String,
    pub version: u64,
    pub name: String,
    pub description: String,
    pub directory: String,
    pub bundle_sha256: String,
    pub files: Vec<SkillBundleFile>,
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
    pub id: String,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_title: Option<String>,
    pub latest_version: u64,
    /// Highest version number ever assigned. It never decreases or reuses a
    /// retired version number.
    pub last_version: u64,
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
    /// Delete the complete Skill aggregate. Idempotent.
    async fn delete_skill(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<bool, SkillStoreError>;
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
// Memory FS (ADR-0053): path-addressed, CAS memory port + its value types.
// ---------------------------------------------------------------------------

/// Hard cap on a single memory's content (Anthropic parity, ADR-0057). Enforced on
/// every create/update before any allocation.
pub const MAX_MEMORY_BYTES: usize = 102_400;
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
/// content-less [`Memory`], so a `MemoryFs::list` result crosses the managed/HTTP
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

/// A [`MemoryFs`] failure.
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
pub trait MemoryFs: Send + Sync {
    /// Memories whose path is at or under `prefix` (`"/"` or `""` = all).
    async fn list(&self, store: &str, prefix: &str) -> Result<Vec<MemoryEntry>, MemErr>;
    /// The memory at `path`, or `None`.
    async fn get_by_path(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr>;
    /// Create a memory at `path`. `PathConflict` if it already exists.
    async fn create(&self, store: &str, path: &str, content: &str) -> Result<Memory, MemErr>;
    /// Compare-and-swap the content of memory `id`: succeeds only if the stored sha
    /// equals `base_sha` (or the new content is already current — idempotent). On a
    /// mismatch returns [`MemErr::Conflict`] carrying the live memory; never clobbers.
    async fn update(
        &self,
        store: &str,
        id: &str,
        content: &str,
        base_sha: &str,
    ) -> Result<Memory, MemErr>;
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
}
