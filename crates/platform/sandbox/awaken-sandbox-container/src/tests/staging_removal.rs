//! Removing host staging that a read-only Memory copy closed to its owner.

use super::*;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(unix)]
/// A link target outside the staging root, with the mode and bytes it must
/// keep however the staging tree is cleaned up.
struct OutsideTargets {
    _root: tempfile::TempDir,
    directory: std::path::PathBuf,
    file: std::path::PathBuf,
}

#[cfg(unix)]
impl OutsideTargets {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("outside-dir");
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("kept.txt"), b"outside").unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o510)).unwrap();
        let file = root.path().join("outside-file");
        std::fs::write(&file, b"outside").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o400)).unwrap();
        Self {
            _root: root,
            directory,
            file,
        }
    }

    fn modes(&self) -> (u32, u32) {
        let mode = |path: &std::path::Path| {
            std::fs::symlink_metadata(path)
                .unwrap()
                .permissions()
                .mode()
                & 0o7777
        };
        (mode(&self.directory), mode(&self.file))
    }

    fn assert_untouched(&self, rule: &str) {
        let (directory, file) = self.modes();
        assert_eq!(
            format!("{directory:o} {file:o}"),
            "510 400",
            "{rule}: link target modes"
        );
        assert_eq!(std::fs::read(&self.file).unwrap(), b"outside", "{rule}");
        assert!(self.directory.join("kept.txt").exists(), "{rule}");
    }
}

#[cfg(unix)]
impl Drop for OutsideTargets {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.directory, std::fs::Permissions::from_mode(0o700));
    }
}

#[cfg(unix)]
/// Lay out the read-only Memory copy `make_memory_tree_accessible` leaves
/// behind (`0o555` directories, `0o444` files, nested), plus links out of the
/// staging root, inside a fresh provider staging root.
fn closed_memory_copy(scope: &str, outside: &OutsideTargets) -> Option<StagingGuard> {
    let mut guard = None;
    let root = staging_dir(&mut guard, scope).unwrap();
    let memory = root.join("memory-_mnt_memory_archive");
    let nested = memory.join("nested").join("deeper");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(memory.join("seed.txt"), b"seed").unwrap();
    std::fs::write(nested.join("note.md"), b"note").unwrap();
    std::os::unix::fs::symlink(&outside.directory, memory.join("link-dir")).unwrap();
    std::os::unix::fs::symlink(&outside.file, nested.join("link-file")).unwrap();
    for file in [memory.join("seed.txt"), nested.join("note.md")] {
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o444)).unwrap();
    }
    for directory in [nested.clone(), memory.join("nested"), memory] {
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o555)).unwrap();
    }
    guard
}

#[cfg(unix)]
/* Read-only staging removal table. C1 the staging root holds a read-only
 * Memory copy: nested 0o555 directories and 0o444 files; C2 the copy holds
 * links to a directory and a file outside the staging root, whose modes are
 * recorded; C3 the removal edge: R1 compensation drop of an uncommitted guard,
 * R2 explicit release (`StagingGuard::remove`, the dispose path), R3 terminal
 * removal of a restored host-bind root (`remove_host_staging_path`).
 * E1 the staging root is gone; E2 the link targets keep their modes and bytes.
 * Every rule is C1+C2+C3 => E1+E2. */
#[test]
fn read_only_memory_staging_is_removed_without_touching_link_targets() {
    let outside = OutsideTargets::new();

    let guard = closed_memory_copy("ro-staging-drop", &outside);
    let root = guard.as_ref().unwrap().path().to_path_buf();
    drop(guard);
    assert!(!root.exists(), "R1/E1: compensation drop removes the copy");
    outside.assert_untouched("R1/E2");

    let mut guard = closed_memory_copy("ro-staging-release", &outside).unwrap();
    guard.retain_for_restoration();
    let root = guard.path().to_path_buf();
    guard
        .remove()
        .expect("R2: explicit release reports success");
    assert!(!root.exists(), "R2/E1: explicit release removes the copy");
    drop(guard);
    outside.assert_untouched("R2/E2");

    let mut guard = closed_memory_copy("ro-staging-restored", &outside).unwrap();
    guard.retain_for_restoration();
    let root = guard.path().to_path_buf();
    drop(guard);
    remove_host_staging_path(&root).expect("R3: terminal removal reports success");
    assert!(!root.exists(), "R3/E1: terminal removal removes the copy");
    outside.assert_untouched("R3/E2");
}

#[cfg(unix)]
/* Honest-failure table. C1 one directory in the tree cannot be opened by its
 * owner (mode 0o000), so its identity cannot be checked and its mode is not
 * changed through a path; C2 a sibling directory is an ordinary 0o555 copy.
 * E1 removal reports an error naming the unopenable entry; E2 every other
 * entry was still restored and removed; E3 a retry after the owner reopens the
 * entry removes the rest and succeeds. */
#[test]
fn an_unrestorable_staging_entry_is_reported_and_the_rest_is_still_removed() {
    let mut guard = None;
    let root = staging_dir(&mut guard, "ro-staging-unrestorable").unwrap();
    let sealed = root.join("sealed");
    let closed = root.join("closed");
    std::fs::create_dir_all(closed.join("inner")).unwrap();
    std::fs::create_dir(&sealed).unwrap();
    std::fs::write(closed.join("inner").join("seed.txt"), b"seed").unwrap();
    for directory in [closed.join("inner"), closed.clone()] {
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o555)).unwrap();
    }
    std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o000)).unwrap();
    let mut guard = guard.unwrap();
    guard.retain_for_restoration();

    let error = guard
        .remove()
        .expect_err("E1: the unopenable entry is reported");
    assert!(
        error.to_string().contains(&sealed.display().to_string()),
        "E1: {error}"
    );
    assert!(!closed.exists(), "E2: the restorable sibling is removed");
    assert!(sealed.exists(), "E1: the unrestorable entry is not forced");

    std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o700)).unwrap();
    guard
        .remove()
        .expect("E3: a retried release removes the rest");
    assert!(!root.exists(), "E3");
}

#[cfg(unix)]
/* Projection table. C1 a Memory copy holds a link to a directory and one to a
 * file outside the staging root; C2 the copy is projected read-only. E1 the
 * copy's own directories and files are closed (0o555 / 0o444); E2 the link
 * targets keep their modes; E3 the projected copy is then removed. */
#[test]
fn read_only_memory_projection_never_changes_a_link_target() {
    let outside = OutsideTargets::new();
    let mut guard = None;
    let root = staging_dir(&mut guard, "ro-staging-projection").unwrap();
    let memory = root.join("memory-_mnt_memory_archive");
    std::fs::create_dir_all(memory.join("nested")).unwrap();
    std::fs::write(memory.join("nested").join("seed.txt"), b"seed").unwrap();
    std::os::unix::fs::symlink(&outside.directory, memory.join("link-dir")).unwrap();
    std::os::unix::fs::symlink(&outside.file, memory.join("nested").join("link-file")).unwrap();

    make_memory_tree_accessible(&memory, pc::MountAccess::ReadOnly).unwrap();

    let mode = |path: std::path::PathBuf| {
        std::fs::symlink_metadata(path)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    };
    assert_eq!(mode(memory.join("nested")), 0o555, "E1");
    assert_eq!(mode(memory.join("nested").join("seed.txt")), 0o444, "E1");
    outside.assert_untouched("E2");
    let root = guard.as_ref().unwrap().path().to_path_buf();
    drop(guard);
    assert!(!root.exists(), "E3");
    outside.assert_untouched("E3");
}

#[tokio::test]
async fn memory_store_realizes_as_copy_on_the_container_tier() {
    // Cause/effect graph: C1 a Memory requirement names an exact protocol path;
    // C2 access is ReadWrite or ReadOnly. E1 the container binds the canonical
    // mounter copy at that exact path, without `/workspace` or hidden `.mnt`;
    // E2 the runtime bind's read-only flag exactly projects C2; E3 disposal
    // removes the whole staging root, the read-only copy included.
    //
    // | Rule | access | bind flag | effects  |
    // | M1   | RW     | false     | E1,E2,E3 |
    // | M2   | RO     | true      | E1,E2,E3 |
    // Constraint: the container runtime enforces access; the Memory mounter is
    // the one content/lifecycle owner and must not be reimplemented here.
    let rt = Arc::new(FakeRuntime::default());
    let mut s = spec("mem-real");
    s.mounts.push(pc::MountRequirement {
        mount_id: "notes".into(),
        source: pc::MountSource::MemoryStore {
            store_id: "store-7".into(),
            materialization_reference: None,
            write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
        },
        mount_path: "/mnt/memory/notes".into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::Session,
        required: true,
    });
    s.mounts.push(pc::MountRequirement {
        mount_id: "archive".into(),
        source: pc::MountSource::MemoryStore {
            store_id: "store-8".into(),
            materialization_reference: None,
            write_consistency: pc::MemoryWriteConsistency::ProviderDefault,
        },
        mount_path: "/mnt/memory/archive".into(),
        access: pc::MountAccess::ReadOnly,
        lifetime: pc::MountLifetime::Session,
        required: true,
    });
    let p = provider(rt);
    let torn_down = Arc::new(AtomicBool::new(false));
    p.install_memory_mounter(Arc::new(FakeMemoryMounter {
        torn_down: torn_down.clone(),
        fail_teardown: false,
    }));
    let sandbox = p.create(&s).await.unwrap();
    let mem = sandbox
        .realized()
        .iter()
        .find(|r| r.mount_id == "notes")
        .expect("memory mount realized");
    // Portable default: the container tier reports the canonical mounter's Copy,
    // not a second runtime-local store.
    assert_eq!(mem.realization, pc::Realization::Copy);
    let staging_roots = {
        let state = p.runtime.st.lock().unwrap();
        let bind = state.created_binds["cid-mem-real"]
            .iter()
            .find(|bind| bind.mount_path == "/mnt/memory/notes")
            .expect("portable memory copy is bound into the container");
        assert!(!bind.read_only);
        assert_eq!(
            std::fs::read(std::path::Path::new(&bind.source_ref).join("seed.txt")).unwrap(),
            b"seed"
        );
        let read_only = state.created_binds["cid-mem-real"]
            .iter()
            .find(|bind| bind.mount_path == "/mnt/memory/archive")
            .expect("M2 read-only memory copy is bound into the container");
        assert!(read_only.read_only, "M2/E2");
        [bind, read_only].map(|bind| {
            std::path::Path::new(&bind.source_ref)
                .parent()
                .expect("Memory copy lives under its staging root")
                .to_path_buf()
        })
    };
    sandbox.dispose_unattached().await.unwrap();
    assert!(torn_down.load(Ordering::SeqCst));
    for root in staging_roots {
        assert!(
            !root.exists(),
            "M1/M2 E3: {} survived disposal",
            root.display()
        );
    }
}
