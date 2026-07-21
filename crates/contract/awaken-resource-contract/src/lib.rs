//! Port-only contract for the resources plane (files, memory, repositories, skills).
//!
//! The four mountable-resource **ports** — [`FileStore`], [`MemoryBlobStore`],
//! [`MemoryFs`], [`SkillStore`] — plus the value/error types in their signatures,
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

pub use catalog::{
    ClonePolicy, ConfigVersion, ExtractionPolicy, MemoryStoreConfigVersion, MemoryStoreDefinition,
    RecallPolicy, RepositoryConfigVersion, RepositoryDefinition, ResolvedMemoryStoreConfig,
    ResolvedRepositoryConfig, ResourceCatalog, ResourceCatalogError, ResourceState,
    RetentionPolicy,
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
// Skill store: durable, workspace-scoped SKILL.md catalog port.
// ---------------------------------------------------------------------------

/// A skill-store failure.
#[derive(Debug, thiserror::Error)]
pub enum SkillStoreError {
    #[error("io: {0}")]
    Io(String),
    #[error("storage: {0}")]
    Storage(String),
}

/// A durable, workspace-scoped catalog of `SKILL.md` bodies, addressed by a stable
/// id. `put` returns the sanitized id the skill is addressable by (what `list`
/// reports); `list` is sorted by id for a stable catalog. Async so a network-DB
/// backend fits; the filesystem/in-memory backends satisfy it trivially.
#[async_trait]
pub trait SkillStore: Send + Sync {
    /// Store (or overwrite) `content` under `id` in `workspace_id`; returns the safe
    /// id (sanitized stem) it is addressable by.
    async fn put(
        &self,
        workspace_id: &str,
        id: &str,
        content: &str,
    ) -> Result<String, SkillStoreError>;
    /// The content under `id`, or `None` if absent.
    async fn get(&self, workspace_id: &str, id: &str) -> Result<Option<String>, SkillStoreError>;
    /// Every skill in the workspace as `(id, content)`, sorted by id.
    async fn list(&self, workspace_id: &str) -> Result<Vec<(String, String)>, SkillStoreError>;
    /// Delete a skill; returns whether it existed. Idempotent.
    async fn delete(&self, workspace_id: &str, id: &str) -> Result<bool, SkillStoreError>;
}

// ---------------------------------------------------------------------------
// Memory blob store (ADR-0038): id-keyed byte store port.
// ---------------------------------------------------------------------------

/// A memory-store failure.
#[derive(Debug, thiserror::Error)]
pub enum MemoryStoreError {
    #[error("io: {0}")]
    Io(String),
    #[error("storage: {0}")]
    Storage(String),
}

/// A durable, id-keyed, workspace-scoped byte store: one blob per id. Backs the
/// ADR-0038 `memory_store` resource family. `create` mints a dense, globally-unique
/// `memstore_<n>` id and writes it empty so it resolves before any write-back. Async
/// so a network-DB backend fits; the filesystem/in-memory backends satisfy it
/// trivially. Mirrors [`FileStore`].
#[async_trait]
pub trait MemoryBlobStore: Send + Sync {
    /// Mint a new, empty store and return its stable id.
    async fn create(&self, workspace_id: &str) -> Result<String, MemoryStoreError>;
    /// Overwrite the bytes under `id` (the harvest write-back path).
    async fn put(&self, workspace_id: &str, id: &str, bytes: &[u8])
    -> Result<(), MemoryStoreError>;
    /// The bytes under `id`, or `None` if no such store exists.
    async fn get(&self, workspace_id: &str, id: &str) -> Result<Option<Vec<u8>>, MemoryStoreError>;
    /// Whether a store with `id` exists.
    async fn exists(&self, workspace_id: &str, id: &str) -> Result<bool, MemoryStoreError>;
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

/// A path-addressed, CAS memory store — the seam a write-through FUSE mount calls
/// (ADR-0053). All methods are store-scoped by an opaque, globally-unique `store` id.
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
}
