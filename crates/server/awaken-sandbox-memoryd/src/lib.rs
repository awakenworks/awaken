//! Write-through memory-store FUSE server (ADR-0053).
//!
//! A [`MemoryFuse`](fuse::MemoryFuse) projects a path-addressed [`MemoryFs`] store as
//! a filesystem: reads are lazy through a short-TTL LRU cache, writes buffer per-fd
//! and flush on `flush`/`release` as a **CAS `update`** (keeping the buffer and
//! returning `EAGAIN` on conflict, never clobbering). Ported from awaken-next's
//! `awaken-sandbox-memoryd`, adapted to call an **in-process** `MemoryFs` (not HTTP)
//! and to report **faithful `getattr` timestamps** from the store's record.
//!
//! The `fuse` feature (default) gates the fuser-backed mount ([`fuse`]); the pure
//! helpers below build without it.

use std::collections::BTreeMap;

use awaken_memory_store::{MAX_MEMORY_BYTES, MemErr, MemoryEntry};

pub mod coordinator;
#[cfg(feature = "fuse")]
pub mod fuse;

#[cfg(feature = "fuse")]
pub use coordinator::FuseMountFactory;
pub use coordinator::{Mount, MountCoordinator, MountFactory};

/// A memoryd operation failure — the store's [`MemErr`] plus FUSE-local faults
/// (a full dirty-fd budget, a missing handle, or an internal encoding fault).
#[derive(Debug, thiserror::Error)]
pub enum FuseError {
    #[error(transparent)]
    Mem(#[from] MemErr),
    /// The buffered-write budget is full; admitting another dirty fd would risk
    /// dropping buffered writes, so we fail closed (surfaced as `EAGAIN`).
    #[error("dirty open-file limit exceeded")]
    DirtyFileLimitExceeded,
    /// A handle or inode that should exist does not (surfaced as `ENOENT`).
    #[error("not found: {0}")]
    NotFound(String),
    /// Content over the size cap (surfaced as `EIO`; the store also rejects it).
    #[error("content exceeds {MAX_MEMORY_BYTES} bytes")]
    TooLarge,
    /// An internal fault — non-UTF-8 buffer, runtime construction, mount config.
    #[error("internal: {0}")]
    Internal(String),
}

/// Positional overwrite/extend of `original` with `data` at `offset`, returning the
/// new UTF-8 content. Caps the projected size **before** any allocation so a large
/// agent-controlled offset (e.g. a 1 TiB `pwrite`) cannot abort the process on an
/// unbounded `Vec::resize`; zero-fills any gap.
pub fn splice_bytes(original: &str, offset: i64, data: &[u8]) -> Result<String, FuseError> {
    let offset =
        usize::try_from(offset).map_err(|_| FuseError::Internal("negative write offset".into()))?;
    let mut bytes = original.as_bytes().to_vec();
    let end = offset.saturating_add(data.len());
    let projected_len = bytes.len().max(end);
    if projected_len > MAX_MEMORY_BYTES {
        return Err(FuseError::TooLarge);
    }
    if offset > bytes.len() {
        bytes.resize(offset, 0);
    }
    if end > bytes.len() {
        bytes.resize(end, 0);
    }
    bytes[offset..end].copy_from_slice(data);
    String::from_utf8(bytes).map_err(|e| FuseError::Internal(e.to_string()))
}

/// The immediate children of `dir` rolled up one level from a flat memory listing:
/// `name → is_dir` (a nested path contributes its first segment as a directory).
pub fn immediate_children(memories: &[MemoryEntry], dir: &str) -> BTreeMap<String, bool> {
    let prefix = if dir == "/" {
        "/".to_string()
    } else {
        format!("{}/", dir.trim_end_matches('/'))
    };
    let mut children = BTreeMap::new();
    for memory in memories {
        let Some(rest) = memory.path.strip_prefix(&prefix) else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        if let Some((name, _)) = rest.split_once('/') {
            children.insert(name.to_string(), true);
        } else {
            children.insert(rest.to_string(), false);
        }
    }
    children
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str) -> MemoryEntry {
        MemoryEntry {
            id: format!("id{path}"),
            path: path.to_string(),
            content_sha256: String::new(),
            content_size: 0,
            version: 1,
            updated_unix_nanos: 0,
        }
    }

    #[test]
    fn splice_overwrites_and_extends_with_zero_fill() {
        assert_eq!(splice_bytes("hello", 0, b"J").unwrap(), "Jello");
        // A gap past the end zero-fills; the bytes are UTF-8 NULs.
        assert_eq!(splice_bytes("ab", 4, b"z").unwrap(), "ab\0\0z");
    }

    #[test]
    fn splice_rejects_oversized_projection_before_allocating() {
        // A huge offset would blow up Vec::resize; it is rejected up front.
        assert!(matches!(
            splice_bytes("x", i64::MAX / 2, b"y"),
            Err(FuseError::TooLarge)
        ));
    }

    #[test]
    fn immediate_children_rolls_up_one_level() {
        let memories = vec![
            entry("/notes/today.md"),
            entry("/notes/deep/a.md"),
            entry("/root.md"),
        ];
        let kids = immediate_children(&memories, "/notes");
        assert_eq!(kids.get("today.md"), Some(&false));
        assert_eq!(
            kids.get("deep"),
            Some(&true),
            "a nested path is a directory"
        );
        assert!(!kids.contains_key("root.md"));

        let root = immediate_children(&memories, "/");
        assert_eq!(root.get("notes"), Some(&true));
        assert_eq!(root.get("root.md"), Some(&false));
    }
}
