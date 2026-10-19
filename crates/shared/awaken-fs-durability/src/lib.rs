// @formal surface requirement=resource.files.resource_lifecycle
// @formal reason: Provider-neutral filesystem durability port used after each
// @formal reason: owning store or Sandbox adapter has decided what to publish or
// @formal reason: remove. It owns physical directory durability and optional native mechanics,
// @formal reason: not resource identity or authorization, and claims no proof.
//! Shared filesystem durability. Directory flush remains available by default;
//! the original native filesystem component is enabled only by `native-fs`.

// Native capability selection is compile-time only. An original FS consumer
// requests native-fs in its dependency; no missing-feature runtime fallback.
#[cfg(feature = "native-fs")]
mod native;
#[cfg(all(feature = "native-fs", target_os = "linux"))]
pub use native::PinnedUnixSocketPath;
#[cfg(feature = "native-fs")]
pub use native::{
    DirectoryIdentity, ExclusiveFileLock, PathEntry, ProjectionRoot, TreeDirectory, TreeFile,
    TreeReadLimits, TreeSnapshot, classify_nofollow, clear_directory_contents_exact,
    create_directory_noreplace, create_private_directory_stage, create_relative_directory_all,
    directory_identity_nofollow, is_portable_component, is_reserved_component,
    publish_directory_noreplace, publish_file_noreplace, read_regular_file_nofollow,
    read_regular_tree_nofollow, read_tree_nofollow, read_tree_nofollow_bounded,
    read_tree_nofollow_excluding, read_tree_nofollow_with_limits, remove_directory_tree_exact,
    remove_regular_file_nofollow, remove_relative_entry_exact, replace_regular_file_atomic,
    set_relative_directory_mode, try_lock_exclusive, write_relative_file_atomic,
    zero_relative_regular_file_nofollow,
};

#[cfg(not(windows))]
use std::fs::File;
use std::io;
use std::path::Path;

/// Flush directory metadata after an owning adapter creates, renames, or
/// removes an entry. A file or absent path is never accepted as a directory.
#[cfg_attr(
    not(feature = "native-fs"),
    doc = r#"
Compile capability F0: the default flush-only package exposes no native lock.
This source fence complements, and does not replace, the actual dependency graph.
```compile_fail,E0432
use awaken_fs_durability::ExclusiveFileLock;
```
"#
)]
pub fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    let directory = {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?
    };
    #[cfg(not(windows))]
    let directory = File::open(path)?;

    if !directory.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "directory durability requires a directory handle",
        ));
    }
    directory.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_directory_flush_accepts_only_a_real_directory() {
        // Causes: C1 an existing directory; C2 an existing regular file; C3
        // an absent path. Effects: R1 native metadata flush succeeds on C1;
        // R2 a file is rejected as InvalidInput; R3 absence remains NotFound.
        // Compile-wiring cause/effect table (the original runtime R1-R3
        // assertions below remain the one flush kernel oracle):
        // F0 native off x any OS => flush API and its original dependency
        // contribution only; the conditional E0432 doc owns native API absence.
        // F1 native on x Linux/macOS => the original dirfd native tests.
        // F2 native on x Windows => original NTFS/ancestor/long-path tests;
        // F3 native on x other OS => original explicit Unsupported exact APIs,
        // never a feature-off runtime substitute. Source cfg union is not OS PASS.
        // F4 each original FS caller must request native in its dependency;
        // omission must be a compile error, not workspace feature luck. Original
        // standalone SDK filesystem restart and real caller builds own that row.
        // F5 a shared build containing any F4 caller enables the union for this
        // package; isolated default/native normal-build graphs own the compile
        // cost, and the authored workspace test profile must remain unchanged.
        // Portability's same required-case executor must run the original native
        // cases with this capability selected; a zero-case run cannot qualify.
        // The owning adapter, not this leaf, decides which tree is authorized.
        let temp = tempfile::tempdir().unwrap();
        assert!(sync_directory(temp.path()).is_ok(), "R1");
        let file = temp.path().join("file");
        std::fs::write(&file, b"data").unwrap();
        assert_eq!(
            sync_directory(&file).unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
            "R2"
        );
        assert_eq!(
            sync_directory(&temp.path().join("absent"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound,
            "R3"
        );
    }
}
