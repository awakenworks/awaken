//! Durable integration tests for the canonical copy-only `MemoryMounter`.
//! This replaces the retired `awaken-sandbox memoryd` executable test so there is
//! one production realization path and one matching lifecycle test owner.

use std::path::PathBuf;
use std::sync::Arc;

use awaken_memory_store::{MemoryRepository, SqliteMemoryRepository};
use awaken_provisioning_contract::MemoryMaterializationEvidence;
use awaken_provisioning_contract::{MemoryMounter, MountAccess};
use awaken_sandbox_memoryd::MemoryStoreMounter;

fn temporary_root() -> PathBuf {
    tempfile::Builder::new()
        .prefix("awaken-memory-mounter-copy-")
        .tempdir()
        .expect("exclusive temporary root")
        .keep()
}

#[tokio::test]
async fn ingress_and_recovery_reject_a_tree_that_cannot_be_harvested() {
    // B11 a logically valid head exceeds physical depth. Initial projection and
    // recovered-file admission must BOTH reject, with no partial initial files or
    // Repository mutation. Tests the production call paths, not just the budget
    // helper, so removing either admission call cannot leave this row green.
    let root = tempfile::tempdir().unwrap();
    let repository = Arc::new(
        SqliteMemoryRepository::open(root.path().join("memory.sqlite").to_str().unwrap()).unwrap(),
    );
    repository.create("s", "/a.md", "valid").await.unwrap();
    let relative = std::iter::repeat_n("d", 129).collect::<Vec<_>>().join("/");
    repository
        .create("s", &format!("/{relative}"), "deep")
        .await
        .unwrap();
    let heads = repository.snapshot_heads("s").await.unwrap();
    let history = repository.list_versions("s").await.unwrap();
    let projection = root.path().join("projection");
    let mounter = MemoryStoreMounter::copy_only(repository.clone());
    assert!(
        mounter
            .mount("s", &projection, MountAccess::ReadWrite)
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_dir(&projection).unwrap().count(), 0);
    let evidence = MemoryMaterializationEvidence::new("s", "/workspace/memory", vec![]).unwrap();
    let error = mounter
        .reconcile_recovered_copy(
            "s",
            &evidence,
            &[(relative, b"unharvestable".to_vec())],
            MountAccess::ReadWrite,
        )
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Memory projection exceeds copy budget"),
        "B11 fail at ingress, not at the later scan: {error}"
    );
    assert_eq!(repository.snapshot_heads("s").await.unwrap(), heads);
    assert_eq!(repository.list_versions("s").await.unwrap(), history);
}

#[tokio::test]
async fn materialization_rejects_nonportable_store_paths_before_any_file_write() {
    // I1 logical Repository accepts Windows-special syntax, but every physical
    // projection rejects it BEFORE writing even a preceding valid head. I2 the
    // recovery entry uses the same validator. Both preserve outside bytes and
    // exact durable heads/history; this runs on Linux too, not a cfg-only test.
    for invalid in [
        "/..\\outside.txt",
        "/C:\\outside.txt",
        "/\\\\server\\share",
        "/note:stream",
        "/NUL.txt",
        "/COM1",
        "/LPT9.log",
        "/trailing.",
        "/trailing ",
    ] {
        let root = temporary_root();
        let repository = Arc::new(
            SqliteMemoryRepository::open(root.join("memory.sqlite").to_str().unwrap()).unwrap(),
        );
        repository.create("s", "/a.md", "valid").await.unwrap();
        repository
            .create("s", invalid, "must not escape")
            .await
            .unwrap();
        let heads = repository.snapshot_heads("s").await.unwrap();
        let history = repository.list_versions("s").await.unwrap();
        std::fs::write(root.join("outside.txt"), "sentinel").unwrap();
        let projection = root.join("projection");
        std::fs::create_dir(&projection).unwrap();
        assert!(
            awaken_sandbox_memoryd::copy::materialize(&*repository, "s", &projection)
                .await
                .is_err(),
            "I1 {invalid}"
        );
        assert_eq!(
            std::fs::read_dir(&projection).unwrap().count(),
            0,
            "I1 no partial projection"
        );
        let mounter = MemoryStoreMounter::copy_only(repository.clone());
        let evidence =
            MemoryMaterializationEvidence::new("s", "/workspace/memory", vec![]).unwrap();
        assert!(
            mounter
                .reconcile_recovered_copy(
                    "s",
                    &evidence,
                    &[(invalid.trim_start_matches('/').into(), b"bad".to_vec())],
                    MountAccess::ReadWrite
                )
                .await
                .is_err(),
            "I2"
        );
        assert_eq!(repository.snapshot_heads("s").await.unwrap(), heads);
        assert_eq!(repository.list_versions("s").await.unwrap(), history);
        assert_eq!(
            std::fs::read_to_string(root.join("outside.txt")).unwrap(),
            "sentinel"
        );
        drop(mounter);
        drop(repository);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn materialization_never_silently_overwrites_filesystem_aliases() {
    // A1 case-sensitive filesystem => two exact paths and bytes. A2 the host
    // aliases those names => exclusive creation rejects; the first file is not
    // overwritten and Repository truth is untouched. A3 occupied final (e.g.
    // an alias or leftover projection) => reject rather than truncate.
    let root = temporary_root();
    let probe = root.join("probe");
    std::fs::create_dir(&probe).unwrap();
    std::fs::write(probe.join("A"), "probe").unwrap();
    let insensitive = probe.join("a").exists();
    let repository = Arc::new(
        SqliteMemoryRepository::open(root.join("memory.sqlite").to_str().unwrap()).unwrap(),
    );
    repository.create("s", "/A.md", "upper").await.unwrap();
    repository.create("s", "/a.md", "lower").await.unwrap();
    let heads = repository.snapshot_heads("s").await.unwrap();
    let projection = root.join("projection");
    std::fs::create_dir(&projection).unwrap();
    let result = awaken_sandbox_memoryd::copy::materialize(&*repository, "s", &projection).await;
    assert_eq!(
        result.is_err(),
        insensitive,
        "A1/A2 actual filesystem semantics"
    );
    assert_eq!(
        std::fs::read_to_string(projection.join("A.md")).unwrap(),
        "upper"
    );
    if !insensitive {
        assert_eq!(
            std::fs::read_to_string(projection.join("a.md")).unwrap(),
            "lower"
        );
    }
    assert!(
        awaken_sandbox_memoryd::copy::materialize(&*repository, "s", &projection)
            .await
            .is_err(),
        "A3"
    );
    assert_eq!(repository.snapshot_heads("s").await.unwrap(), heads);
    drop(repository);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn parent_alias_admission_preserves_exact_no_edit_heads_and_history() {
    // A4 distinct parent spellings + FS alias => reject before file writes.
    // A5 exact-preserving FS => mount/no-edit teardown preserves paths, IDs,
    // hashes and history. A6 a shared EXACT parent is allowed. A7 normalization
    // changes a name even without collision => fail closed, never implicit rename.
    // Probe actual volume semantics, not target_os (macOS/Windows can be sensitive).
    for (left, right) in [
        ("A", "a"),
        ("é", "e\u{301}"),
        ("é", "é"),
        ("shared", "shared"),
    ] {
        for nested in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let probe = root.path().join("probe");
            std::fs::create_dir(&probe).unwrap();
            std::fs::create_dir(probe.join(left)).unwrap();
            let aliases = left != right && probe.join(right).exists();
            if !probe.join(right).exists() {
                std::fs::create_dir(probe.join(right)).unwrap();
            }
            let observed: std::collections::HashSet<_> = std::fs::read_dir(&probe)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            let exact = observed.contains(std::ffi::OsStr::new(left))
                && observed.contains(std::ffi::OsStr::new(right));
            let repository = Arc::new(
                SqliteMemoryRepository::open(root.path().join("memory.sqlite").to_str().unwrap())
                    .unwrap(),
            );
            let prefix = if nested { "/outer" } else { "" };
            repository
                .create("s", &format!("{prefix}/{left}/x.md"), "left")
                .await
                .unwrap();
            repository
                .create("s", &format!("{prefix}/{right}/y.md"), "right")
                .await
                .unwrap();
            let heads = repository.snapshot_heads("s").await.unwrap();
            let history = repository.list_versions("s").await.unwrap();
            let mounter = MemoryStoreMounter::copy_only(repository.clone());
            let result = mounter
                .mount("s", &root.path().join("projection"), MountAccess::ReadWrite)
                .await;
            assert_eq!(result.is_ok(), !aliases && exact, "{left}/{right}/{nested}");
            if let Ok(mount) = result {
                mount.teardown().await.unwrap();
                mount.teardown().await.unwrap();
            }
            assert_eq!(repository.snapshot_heads("s").await.unwrap(), heads);
            assert_eq!(repository.list_versions("s").await.unwrap(), history);
        }
    }
}

#[tokio::test]
async fn recovery_preserves_logical_names_without_host_rematerialization() {
    // R1 captured paths may alias on this host but are distinct logical names:
    // reconcile directly, retaining identity; input order cannot rename heads.
    let root = tempfile::tempdir().unwrap();
    let repository = Arc::new(
        SqliteMemoryRepository::open(root.path().join("memory.sqlite").to_str().unwrap()).unwrap(),
    );
    let mut files = vec![];
    for path in ["A/x.md", "a/y.md", "é/x.md", "e\u{301}/y.md"] {
        repository
            .create("s", &format!("/{path}"), "base")
            .await
            .unwrap();
        files.push((path.to_owned(), b"base".to_vec()));
    }
    let heads = repository.snapshot_heads("s").await.unwrap();
    let history = repository.list_versions("s").await.unwrap();
    let evidence = MemoryMaterializationEvidence::new(
        "s",
        "/workspace/memory",
        heads
            .iter()
            .map(
                |head| awaken_provisioning_contract::MemoryMaterializationHead {
                    path: head.path.clone(),
                    id: head.id.clone(),
                    content_sha256: head.content_sha256.clone(),
                },
            )
            .collect(),
    )
    .unwrap();
    files.reverse();
    MemoryStoreMounter::copy_only(repository.clone())
        .reconcile_recovered_copy("s", &evidence, &files, MountAccess::ReadWrite)
        .await
        .unwrap();
    assert_eq!(repository.snapshot_heads("s").await.unwrap(), heads);
    assert_eq!(repository.list_versions("s").await.unwrap(), history);
}

#[tokio::test]
async fn unrepresentable_copy_blocks_writeback_until_source_is_repaired() {
    // Cause/effect table U2-U5 (full Cartesian product, 16 rows):
    // C1 live/recovered; C2 binary creates/replaces a head; C3 binary sorts
    // before/after valid edits; C4 read-write/read-only. Every row also includes
    // a valid create, update and local deletion.
    // U2 RW + binary => error on every retry, NO partial CAS/delete/history or
    // snapshot advance; exact source bytes and frozen bases survive.
    // U3 RO + binary => success WITHOUT writeback; this is not a preservation
    // receipt for unauthorized edits. U4 RW + repaired UTF-8 => four effects
    // commit once; replay with original recovery bases adds no history.
    // U5 close/reopen SQLite => exact heads/history, not merely cached success.
    // Constraint: captured bytes are quiescent; OS fencing is outside this test.
    for recovered in [false, true] {
        for existing in [false, true] {
            for binary_path in ["0.bin", "z.bin"] {
                for access in [MountAccess::ReadWrite, MountAccess::ReadOnly] {
                    let root = tempfile::tempdir().unwrap();
                    let database = root.path().join("memory.sqlite");
                    let repository =
                        Arc::new(SqliteMemoryRepository::open(database.to_str().unwrap()).unwrap());
                    for path in ["/a.md", "/gone.md"] {
                        repository.create("s", path, "base").await.unwrap();
                    }
                    if existing {
                        repository
                            .create("s", &format!("/{binary_path}"), "base")
                            .await
                            .unwrap();
                    }
                    let mounter = MemoryStoreMounter::copy_only(repository.clone());
                    let projection = root.path().join("projection");
                    let mount = mounter.mount("s", &projection, access).await.unwrap();
                    let evidence = MemoryMaterializationEvidence::new(
                        "s",
                        "/workspace/memory",
                        mount.materialization_heads().unwrap(),
                    )
                    .unwrap();
                    let heads = repository.snapshot_heads("s").await.unwrap();
                    let history = repository.list_versions("s").await.unwrap();
                    let mut files = vec![
                        ("a.md".to_owned(), b"edit".to_vec()),
                        ("b.md".to_owned(), b"new".to_vec()),
                        (binary_path.to_owned(), vec![0xff, 0xfe, 0, 0x42]),
                    ];
                    for (path, bytes) in &files {
                        std::fs::write(projection.join(path), bytes).unwrap();
                    }
                    std::fs::remove_file(projection.join("gone.md")).unwrap();
                    files.reverse(); // Transport order cannot admit a valid prefix.
                    for _ in 0..2 {
                        let result = if recovered {
                            mounter
                                .reconcile_recovered_copy("s", &evidence, &files, access)
                                .await
                        } else {
                            mount.teardown().await
                        };
                        if access == MountAccess::ReadWrite {
                            let error = result.unwrap_err().to_string();
                            assert!(error.contains("non-UTF-8 Memory content"), "U2: {error}");
                        } else {
                            result.expect("U3 no unauthorized writeback");
                        }
                        assert_eq!(repository.snapshot_heads("s").await.unwrap(), heads);
                        assert_eq!(repository.list_versions("s").await.unwrap(), history);
                        assert_eq!(mount.materialization_heads().unwrap(), evidence.heads);
                        for (path, bytes) in &files {
                            assert_eq!(std::fs::read(projection.join(path)).unwrap(), *bytes);
                        }
                    }
                    for (path, bytes) in &mut files {
                        if path == binary_path {
                            *bytes = "修复后的文本".as_bytes().to_vec();
                            std::fs::write(projection.join(path), bytes).unwrap();
                        }
                    }
                    let mut committed_history = None;
                    for _ in 0..2 {
                        if recovered {
                            mounter
                                .reconcile_recovered_copy("s", &evidence, &files, access)
                                .await
                                .unwrap();
                        } else {
                            mount.teardown().await.unwrap();
                        }
                        let after = repository.list_versions("s").await.unwrap();
                        if access == MountAccess::ReadWrite {
                            assert_eq!(after.len(), history.len() + 4, "U4 four effects");
                            for (path, bytes) in &files {
                                let head = repository
                                    .get_by_path("s", &format!("/{path}"))
                                    .await
                                    .unwrap()
                                    .unwrap();
                                assert_eq!(head.content.unwrap().as_bytes(), bytes);
                            }
                            assert!(
                                repository
                                    .get_by_path("s", "/gone.md")
                                    .await
                                    .unwrap()
                                    .is_none()
                            );
                        } else {
                            assert_eq!(after, history);
                        }
                        if let Some(first) = &committed_history {
                            assert_eq!(&after, first, "U4 idempotent replay");
                        }
                        committed_history = Some(after);
                    }
                    let durable_heads = repository.snapshot_heads("s").await.unwrap();
                    drop(mount);
                    drop(mounter);
                    drop(repository);
                    let reopened =
                        SqliteMemoryRepository::open(database.to_str().unwrap()).unwrap();
                    assert_eq!(reopened.snapshot_heads("s").await.unwrap(), durable_heads);
                    assert_eq!(
                        reopened.list_versions("s").await.unwrap(),
                        committed_history.unwrap()
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn ambiguous_projection_shapes_reject_before_any_write_in_both_orders() {
    // S1 duplicate file, S2 file is ancestor, S3 file replaces an ancestor;
    // all entries preflight before an earlier valid edit can commit. Logical
    // Repository may hold overlapping paths; a physical copy cannot.
    for paths in [vec!["b", "b"], vec!["b", "b/c"], vec!["b/c", "b"]] {
        let root = tempfile::tempdir().unwrap();
        let repository = Arc::new(
            SqliteMemoryRepository::open(root.path().join("memory.sqlite").to_str().unwrap())
                .unwrap(),
        );
        repository.create("s", "/a", "base").await.unwrap();
        let history = repository.list_versions("s").await.unwrap();
        let heads = repository.snapshot_heads("s").await.unwrap();
        let evidence =
            MemoryMaterializationEvidence::new("s", "/workspace/memory", vec![]).unwrap();
        let mut files = vec![("a".to_owned(), b"edit".to_vec())];
        files.extend(paths.iter().map(|p| (p.to_string(), b"new".to_vec())));
        assert!(
            MemoryStoreMounter::copy_only(repository.clone())
                .reconcile_recovered_copy("s", &evidence, &files, MountAccess::ReadWrite)
                .await
                .is_err()
        );
        assert_eq!(repository.snapshot_heads("s").await.unwrap(), heads);
        assert_eq!(repository.list_versions("s").await.unwrap(), history);
        if paths[0] != paths[1] {
            for path in paths {
                repository
                    .create("s", &format!("/{path}"), "base")
                    .await
                    .unwrap();
            }
            let projection = root.path().join("projection");
            std::fs::create_dir(&projection).unwrap();
            assert!(
                awaken_sandbox_memoryd::copy::materialize(&*repository, "s", &projection)
                    .await
                    .is_err()
            );
            assert_eq!(std::fs::read_dir(&projection).unwrap().count(), 0);
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn unreadable_projection_never_authorizes_partial_writeback() {
    // IO2 file read denied; IO3 nested directory enumeration denied. A readable
    // dirty sibling exists in both cases. Restore permission BEFORE assertions
    // so even a failed campaign can clean up. This row requires an unprivileged
    // runner: DAC-bypassing execution is not accepted as proof of read failure.
    use std::os::unix::fs::PermissionsExt;
    for directory in [false, true] {
        let root = temporary_root();
        let repository = Arc::new(
            SqliteMemoryRepository::open(root.join("memory.sqlite").to_str().unwrap()).unwrap(),
        );
        let original = repository.create("s", "/a.md", "base").await.unwrap();
        let history = repository.list_versions("s").await.unwrap();
        let mounter = MemoryStoreMounter::copy_only(repository.clone());
        let projection = root.join("projection");
        let mount = mounter
            .mount("s", &projection, MountAccess::ReadWrite)
            .await
            .unwrap();
        std::fs::write(projection.join("a.md"), "dirty").unwrap();
        let denied = projection.join("z");
        if directory {
            std::fs::create_dir(&denied).unwrap();
        } else {
            std::fs::write(&denied, "unreadable").unwrap();
        }
        let permissions = std::fs::metadata(&denied).unwrap().permissions();
        std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o0)).unwrap();
        let result = mount.teardown().await;
        std::fs::set_permissions(&denied, permissions).unwrap();
        assert!(
            result.is_err(),
            "IO2/IO3 require actual permission denial, not a privileged bypass"
        );
        assert_eq!(
            repository.snapshot_heads("s").await.unwrap(),
            vec![original]
        );
        assert_eq!(repository.list_versions("s").await.unwrap(), history);
        mount.teardown().await.unwrap();
        assert_eq!(
            repository
                .get_by_path("s", "/a.md")
                .await
                .unwrap()
                .unwrap()
                .version,
            2
        );
        drop(mount);
        drop(mounter);
        drop(repository);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn empty_entry_and_depth_overflows_fail_before_any_repository_write() {
    // B9 many empty entries bypass byte limits but must consume metadata budget;
    // B10 deep empty directories must stop before unbounded recursive descent.
    // Both contain a valid dirty file: failure preserves exact durable history.
    for deep in [false, true] {
        let root = temporary_root();
        let repository = Arc::new(
            SqliteMemoryRepository::open(root.join("memory.sqlite").to_str().unwrap()).unwrap(),
        );
        let original = repository.create("s", "/a.md", "base").await.unwrap();
        let history = repository.list_versions("s").await.unwrap();
        let mounter = MemoryStoreMounter::copy_only(repository.clone());
        let projection = root.join("projection");
        let mount = mounter
            .mount("s", &projection, MountAccess::ReadWrite)
            .await
            .unwrap();
        std::fs::write(projection.join("a.md"), "dirty").unwrap();
        if deep {
            let mut path = projection.clone();
            for _ in 0..129 {
                path.push("d");
                std::fs::create_dir(&path).unwrap();
            }
        } else {
            for index in 0..awaken_memory_store::MAX_MEMORIES_PER_STORE * 2 {
                std::fs::write(projection.join(format!("z{index}")), "").unwrap();
            }
        }
        assert!(mount.teardown().await.is_err(), "B9/B10");
        assert_eq!(
            repository.snapshot_heads("s").await.unwrap(),
            vec![original]
        );
        assert_eq!(repository.list_versions("s").await.unwrap(), history);
        drop(mount);
        drop(mounter);
        drop(repository);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn unavailable_copy_root_is_not_evidence_of_memory_deletion() {
    // Cause/effect table: C1 frozen nonempty Memory snapshot; C2 root is
    // missing (M1) or replaced by a regular file (M2); C3 root restored (M3).
    // M1/M2 => teardown fails, exact durable heads/history and evidence survive;
    // M3 => retry uses those SAME bases and commits the retained local edit once.
    // Rename preserves the test-owned tree: missing projection != user unlink.
    // M4/M5 a different plain directory (empty/populated) is NOT user unlink.
    // Keep the old directory alive so identity reuse cannot mask replacement.
    for replacement in 0..4 {
        let root = temporary_root();
        let database = root.join("memory.sqlite");
        let repository =
            Arc::new(SqliteMemoryRepository::open(database.to_str().unwrap()).unwrap());
        let original = repository
            .create("s", "/nested/note.md", "base")
            .await
            .unwrap();
        let history = repository.list_versions("s").await.unwrap();
        let mounter = MemoryStoreMounter::copy_only(repository.clone());
        let projection = root.join("projection");
        let retained = root.join("retained");
        let mount = mounter
            .mount("s", &projection, MountAccess::ReadWrite)
            .await
            .unwrap();
        let evidence = mount.materialization_heads();
        std::fs::write(projection.join("nested/note.md"), "local edit").unwrap();
        std::fs::rename(&projection, &retained).unwrap();
        if replacement == 1 {
            std::fs::write(&projection, "not a directory").unwrap();
        } else if replacement >= 2 {
            std::fs::create_dir(&projection).unwrap();
            if replacement == 3 {
                std::fs::write(projection.join("foreign.md"), "foreign").unwrap();
            }
        }
        let failed = mount.teardown().await;
        if replacement == 1 {
            std::fs::remove_file(&projection).unwrap();
        } else if replacement >= 2 {
            if replacement == 3 {
                assert_eq!(
                    std::fs::read_to_string(projection.join("foreign.md")).unwrap(),
                    "foreign"
                );
                std::fs::remove_file(projection.join("foreign.md")).unwrap();
            }
            std::fs::remove_dir(&projection).unwrap();
        }
        std::fs::rename(&retained, &projection).unwrap();
        assert!(
            failed.is_err(),
            "M1/M2 incomplete scan must fail, got {failed:?}"
        );
        assert_eq!(
            repository
                .get_by_path("s", "/nested/note.md")
                .await
                .unwrap(),
            Some(original)
        );
        assert_eq!(repository.list_versions("s").await.unwrap(), history);
        assert_eq!(mount.materialization_heads(), evidence);
        for _ in 0..2 {
            mount.teardown().await.unwrap();
            let head = repository
                .get_by_path("s", "/nested/note.md")
                .await
                .unwrap()
                .unwrap();
            assert_eq!(head.content.as_deref(), Some("local edit"));
            assert_eq!(head.version, 2);
        }
        drop(mount);
        drop(mounter);
        drop(repository);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_copy_harvest_preserves_one_winner_and_retry_evidence() {
    // Cause/effect decision table (both mounts capture the SAME initial head):
    // C1=second operation delete (otherwise update); C2=dispatch order.
    // R1 !C1+simultaneous => exactly one update succeeds, loser conflicts.
    // R2 C1+simultaneous => update OR delete wins, loser conflicts/not-found.
    // R3 either+update-first; R4 either+second-first => prescribed winner.
    // All rules: repeated teardown and recovery with frozen evidence may NOT
    // rebase the loser; reopening SQLite must retain the exact winning head.
    // Barrier forces both snapshots/dirty trees to exist before dispatch. The
    // two ordered rules cover both linearizations independent of OS scheduling.
    // Model link: MemoryCAS.CopySpec / CopyRecoveryKeepsMaterializationBase.
    for deleting in [false, true] {
        for order in [0, 1, 2] {
            let root = temporary_root();
            let database = root.join("memory.sqlite");
            let repository =
                Arc::new(SqliteMemoryRepository::open(database.to_str().unwrap()).unwrap());
            repository.create("s", "/note.md", "base").await.unwrap();
            let mounter = MemoryStoreMounter::copy_only(repository.clone());
            let left_path = root.join("left");
            let right_path = root.join("right");
            let left = mounter
                .mount("s", &left_path, MountAccess::ReadWrite)
                .await
                .unwrap();
            let right = mounter
                .mount("s", &right_path, MountAccess::ReadWrite)
                .await
                .unwrap();
            let evidence = MemoryMaterializationEvidence::new(
                "s",
                "/workspace/memory",
                left.materialization_heads().unwrap(),
            )
            .unwrap();
            assert_eq!(left.materialization_heads(), right.materialization_heads());
            std::fs::write(left_path.join("note.md"), "left").unwrap();
            if deleting {
                std::fs::remove_file(right_path.join("note.md")).unwrap();
            } else {
                std::fs::write(right_path.join("note.md"), "right").unwrap();
            }
            let (a, b) = match order {
                1 => (left.teardown().await, right.teardown().await),
                2 => {
                    let b = right.teardown().await;
                    (left.teardown().await, b)
                }
                _ => {
                    let barrier = tokio::sync::Barrier::new(2);
                    tokio::join!(
                        async {
                            barrier.wait().await;
                            left.teardown().await
                        },
                        async {
                            barrier.wait().await;
                            right.teardown().await
                        },
                    )
                }
            };
            assert_ne!(a.is_ok(), b.is_ok(), "exactly one winner: {a:?}, {b:?}");
            if order == 1 {
                assert!(a.is_ok());
            }
            if order == 2 {
                assert!(b.is_ok());
            }
            let (winner, loser, stale_files) = if a.is_ok() {
                (
                    &left,
                    &right,
                    if deleting {
                        vec![]
                    } else {
                        vec![("note.md".into(), b"right".to_vec())]
                    },
                )
            } else {
                (&right, &left, vec![("note.md".into(), b"left".to_vec())])
            };
            let durable = repository.get_by_path("s", "/note.md").await.unwrap();
            assert_eq!(
                durable.as_ref().and_then(|head| head.content.as_deref()),
                if a.is_ok() {
                    Some("left")
                } else if deleting {
                    None
                } else {
                    Some("right")
                }
            );
            winner.teardown().await.unwrap();
            assert!(
                loser.teardown().await.is_err(),
                "loser must not silently rebase"
            );
            assert!(
                mounter
                    .reconcile_recovered_copy("s", &evidence, &stale_files, MountAccess::ReadWrite)
                    .await
                    .is_err(),
                "restart must retain the frozen base"
            );
            drop(left);
            drop(right);
            drop(mounter);
            drop(repository);
            let reopened = SqliteMemoryRepository::open(database.to_str().unwrap()).unwrap();
            assert_eq!(
                reopened.get_by_path("s", "/note.md").await.unwrap(),
                durable
            );
            drop(reopened);
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn unsafe_copy_entries_fail_before_any_durable_mutation() {
    // Scan decision table: S1 file symlink, S2 directory symlink, S3 cycle,
    // S4 dangling link, S5 FIFO, S6 non-UTF-8 name, S7 literal backslash name,
    // S8 a hard-linked external file (the shared exact walker rejects aliases).
    // All: root exists + one valid dirty entry + unsafe entry => whole scan
    // fails before ANY create/update/delete. Removing only the unsafe entry
    // makes the same guard retryable. No permissions/root-user-dependent skip.
    // These are static corrupt-tree cases, not proof against hostile TOCTOU.
    use std::os::unix::{ffi::OsStringExt, fs::symlink};
    for rule in 1..=8 {
        let root = temporary_root();
        let repository = Arc::new(
            SqliteMemoryRepository::open(root.join("memory.sqlite").to_str().unwrap()).unwrap(),
        );
        let original = repository.create("s", "/a.md", "base").await.unwrap();
        let history = repository.list_versions("s").await.unwrap();
        let projection = root.join("projection");
        let mounter = MemoryStoreMounter::copy_only(repository.clone());
        let mount = mounter
            .mount("s", &projection, MountAccess::ReadWrite)
            .await
            .unwrap();
        std::fs::write(projection.join("a.md"), "dirty").unwrap();
        let external = root.join("external");
        std::fs::create_dir(&external).unwrap();
        std::fs::write(external.join("secret.md"), "not a Memory resource").unwrap();
        let invalid = match rule {
            6 => projection.join(std::ffi::OsString::from_vec(vec![0xff])),
            7 => projection.join("z\\alias.md"),
            _ => projection.join("z"),
        };
        match rule {
            1 => symlink(external.join("secret.md"), &invalid).unwrap(),
            2 => symlink(&external, &invalid).unwrap(),
            3 => symlink(&projection, &invalid).unwrap(),
            4 => symlink(root.join("absent"), &invalid).unwrap(),
            5 => nix::unistd::mkfifo(&invalid, nix::sys::stat::Mode::S_IRUSR).unwrap(),
            8 => std::fs::hard_link(external.join("secret.md"), &invalid).unwrap(),
            _ => std::fs::write(&invalid, "invalid path").unwrap(),
        }
        let result = mount.teardown().await;
        std::fs::remove_file(&invalid).unwrap();
        assert!(
            result.is_err(),
            "S{rule} must reject the entire scan: {result:?}"
        );
        assert_eq!(
            repository.snapshot_heads("s").await.unwrap(),
            vec![original]
        );
        assert_eq!(repository.list_versions("s").await.unwrap(), history);
        assert_eq!(
            std::fs::read_to_string(external.join("secret.md")).unwrap(),
            "not a Memory resource"
        );
        mount.teardown().await.unwrap();
        assert_eq!(
            repository
                .get_by_path("s", "/a.md")
                .await
                .unwrap()
                .unwrap()
                .version,
            2
        );
        drop(mount);
        drop(mounter);
        drop(repository);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn an_existing_empty_copy_is_an_intentional_delete_not_a_missing_tree() {
    // D1: readable root + captured entry explicitly unlinked => conditional
    // deletion succeeds; a retry is idempotent. Contrasts M1 missing ROOT.
    let root = temporary_root();
    let repository = Arc::new(
        SqliteMemoryRepository::open(root.join("memory.sqlite").to_str().unwrap()).unwrap(),
    );
    repository.create("s", "/note.md", "base").await.unwrap();
    let projection = root.join("projection");
    let mounter = MemoryStoreMounter::copy_only(repository.clone());
    let mount = mounter
        .mount("s", &projection, MountAccess::ReadWrite)
        .await
        .unwrap();
    std::fs::remove_file(projection.join("note.md")).unwrap();
    mount.teardown().await.unwrap();
    let history = repository.list_versions("s").await.unwrap();
    mount.teardown().await.unwrap();
    assert!(
        repository
            .get_by_path("s", "/note.md")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(repository.list_versions("s").await.unwrap(), history);
    drop(mount);
    drop(mounter);
    drop(repository);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(windows)]
#[tokio::test]
async fn windows_junction_is_not_a_readable_memory_projection() {
    // W1 root replaced by a junction; W2 nested junction alongside a valid
    // dirty file. Both => reject before write-back, preserve store/history and
    // external bytes. Restoring only the link => same guard commits once.
    // Directory junctions exercise Windows reparse points without requiring
    // symlink privileges/Developer Mode. Missing capability FAILS, never skips.
    for root_link in [false, true] {
        let root = temporary_root();
        let repository = Arc::new(
            SqliteMemoryRepository::open(root.join("memory.sqlite").to_str().unwrap()).unwrap(),
        );
        let original = repository.create("s", "/a.md", "base").await.unwrap();
        let history = repository.list_versions("s").await.unwrap();
        let mounter = MemoryStoreMounter::copy_only(repository.clone());
        let projection = root.join("projection");
        let retained = root.join("retained");
        let external = root.join("external directory");
        std::fs::create_dir(&external).unwrap();
        std::fs::write(external.join("secret.md"), "external bytes").unwrap();
        let mount = mounter
            .mount("s", &projection, MountAccess::ReadWrite)
            .await
            .unwrap();
        std::fs::write(projection.join("a.md"), "dirty").unwrap();
        let junction = if root_link {
            std::fs::rename(&projection, &retained).unwrap();
            projection.clone()
        } else {
            projection.join("junction")
        };
        let created = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&external)
            .output()
            .unwrap();
        assert!(
            created.status.success(),
            "junction fixture: {}",
            String::from_utf8_lossy(&created.stderr)
        );
        let result = mount.teardown().await;
        // Remove the junction itself, never recursively delete its target.
        std::fs::remove_dir(&junction).unwrap();
        if root_link {
            std::fs::rename(&retained, &projection).unwrap();
        }
        assert!(result.is_err(), "W1/W2 incomplete projection must fail");
        assert_eq!(
            repository.snapshot_heads("s").await.unwrap(),
            vec![original]
        );
        assert_eq!(repository.list_versions("s").await.unwrap(), history);
        assert_eq!(
            std::fs::read_to_string(external.join("secret.md")).unwrap(),
            "external bytes"
        );
        mount.teardown().await.unwrap();
        assert_eq!(
            repository
                .get_by_path("s", "/a.md")
                .await
                .unwrap()
                .unwrap()
                .version,
            2
        );
        drop(mount);
        drop(mounter);
        drop(repository);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn partial_copy_commit_keeps_per_file_truth_across_retry_and_reopen() {
    // Cause/effect table: complete scan + a.md valid update + z.md failure.
    // P1 z.md stale CAS => a.md commits, teardown fails, z.md retains durable
    // truth (NO cross-file transaction). P2 z.md exceeds the content limit =>
    // preflight fails with NO writes; repairing z.md permits one a.md commit.
    // P3 retry + SQLite reopen + original evidence =>
    // a.md remains exactly the same version/metadata, z.md still fails; repair
    // only z.md to its durable content => recovery converges without rebasing.
    for oversized in [false, true] {
        let root = temporary_root();
        let database = root.join("memory.sqlite");
        let repository =
            Arc::new(SqliteMemoryRepository::open(database.to_str().unwrap()).unwrap());
        repository.create("s", "/a.md", "a-base").await.unwrap();
        let z_base = repository.create("s", "/z.md", "z-base").await.unwrap();
        let projection = root.join("projection");
        let mounter = MemoryStoreMounter::copy_only(repository.clone());
        let mount = mounter
            .mount("s", &projection, MountAccess::ReadWrite)
            .await
            .unwrap();
        let evidence = MemoryMaterializationEvidence::new(
            "s",
            "/workspace/memory",
            mount.materialization_heads().unwrap(),
        )
        .unwrap();
        let z_local = if oversized {
            "x".repeat(awaken_memory_store::MAX_MEMORY_BYTES + 1)
        } else {
            "z-stale".into()
        };
        let z_durable = if oversized {
            z_base
        } else {
            repository
                .update("s", &z_base.id, "z-winner", &z_base.content_sha256)
                .await
                .unwrap()
        };
        std::fs::write(projection.join("a.md"), "a-committed").unwrap();
        std::fs::write(projection.join("z.md"), &z_local).unwrap();
        let before = repository.snapshot_heads("s").await.unwrap();
        assert!(mount.teardown().await.is_err());
        if oversized {
            assert_eq!(
                repository.snapshot_heads("s").await.unwrap(),
                before,
                "P2 no write before complete valid scan"
            );
            std::fs::write(projection.join("z.md"), "z-base").unwrap();
            mount.teardown().await.unwrap();
            std::fs::write(projection.join("z.md"), &z_local).unwrap();
        }
        let a_durable = repository.get_by_path("s", "/a.md").await.unwrap().unwrap();
        assert_eq!(a_durable.content.as_deref(), Some("a-committed"));
        assert_eq!(a_durable.version, 2);
        let history = repository.list_versions("s").await.unwrap();
        assert!(mount.teardown().await.is_err());
        drop(mount);
        drop(mounter);
        drop(repository);
        let repository =
            Arc::new(SqliteMemoryRepository::open(database.to_str().unwrap()).unwrap());
        let mounter = MemoryStoreMounter::copy_only(repository.clone());
        let mut files = vec![
            ("a.md".into(), b"a-committed".to_vec()),
            ("z.md".into(), z_local.into_bytes()),
        ];
        assert!(
            mounter
                .reconcile_recovered_copy("s", &evidence, &files, MountAccess::ReadWrite)
                .await
                .is_err()
        );
        assert_eq!(
            repository.get_by_path("s", "/a.md").await.unwrap(),
            Some(a_durable)
        );
        assert_eq!(
            repository.get_by_path("s", "/z.md").await.unwrap(),
            Some(z_durable.clone())
        );
        assert_eq!(repository.list_versions("s").await.unwrap(), history);
        files[1].1 = z_durable.content.unwrap().into_bytes();
        mounter
            .reconcile_recovered_copy("s", &evidence, &files, MountAccess::ReadWrite)
            .await
            .unwrap();
        assert_eq!(repository.list_versions("s").await.unwrap(), history);
        drop(mounter);
        drop(repository);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn recovered_copy_retries_committed_write_without_a_second_version() {
    // R5: durable commit + lost response/local snapshot + process replacement
    // => retry exact bytes with ORIGINAL materialization evidence is idempotent.
    // R6: another writer advances after R5 => old retry conflicts, never reverts.
    // This exercises the real SQLite commit boundary, not a mocked CAS result;
    // dropping the guard without teardown represents losing its acknowledgement.
    let root = temporary_root();
    let database = root.join("memory.sqlite");
    let repository = Arc::new(SqliteMemoryRepository::open(database.to_str().unwrap()).unwrap());
    let base = repository.create("s", "/note.md", "base").await.unwrap();
    let mounter = MemoryStoreMounter::copy_only(repository.clone());
    let mount = mounter
        .mount("s", &root.join("projection"), MountAccess::ReadWrite)
        .await
        .unwrap();
    let evidence = MemoryMaterializationEvidence::new(
        "s",
        "/workspace/memory",
        mount.materialization_heads().unwrap(),
    )
    .unwrap();
    let committed = repository
        .update("s", &base.id, "committed", &base.content_sha256)
        .await
        .unwrap();
    drop(mount);
    drop(mounter);
    drop(repository);
    let repository = Arc::new(SqliteMemoryRepository::open(database.to_str().unwrap()).unwrap());
    let replacement = MemoryStoreMounter::copy_only(repository.clone());
    let surviving_files = [("note.md".into(), b"committed".to_vec())];
    for _ in 0..2 {
        replacement
            .reconcile_recovered_copy("s", &evidence, &surviving_files, MountAccess::ReadWrite)
            .await
            .unwrap();
        assert_eq!(
            repository.get_by_path("s", "/note.md").await.unwrap(),
            Some(committed.clone())
        );
    }
    let newer = repository
        .update("s", &base.id, "newer", &committed.content_sha256)
        .await
        .unwrap();
    assert!(
        replacement
            .reconcile_recovered_copy("s", &evidence, &surviving_files, MountAccess::ReadWrite)
            .await
            .is_err()
    );
    assert_eq!(
        repository.get_by_path("s", "/note.md").await.unwrap(),
        Some(newer)
    );
    drop(replacement);
    drop(repository);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_identical_edits_converge_but_different_creates_conflict() {
    // Decision table completes the operation/equality partition of the race:
    // R7 absent base + different creates => 1 success / 1 conflict / version 1.
    // R8 absent base + identical creates => 2 successes / version 1.
    // R9 present base + identical updates => 2 successes / version 2.
    // R10 present base + two deletes => 2 successes / absent head.
    // All retries preserve the exact head including version and timestamps.
    // The barrier is after BOTH materializations and edits, not before mount.
    for rule in [
        "different-create",
        "same-create",
        "same-update",
        "same-delete",
    ] {
        let root = temporary_root();
        let repository = Arc::new(
            SqliteMemoryRepository::open(root.join("memory.sqlite").to_str().unwrap()).unwrap(),
        );
        let seeded = matches!(rule, "same-update" | "same-delete");
        if seeded {
            repository.create("s", "/note.md", "base").await.unwrap();
        }
        let mounter = MemoryStoreMounter::copy_only(repository.clone());
        let left_path = root.join("left");
        let right_path = root.join("right");
        let left = mounter
            .mount("s", &left_path, MountAccess::ReadWrite)
            .await
            .unwrap();
        let right = mounter
            .mount("s", &right_path, MountAccess::ReadWrite)
            .await
            .unwrap();
        for (directory, content) in [
            (&left_path, "left"),
            (
                &right_path,
                if rule == "different-create" {
                    "right"
                } else {
                    "left"
                },
            ),
        ] {
            if rule == "same-delete" {
                std::fs::remove_file(directory.join("note.md")).unwrap();
            } else {
                std::fs::write(directory.join("note.md"), content).unwrap();
            }
        }
        let barrier = tokio::sync::Barrier::new(2);
        let (a, b) = tokio::join!(
            async {
                barrier.wait().await;
                left.teardown().await
            },
            async {
                barrier.wait().await;
                right.teardown().await
            },
        );
        let successes = usize::from(a.is_ok()) + usize::from(b.is_ok());
        assert_eq!(
            successes,
            if rule == "different-create" { 1 } else { 2 },
            "{rule}"
        );
        let durable = repository.get_by_path("s", "/note.md").await.unwrap();
        if rule == "same-delete" {
            assert!(durable.is_none());
        } else {
            let head = durable.as_ref().unwrap();
            assert_eq!(head.version, if seeded { 2 } else { 1 });
            assert_eq!(
                head.content.as_deref(),
                Some(if a.is_ok() { "left" } else { "right" })
            );
        }
        assert_eq!(left.teardown().await.is_ok(), a.is_ok());
        assert_eq!(right.teardown().await.is_ok(), b.is_ok());
        assert_eq!(
            repository.get_by_path("s", "/note.md").await.unwrap(),
            durable
        );
        drop(left);
        drop(right);
        drop(mounter);
        drop(repository);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn copy_mount_reconciles_once_and_survives_mounter_replacement() {
    // Cause/effect graph:
    // C1=the configured Repository contains an initial generation;
    // C2=a canonical read-write MemoryMounter materializes it;
    // C3=the projected tree updates, deletes and creates UTF-8 files;
    // C4=the mount tears down and a fresh Repository/Mounter instance opens the
    // same durable database. Effects: E1=initial bytes are visible; E2=the exact
    // update/delete/create write set is harvested once;
    // E3=the replacement rematerializes durable truth.
    //
    // Decision table (C3 requires C1+C2; C4 follows teardown):
    // M1 C1+C2 => E1; M2 C1+C2+C3 => E2; M3 C1+C2+C3+C4 => E3.
    // FMECA: lost harvest, stale resurrection and replacement reading a local
    // cache as authority. Invalid-byte admission belongs to U2-U5 above.
    let root = temporary_root();
    let database = root.join("memory.sqlite");
    let first_projection = root.join("projection-1");
    let store_id = "memstore-copy-lifecycle";

    let repository = Arc::new(
        SqliteMemoryRepository::open(database.to_str().expect("UTF-8 database path"))
            .expect("open durable repository"),
    );
    repository
        .create(store_id, "/root.md", "one")
        .await
        .expect("seed root");
    repository
        .create(store_id, "/deleted.md", "remove-me")
        .await
        .expect("seed deleted file");
    let mounter = MemoryStoreMounter::copy_only(repository.clone());
    let mount = mounter
        .mount(store_id, &first_projection, MountAccess::ReadWrite)
        .await
        .expect("materialize first generation");

    assert_eq!(
        std::fs::read_to_string(first_projection.join("root.md")).expect("read seed"),
        "one",
        "M1/E1"
    );
    std::fs::write(first_projection.join("root.md"), "two").expect("update root");
    std::fs::remove_file(first_projection.join("deleted.md")).expect("delete memory");
    std::fs::create_dir_all(first_projection.join("nested")).expect("create nested directory");
    std::fs::write(first_projection.join("nested/new.md"), "new").expect("create memory");
    mount.teardown().await.unwrap();
    drop(mounter);
    drop(repository);

    let replacement_repository = Arc::new(
        SqliteMemoryRepository::open(database.to_str().expect("UTF-8 database path"))
            .expect("reopen durable repository"),
    );
    let replacement = MemoryStoreMounter::copy_only(replacement_repository.clone());
    let second_projection = root.join("projection-2");
    let replacement_mount = replacement
        .mount(store_id, &second_projection, MountAccess::ReadOnly)
        .await
        .expect("materialize replacement generation");

    assert_eq!(
        std::fs::read_to_string(second_projection.join("root.md")).expect("read update"),
        "two",
        "M2+M3/E2+E3"
    );
    assert_eq!(
        std::fs::read_to_string(second_projection.join("nested/new.md")).expect("read creation"),
        "new",
        "M2+M3/E2+E3"
    );
    assert!(!second_projection.join("deleted.md").exists(), "M2/E2");

    replacement_mount.teardown().await.unwrap();
    std::fs::remove_dir_all(root).ok();
}
