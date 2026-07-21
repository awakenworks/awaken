//! Durable memory persistence for the resources plane.
//!
//! The [`MemoryFs`] port with pluggable in-memory, filesystem, SQLite, and Postgres
//! backends. A store is addressed only by an opaque, globally unique id; workspace
//! ownership and authorization deliberately remain outside this storage adapter in
//! the resource catalog and authorization edge respectively.
//!
//! Recall, extraction, API history, and mounts all use this same store-scoped
//! aggregate; there is no Host-global extraction directory.

/// Path-addressed, CAS memory model (ADR-0053): the `MemoryFs` port a write-through
/// FUSE mount projects.
pub mod memfs;

pub use memfs::{
    FsMemoryFs, InMemoryFs, MAX_MEMORY_BYTES, MemErr, Memory, MemoryEntry, MemoryFs, MemoryVersion,
    MemoryVersionOperation, sha256_hex,
};

#[cfg(feature = "postgres")]
mod postgres;
#[cfg(any(feature = "sqlite", feature = "postgres"))]
mod schema;
#[cfg(feature = "sqlite")]
mod sqlite;

#[cfg(feature = "postgres")]
pub use postgres::{PgMemoryFs, PgStoreError};
#[cfg(any(feature = "sqlite", feature = "postgres"))]
pub use schema::{BUNDLE_ID, memory_store_bundle};
#[cfg(feature = "sqlite")]
pub use sqlite::{SqliteMemoryFs, StoreError};

/// Reduce `name` to a safe single stem: keep alphanumerics, `-`, `_`; map every other
/// run to a single `-`; never empty. So a crafted store/workspace id can name neither
/// a file that escapes a store root nor a FUSE mountpoint that escapes its parent.
pub fn sanitize_stem(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.truncate(120); // bound the stem so a crafted long id can't exceed NAME_MAX
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "memstore".to_string()
    } else {
        trimmed
    }
}
