//! Write-through memory-store FUSE server (ADR-0053).
//!
//! A [`MemoryFuse`](fuse::MemoryFuse) projects a path-addressed [`awaken_resource_contract::MemoryFilesystem`] store as
//! a filesystem: reads are lazy through a short-TTL LRU cache, writes buffer per-fd
//! and flush on `flush`/`release` as a **CAS `update`** (keeping the buffer and
//! returning `EAGAIN` on conflict, never clobbering). Ported from awaken-next's
//! `awaken-sandbox-memoryd`, adapted to call an **in-process** Memory content roles
//! and to report **faithful `getattr` timestamps** from the store's record.
//!
//! On Linux, the `fuse` feature (default) gates the fuser-backed mount ([`fuse`]);
//! the pure helpers and copy fallback build on every platform.

use std::collections::BTreeMap;

use awaken_resource_contract::{MAX_MEMORY_BYTES, MemErr, MemoryEntry};

pub mod copy;
#[cfg(all(feature = "fuse", target_os = "linux"))]
pub mod fuse;
pub mod mounter;

pub use copy::{
    CopySnapshot, HarvestConflict, HarvestReport, fuse_available, harvest, materialize,
};
#[cfg(all(feature = "fuse", target_os = "linux"))]
pub use fuse::spawn_mount;
pub use mounter::{MemoryMounterConstructionError, MemoryStoreMounter};

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
    /// A local I/O operation the mount depends on failed: creating the async
    /// runtime, spawning the FUSE session or writing a copy-mode file.
    #[error("internal: {0}")]
    Io(#[from] std::io::Error),
    /// Buffered or spliced bytes are not UTF-8; the memory store holds text.
    #[error("internal: {0}")]
    NonUtf8(#[from] std::string::FromUtf8Error),
    /// A poisoned inode table keeps its typed cause until the Sandbox boundary.
    /// FUSE reports the same local EIO as every other internal fault.
    #[error("internal: {0}")]
    LockPoisoned(#[from] awaken_sync::LockPoisoned),
    /// A violated memoryd invariant (an invalid head, a negative offset)
    /// described by its own fixed message.
    #[error("internal: {0}")]
    Internal(String),
}

impl From<FuseError> for awaken_provisioning_contract::SandboxError {
    /// A full dirty-write budget clears once buffered writes flush; the store's
    /// own classes pass through unchanged.
    fn from(error: FuseError) -> Self {
        let detail = error.to_string();
        match error {
            FuseError::Mem(error) => error.into(),
            FuseError::DirtyFileLimitExceeded => Self::NotReady(detail),
            FuseError::NotFound(_) => Self::NotFound(detail),
            FuseError::TooLarge => Self::Invalid(detail),
            FuseError::Io(cause) => Self::backend("memory FUSE", cause),
            FuseError::NonUtf8(cause) => Self::backend("memory FUSE", cause),
            FuseError::LockPoisoned(cause) => Self::backend("memory FUSE", cause),
            FuseError::Internal(cause) => Self::backend("memory FUSE", cause),
        }
    }
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
    String::from_utf8(bytes).map_err(FuseError::NonUtf8)
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
    fn splice_rejects_a_negative_offset() {
        // A negative pwrite offset is a fail-closed error, never wrapped to a huge usize.
        assert!(matches!(
            splice_bytes("hello", -1, b"x"),
            Err(FuseError::Internal(_))
        ));
    }

    #[test]
    fn local_faults_keep_their_source_and_one_backend_class() {
        // Cause -> effect: an I/O fault, a non-UTF-8 buffer and a fixed
        // invariant message each keep their own variant, render the same
        // `internal:` text, and map to the one memory-FUSE backend failure
        // whose detail is the source's own text.
        // P1 holder panic under the actual InvariantLock -> later acquisition
        // refuses with LockPoisoned; P2 conversion preserves that typed source,
        // rather than flattening it into an invariant message. P3 only the
        // existing Sandbox boundary renders it to Backend("memory FUSE", text).
        // Display remains `internal: {text}`; fuse::errno's existing fallback
        // remains EIO. This local test does not require a mounted filesystem.
        use awaken_provisioning_contract::SandboxError;
        let lock = awaken_sync::InvariantLock::new(());
        assert!(
            std::panic::catch_unwind(|| {
                let _guard = lock.lock().unwrap();
                panic!("poison the inode-table boundary fixture");
            })
            .is_err()
        );
        let poisoned = lock.lock().unwrap_err();
        let poison_text = poisoned.to_string();
        let poison_error = FuseError::from(poisoned);
        assert!(matches!(&poison_error, FuseError::LockPoisoned(source) if *source == poisoned));
        assert_eq!(
            std::error::Error::source(&poison_error)
                .and_then(|source| source.downcast_ref::<awaken_sync::LockPoisoned>()),
            Some(&poisoned),
        );
        let io = std::io::Error::other("mount refused");
        let utf8 = String::from_utf8(vec![0xFF]).unwrap_err();
        let utf8_text = utf8.to_string();
        for (error, detail) in [
            (FuseError::from(io), "mount refused".to_string()),
            (FuseError::from(utf8), utf8_text),
            (poison_error, poison_text),
            (
                FuseError::Internal("invariant".into()),
                "invariant".to_string(),
            ),
        ] {
            assert_eq!(error.to_string(), format!("internal: {detail}"));
            match SandboxError::from(error) {
                SandboxError::Backend {
                    operation,
                    detail: mapped,
                } => {
                    assert_eq!(operation, "memory FUSE");
                    assert_eq!(mapped, detail);
                }
                other => panic!("local fault left the backend class: {other:?}"),
            }
        }
    }

    #[test]
    fn splice_rejects_a_non_utf8_result() {
        // The memory store is text; writing raw non-UTF-8 bytes must fail closed rather
        // than materialize an invalid string.
        assert!(matches!(
            splice_bytes("hello", 0, &[0xFF, 0xFE]),
            Err(FuseError::NonUtf8(_))
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
