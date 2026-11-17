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
    #[cfg(test)]
    tests::record_flush(path);
    directory.sync_all()
}

/// Create `path` and every missing ancestor, durably, then flush `path`.
///
/// A new directory exists only as an entry in its parent, so each directory
/// this call creates is followed by a flush of that parent: a crash after the
/// call cannot lose a directory, nor anything later made durable inside it,
/// that the call reported as present. `std::fs::create_dir_all` followed by
/// [`sync_directory`] of the leaf flushes only the leaf's own entries. A
/// component that already exists is not re-created; one another process is
/// creating at the same moment is treated as created here and its parent is
/// flushed too, so neither caller returns before that entry is durable. An
/// existing non-directory at `path` is refused by the final flush.
pub fn create_directory_all(path: &Path) -> io::Result<()> {
    let created = match std::fs::create_dir(path) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            else {
                return Err(error);
            };
            create_directory_all(parent)?;
            match std::fs::create_dir(path) {
                Ok(()) => true,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => true,
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    };
    if created {
        match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => sync_directory(parent)?,
            _ => sync_directory(Path::new("."))?,
        }
    }
    sync_directory(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        static FLUSHED: std::cell::RefCell<Vec<std::path::PathBuf>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    pub(super) fn record_flush(path: &Path) {
        FLUSHED.with(|flushed| flushed.borrow_mut().push(path.to_path_buf()));
    }

    fn take_flushed() -> Vec<std::path::PathBuf> {
        FLUSHED.with(|flushed| std::mem::take(&mut *flushed.borrow_mut()))
    }

    #[test]
    fn creating_a_directory_tree_flushes_the_parent_of_every_created_directory() {
        // Causes: C1 how many trailing components are missing (0, 1, 2); C2 the
        // leaf is an existing regular file. Effects: E1 every missing component
        // exists afterwards; E2 the parent of each created component is flushed
        // after its creation, outermost first, then the leaf; E3 an existing
        // leaf flushes only itself; E4 a regular file is refused and nothing is
        // created. `create_dir_all` + leaf flush satisfies E1/E3/E4 but flushes
        // no parent, so a crash can drop the whole new tree.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let middle = root.join("a");
        let leaf = middle.join("b");

        let _ = take_flushed();
        create_directory_all(&leaf).unwrap();
        assert!(leaf.is_dir(), "C1=2 E1");
        assert_eq!(
            take_flushed(),
            [root.clone(), middle.clone(), middle.clone(), leaf.clone()],
            "C1=2 E2"
        );

        let sibling = middle.join("c");
        create_directory_all(&sibling).unwrap();
        assert!(sibling.is_dir(), "C1=1 E1");
        assert_eq!(take_flushed(), [middle.clone(), sibling], "C1=1 E2");

        create_directory_all(&leaf).unwrap();
        assert_eq!(take_flushed(), [leaf], "C1=0 E3");

        let file = root.join("file");
        std::fs::write(&file, b"data").unwrap();
        assert_eq!(
            create_directory_all(&file).unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
            "C2 E4"
        );
        assert!(
            create_directory_all(&file.join("child")).is_err(),
            "C2 E4 below a file"
        );
        assert!(file.is_file(), "C2 E4");
        assert!(take_flushed().is_empty(), "C2 E4 nothing flushed");
    }

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
