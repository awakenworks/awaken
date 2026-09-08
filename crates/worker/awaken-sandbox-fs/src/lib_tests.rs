use super::*;

struct OwnedDirectory(std::path::PathBuf);

impl Drop for OwnedDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn atomic_directory_publication_decision_table_is_total() {
    // Cause/effect table: C1 stage is an absolute directory, C2 destination
    // is an absolute sibling, C3 destination is absent/occupied. R1
    // C1+C2+absent moves exactly the stage name; R2 C1+C2+occupied rejects
    // while preserving both trees; R3 !C1 or !C2 rejects before the syscall.
    // There is deliberately no delete, replacement, or Repository policy.
    let root = std::env::temp_dir().join(format!(
        "awaken-sandbox-fs-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    let _owned = OwnedDirectory(root.clone());
    let stage = root.join("stage");
    let destination = root.join("destination");
    std::fs::create_dir(&stage).unwrap();
    std::fs::write(stage.join("STAGED"), b"stage").unwrap();
    std::fs::create_dir(&destination).unwrap();
    std::fs::write(destination.join("PRESERVED"), b"destination").unwrap();

    assert!(
        publish_directory_noreplace(&stage, &destination).is_err(),
        "R2"
    );
    assert_eq!(std::fs::read(stage.join("STAGED")).unwrap(), b"stage");
    assert_eq!(
        std::fs::read(destination.join("PRESERVED")).unwrap(),
        b"destination"
    );
    std::fs::remove_file(destination.join("PRESERVED")).unwrap();
    std::fs::remove_dir(&destination).unwrap();
    publish_directory_noreplace(&stage, &destination).expect("R1");
    assert!(!stage.exists());
    assert_eq!(std::fs::read(destination.join("STAGED")).unwrap(), b"stage");

    let file = root.join("file");
    std::fs::write(&file, b"file").unwrap();
    assert!(
        publish_directory_noreplace(&file, &root.join("other")).is_err(),
        "R3"
    );
    assert!(
        publish_directory_noreplace(Path::new("relative"), Path::new("other")).is_err(),
        "R3"
    );
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn exclusive_file_lock_is_one_inode_one_writer_and_drop_releases() {
    // Cause/effect table: C1 path is absent/regular/symlink, C2 the exact
    // inode is unlocked/locked, C3 an unrelated child is absent/between fork
    // and exec with an inherited descriptor. R1 absent or regular+unlocked
    // acquires one stable inode; R2 exact locked returns WouldBlock without
    // replacing it; R3 dropping the owner admits the next caller even for
    // C3=between-fork-and-exec; R4 symlink rejects without locking its target.
    let root = std::env::temp_dir().join(format!(
        "awaken-sandbox-lock-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    let _owned = OwnedDirectory(root.clone());
    let path = root.join("realization.lock");

    let first = try_lock_exclusive(&path).expect("R1");
    let blocked = try_lock_exclusive(&path).expect_err("R2");
    assert_eq!(blocked.kind(), std::io::ErrorKind::WouldBlock, "R2");

    // `try_clone` duplicates the same open-file description, exactly as fork
    // does before CLOEXEC. Keeping it alive proves Drop performs LOCK_UN rather
    // than relying on the last inherited descriptor eventually closing.
    let inherited = first._file.try_clone().unwrap();
    drop(first);
    let _second = try_lock_exclusive(&path).expect("R3");
    drop(inherited);

    let target = root.join("target");
    std::fs::write(&target, b"target").unwrap();
    let alias = root.join("alias");
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    let alias_error = try_lock_exclusive(&alias).expect_err("R4");
    assert_eq!(
        alias_error.raw_os_error(),
        Some(rustix::io::Errno::LOOP.raw_os_error()),
        "R4"
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"target", "R4");
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sibling_file_transform_streams_into_the_same_atomic_replace_boundary() {
    use std::io::{Read as _, Write as _};

    // Causes: C1 a transform streams successfully; C2 it fails after writing a
    // partial stage. Effects: E1 C1 atomically publishes only the complete new
    // file; E2 C2 preserves the old file and removes its private stage.
    // Decision table: R1=C1->E1; R2=C2->E2. Both the byte convenience method and
    // large-log migration reuse this single inode-check/publication authority.
    let root = std::env::temp_dir().join(format!(
        "awaken-sandbox-transform-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    let _owned = OwnedDirectory(root.clone());
    let lock = try_lock_exclusive(&root.join("lifecycle.lock")).unwrap();
    let data = root.join("data");
    std::fs::write(&data, b"alpha\nbeta\n").unwrap();

    lock.transform_sibling_regular_file_atomic(Path::new("data"), |source, stage| {
        let mut buffer = [0_u8; 3];
        loop {
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            buffer[..read].make_ascii_uppercase();
            stage.write_all(&buffer[..read])?;
        }
        Ok(())
    })
    .expect("R1/E1");
    assert_eq!(std::fs::read(&data).unwrap(), b"ALPHA\nBETA\n", "R1/E1");

    let error = lock
        .transform_sibling_regular_file_atomic(Path::new("data"), |_, stage| {
            stage.write_all(b"partial")?;
            Err(std::io::Error::other("injected transform failure"))
        })
        .expect_err("R2/E2");
    assert_eq!(error.kind(), std::io::ErrorKind::Other);
    assert_eq!(std::fs::read(&data).unwrap(), b"ALPHA\nBETA\n", "R2/E2");
    assert!(
        lock.sibling_names()
            .unwrap()
            .iter()
            .all(|name| !name.to_string_lossy().contains("awaken-file-stage")),
        "R2/E2"
    );
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn retained_parent_sibling_effects_fail_closed_after_parent_replacement() {
    // Retained-parent cause/effect table: C1 configured parent still names
    // the retained inode/replaced inode; C2 sibling operation is classify,
    // publish, or directory create. P1 exact parent permits descriptor-
    // relative effects; P2 replacement makes every C2 row reject before
    // mutation, preserving both the renamed owned directory and the foreign
    // replacement. This is the lock-domain invariant used by the marker.
    let base = std::env::temp_dir().join(format!(
        "awaken-sandbox-retained-parent-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&base).unwrap();
    let _owned = OwnedDirectory(base.clone());
    let parent = base.join("parent");
    let renamed = base.join("renamed-owned-parent");
    std::fs::create_dir(&parent).unwrap();
    let lock = try_lock_exclusive(&parent.join("lifecycle.lock")).expect("P1");
    assert_eq!(
        lock.classify_sibling_nofollow(Path::new("marker")).unwrap(),
        PathEntry::Absent,
        "P1"
    );

    std::fs::rename(&parent, &renamed).unwrap();
    std::fs::create_dir(&parent).unwrap();
    std::fs::write(parent.join("FOREIGN"), b"preserve").unwrap();
    assert!(
        lock.classify_sibling_nofollow(Path::new("marker")).is_err(),
        "P2 classify"
    );
    assert!(
        lock.publish_sibling_file_noreplace(Path::new("marker"), b"owned")
            .is_err(),
        "P2 publish"
    );
    assert!(
        lock.create_sibling_directory_noreplace(Path::new("root"))
            .is_err(),
        "P2 create"
    );
    assert_eq!(
        std::fs::read(parent.join("FOREIGN")).unwrap(),
        b"preserve",
        "P2"
    );
    assert!(!parent.join("marker").exists(), "P2 replacement untouched");
    assert!(
        !renamed.join("marker").exists(),
        "P2 retained inode untouched"
    );
    assert!(
        !renamed.join("root").exists(),
        "P2 retained inode untouched"
    );
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn atomic_file_publication_decision_table_is_total() {
    // Cause/effect table: C1 destination is absent/occupied, C2 one/two
    // publishers race, C3 bytes are empty/non-empty. R1 absent publishes
    // exactly one complete byte sequence; R2 occupied returns AlreadyExists
    // and preserves the winner; R3 a race has exactly one winner and no
    // partial destination. Empty bytes are valid infrastructure data; their
    // domain interpretation remains the caller's responsibility.
    let root = std::env::temp_dir().join(format!(
        "awaken-sandbox-file-publish-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    let _owned = OwnedDirectory(root.clone());
    let destination = root.join("identity");

    publish_file_noreplace(&destination, b"first").expect("R1");
    let occupied = publish_file_noreplace(&destination, b"second").expect_err("R2");
    assert_eq!(occupied.kind(), std::io::ErrorKind::AlreadyExists, "R2");
    assert_eq!(std::fs::read(&destination).unwrap(), b"first", "R2");

    let raced = root.join("raced");
    let one_path = raced.clone();
    let two_path = raced.clone();
    let one = std::thread::spawn(move || publish_file_noreplace(&one_path, b"one"));
    let two = std::thread::spawn(move || publish_file_noreplace(&two_path, b"two"));
    let outcomes = [one.join().unwrap(), two.join().unwrap()];
    assert_eq!(
        outcomes.iter().filter(|outcome| outcome.is_ok()).count(),
        1,
        "R3"
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| outcome
                .as_ref()
                .is_err_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists))
            .count(),
        1,
        "R3"
    );
    assert!(
        matches!(std::fs::read(&raced).unwrap().as_slice(), b"one" | b"two"),
        "R3"
    );
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn descriptor_relative_mutation_rejects_substituted_roots_and_symlinks() {
    // Cause/effect table: C1 root identity exact/foreign; C2 target is
    // regular/missing/symlink/hard-linked/directory tree; C3 operation is
    // write/shred/remove/directory metadata; C4 requested mode contains only
    // permission bits or also contains a file-type/unknown bit. R1
    // exact+regular+C4=permission-only writes complete
    // bytes, applies ordinary permission bits through the same dirfd walk,
    // then shreds the same inode; R2 missing shred/remove is idempotent; R3 a
    // symlink or hard-linked file rejects write/shred, and a foreign root
    // rejects every operation, with zero mutation of the aliased target; R4
    // exact-root remove unlinks a symlink name without following it and
    // recursively removes an exact directory tree; R5 C4=extra-bit rejects on
    // every supported platform before chmod rather than truncating the mode.
    let root = std::env::temp_dir().join(format!(
        "awaken-sandbox-relative-mutation-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    let _owned = OwnedDirectory(root.clone());
    let identity = directory_identity_nofollow(&root).unwrap();
    let secret = Path::new("nested/secret");

    assert!(portable_mode(0o7777).is_ok(), "R1 permission-bit ceiling");
    assert!(portable_mode(0o10000).is_err(), "R5 non-permission bit");

    create_relative_directory_all(&root, identity, Path::new("mode-dir")).unwrap();
    set_relative_directory_mode(&root, identity, Path::new("mode-dir"), 0o510)
        .expect("R1 metadata");
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(root.join("mode-dir"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o510,
            "R1 metadata"
        );
    }
    write_relative_file_atomic(&root, identity, secret, b"credential", 0o600).expect("R1 write");
    zero_relative_regular_file_nofollow(&root, identity, secret).expect("R1 shred");
    assert_eq!(std::fs::read(root.join(secret)).unwrap(), vec![0; 10], "R1");
    zero_relative_regular_file_nofollow(&root, identity, Path::new("nested/missing")).expect("R2");
    remove_relative_entry_exact(&root, identity, Path::new("nested/missing")).expect("R2");

    let outside = root.parent().unwrap().join(format!(
        "awaken-sandbox-outside-{}-{}",
        std::process::id(),
        PRIVATE_STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&outside, b"preserve").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("nested/alias")).unwrap();
    assert!(
        zero_relative_regular_file_nofollow(&root, identity, Path::new("nested/alias")).is_err(),
        "R3"
    );
    assert_eq!(std::fs::read(&outside).unwrap(), b"preserve", "R3");
    remove_relative_entry_exact(&root, identity, Path::new("nested/alias")).expect("R4 symlink");
    assert!(!root.join("nested/alias").exists(), "R4 symlink name");
    assert_eq!(std::fs::read(&outside).unwrap(), b"preserve", "R4 target");
    std::fs::remove_file(outside).unwrap();

    std::fs::create_dir_all(root.join("nested/tree/child")).unwrap();
    std::fs::write(root.join("nested/tree/child/file"), b"remove").unwrap();
    remove_relative_entry_exact(&root, identity, Path::new("nested/tree")).expect("R4 tree");
    assert!(!root.join("nested/tree").exists(), "R4 tree");

    let aliased = root.join("nested/aliased-secret");
    let alias = root.join("nested/aliased-secret-copy");
    std::fs::write(&aliased, b"preserve-alias").unwrap();
    std::fs::hard_link(&aliased, &alias).unwrap();
    assert!(
        zero_relative_regular_file_nofollow(&root, identity, Path::new("nested/aliased-secret"))
            .is_err(),
        "R3"
    );
    assert_eq!(std::fs::read(&alias).unwrap(), b"preserve-alias", "R3");

    assert!(
        write_relative_file_atomic(
            &root,
            DirectoryIdentity {
                device: identity.device,
                inode: identity.inode.saturating_add(1),
            },
            Path::new("foreign"),
            b"blocked",
            0o600,
        )
        .is_err(),
        "R3"
    );
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn descriptor_tree_read_propagates_every_non_missing_boundary_fault() {
    // Cause/effect decision table: C1 sandbox root exact/missing/foreign;
    // C2 requested subtree present/missing; C3 descendant regular/directory/
    // symlink; C4 canonical excluded prefix present/absent. R1
    // exact+present+regular recursively returns one sorted byte
    // snapshot; R2 missing root or subtree is explicitly empty; R3 foreign
    // root rejects before descent; R4 any symlink rejects the whole scan so
    // callers cannot reinterpret partial I/O as empty Artifacts/Skills/Memory;
    // R5 an excluded prefix is not opened while other files and empty
    // directory entries remain in the complete snapshot.
    let root = std::env::temp_dir().join(format!(
        "awaken-sandbox-tree-read-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    let _owned = OwnedDirectory(root.clone());
    std::fs::create_dir_all(root.join("tree/nested")).unwrap();
    std::fs::create_dir(root.join("tree/empty")).unwrap();
    std::fs::create_dir_all(root.join("tree/excluded/private")).unwrap();
    std::fs::write(root.join("tree/z.txt"), b"z").unwrap();
    std::fs::write(root.join("tree/nested/a.txt"), b"a").unwrap();
    std::fs::write(root.join("tree/excluded/private/secret"), b"secret").unwrap();
    let identity = directory_identity_nofollow(&root).unwrap();

    let files = read_regular_tree_nofollow(&root, identity, Path::new("tree")).unwrap();
    assert_eq!(
        files[0].relative_path,
        Path::new("excluded/private/secret"),
        "R1"
    );
    assert_eq!(files[1].relative_path, Path::new("nested/a.txt"), "R1");
    assert_eq!(files[1].bytes, b"a", "R1");
    assert_eq!(files[2].relative_path, Path::new("z.txt"), "R1");
    let snapshot = read_tree_nofollow_excluding(
        &root,
        identity,
        Path::new("tree"),
        &[PathBuf::from("excluded")],
    )
    .unwrap();
    assert!(
        snapshot
            .directories
            .iter()
            .any(|directory| directory.relative_path == Path::new("empty")),
        "R5"
    );
    assert!(
        snapshot
            .files
            .iter()
            .all(|file| !file.relative_path.starts_with("excluded")),
        "R5"
    );
    assert!(
        read_regular_tree_nofollow(&root, identity, Path::new("missing"))
            .unwrap()
            .is_empty(),
        "R2"
    );
    assert!(
        read_regular_tree_nofollow(&root.join("absent"), identity, Path::new("tree"))
            .unwrap()
            .is_empty(),
        "R2"
    );
    assert!(
        read_regular_tree_nofollow(
            &root,
            DirectoryIdentity {
                device: identity.device,
                inode: identity.inode.saturating_add(1),
            },
            Path::new("tree")
        )
        .is_err(),
        "R3"
    );

    std::os::unix::fs::symlink("z.txt", root.join("tree/alias")).unwrap();
    assert!(
        read_regular_tree_nofollow(&root, identity, Path::new("tree")).is_err(),
        "R4"
    );
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn bounded_tree_reader_reuses_exact_walk_and_enforces_the_supplied_limit() {
    // B1 file length = limit (including 0) => exact bytes and sorted snapshot;
    // B2 file length > limit => InvalidData, never a partial snapshot;
    // B3 same tree via existing unbounded API => unchanged compatibility;
    // B4 missing required root => NotFound, not a successful empty tree.
    let root = std::env::temp_dir().join(format!(
        "awaken-bounded-tree-{}-{}",
        std::process::id(),
        PRIVATE_STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&root).unwrap();
    let _owned = OwnedDirectory(root.clone());
    for limit in [0_usize, 16] {
        std::fs::write(root.join("a"), vec![b'a'; limit]).unwrap();
        let captured = read_tree_nofollow_bounded(&root, limit as u64, u64::MAX).unwrap();
        assert_eq!(captured.files.len(), 1, "B1");
        assert_eq!(captured.files[0].bytes, vec![b'a'; limit], "B1");
        std::fs::write(root.join("z"), vec![b'z'; limit + 1]).unwrap();
        assert_eq!(
            read_tree_nofollow_bounded(&root, limit as u64, u64::MAX)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidData,
            "B2"
        );
        assert_eq!(
            read_tree_nofollow(
                &root,
                directory_identity_nofollow(&root).unwrap(),
                Path::new("")
            )
            .unwrap()
            .files
            .iter()
            .map(|file| file.relative_path.as_path())
            .collect::<Vec<_>>(),
            vec![Path::new("a"), Path::new("z")],
            "B3"
        );
        std::fs::remove_file(root.join("z")).unwrap();
    }
    assert_eq!(
        read_tree_nofollow_bounded(&root.join("missing"), 16, 16)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotFound,
        "B4"
    );
    // B5 each file is within its limit, but the shared whole-tree budget must
    // include descendants. Exactly total bytes succeeds; total-1 fails closed.
    std::fs::create_dir(root.join("nested")).unwrap();
    std::fs::write(root.join("nested/b"), b"b").unwrap();
    assert_eq!(
        read_tree_nofollow_bounded(&root, 16, 17)
            .unwrap()
            .files
            .len(),
        2,
        "B5 exact"
    );
    assert_eq!(
        read_tree_nofollow_bounded(&root, 16, 16)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidData,
        "B5 over"
    );
}

#[test]
fn metadata_budgets_count_empty_entries_and_depth_before_descent() {
    // B6 empty file/empty directory each consumes one entry. B7 a nested entry
    // consumes depth even when its content is empty. Bound-1 rejects, exact
    // bound succeeds. B8 the old unbounded reader preserves its contract.
    // Unlike descriptor-only tests, B6-B8 run on Windows too: its quiescent-path
    // adapter must enforce exactly the same caller-owned budgets.
    let root = std::env::temp_dir().join(format!(
        "awaken-tree-limits-{}-{}",
        std::process::id(),
        PRIVATE_STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(root.join("d")).unwrap();
    let _owned = OwnedDirectory(root.clone());
    std::fs::write(root.join("a"), b"").unwrap();
    std::fs::write(root.join("d/b"), b"").unwrap();
    for (entries, depth, valid) in [(2, 2, false), (3, 1, false), (3, 2, true), (0, 0, false)] {
        let result = read_tree_nofollow_with_limits(
            &root,
            TreeReadLimits {
                file_bytes: 0,
                total_bytes: 0,
                entries,
                depth,
            },
        );
        assert_eq!(result.is_ok(), valid, "B6/B7 {entries}/{depth}");
        if let Err(error) = result {
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        }
    }
    assert_eq!(
        read_tree_nofollow_bounded(&root, 0, 0).unwrap().files.len(),
        2,
        "B8"
    );
}

#[test]
fn a_read_error_after_partial_bytes_never_returns_a_partial_file() {
    // IO1 some valid bytes then injected EIO => original error, no snapshot,
    // and no consumed budget reported as a successfully read file. This drives
    // the production read primitive, not an independently implemented reader.
    struct FailsAfterPrefix(bool);
    impl std::io::Read for FailsAfterPrefix {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.0 {
                return Err(std::io::Error::from_raw_os_error(5));
            }
            self.0 = true;
            buffer[0] = b'a';
            Ok(1)
        }
    }
    let mut remaining = 16;
    assert_eq!(
        read_tree_file_bounded(FailsAfterPrefix(false), 16, &mut remaining)
            .unwrap_err()
            .raw_os_error(),
        Some(5)
    );
    assert_eq!(remaining, 16, "IO1 rejected read does not publish progress");
}

#[test]
#[cfg(target_os = "linux")]
fn pinned_socket_path_is_short_nofollow_and_directory_exact() {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::net::{UnixListener, UnixStream};
    // Cause/effect table: S1 short/long parent x ASCII/non-UTF8 leaf -> the same
    // filesystem socket and real byte exchange; S2 parent renamed/replaced after
    // capture -> bind only in the retained inode; S3 missing/file/symlink parent,
    // no leaf, NUL or overlong address -> reject with no publication. The kernel
    // and procfs are external TCB; these are executed Linux adapter checks.
    let root = std::env::temp_dir().join(format!(
        "awaken-socket-{}-{}",
        std::process::id(),
        PRIVATE_STAGE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&root).unwrap();
    let _owned = OwnedDirectory(root.clone());
    for length in [1, 120] {
        let parent = root.join("p".repeat(length));
        std::fs::create_dir(&parent).unwrap();
        for leaf in [
            std::ffi::OsStr::new("control.sock"),
            std::ffi::OsStr::from_bytes(b"socket-\xff"),
        ] {
            let socket = parent.join(leaf);
            if length == 120 {
                assert!(
                    UnixListener::bind(&socket).is_err(),
                    "S1 raw address exceeds sun_path"
                );
            }
            let address = PinnedUnixSocketPath::capture(&socket).unwrap();
            let listener = UnixListener::bind(address.path()).unwrap();
            assert!(socket.exists(), "S1 exact canonical pathname");
            let mut client = UnixStream::connect(address.path()).unwrap();
            let (mut server, _) = listener.accept().unwrap();
            client.write_all(b"socket-proof").unwrap();
            let mut bytes = [0; 12];
            server.read_exact(&mut bytes).unwrap();
            assert_eq!(&bytes, b"socket-proof", "S1 real byte exchange");
            std::fs::remove_file(&socket).unwrap();
        }
    }
    let parent = root.join("retained");
    let moved = root.join("moved");
    std::fs::create_dir(&parent).unwrap();
    let address = PinnedUnixSocketPath::capture(&parent.join("control.sock")).unwrap();
    std::fs::rename(&parent, &moved).unwrap();
    std::fs::create_dir(&parent).unwrap();
    let _listener = UnixListener::bind(address.path()).unwrap();
    assert!(moved.join("control.sock").exists(), "S2 retained inode");
    assert!(
        !parent.join("control.sock").exists(),
        "S2 replacement untouched"
    );

    let file = root.join("file");
    std::fs::write(&file, b"unchanged").unwrap();
    let link = root.join("link");
    std::os::unix::fs::symlink(&parent, &link).unwrap();
    for invalid in [
        root.join("missing/control.sock"),
        file.join("control.sock"),
        link.join("control.sock"),
        PathBuf::from("/"),
        parent.join(std::ffi::OsStr::from_bytes(b"bad\0name")),
        parent.join("s".repeat(108)),
    ] {
        assert!(
            PinnedUnixSocketPath::capture(&invalid).is_err(),
            "S3 {}",
            invalid.display()
        );
    }
    assert_eq!(
        std::fs::read(file).unwrap(),
        b"unchanged",
        "S3 no side effects"
    );
}
