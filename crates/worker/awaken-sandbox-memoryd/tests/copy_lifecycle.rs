//! Durable integration tests for the canonical copy-only `MemoryMounter`.
//! This replaces the retired `awaken-sandbox memoryd` executable test so there is
//! one production realization path and one matching lifecycle test owner.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use awaken_memory_store::{MemoryRepository, SqliteMemoryRepository};
use awaken_provisioning_contract::MemoryMaterializationEvidence;
use awaken_provisioning_contract::{MemoryMounter, MountAccess};
use awaken_sandbox_memoryd::MemoryStoreMounter;

fn temporary_root() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "awaken-memory-mounter-copy-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create temporary root");
    root
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
    // C4=the tree also contains a non-UTF-8 file;
    // C5=the mount tears down and a fresh Repository/Mounter instance opens the
    // same durable database. Effects: E1=initial bytes are visible; E2=the exact
    // update/delete/create write set is harvested once; E3=C4 is excluded;
    // E4=the replacement rematerializes durable truth.
    //
    // Decision table (constraints: C3/C4 require C1+C2; C5 follows teardown):
    // | Rule | C1 | C2 | C3 | C4 | C5 | Effects |
    // | M1   | T  | T  | F  | F  | F  | E1      |
    // | M2   | T  | T  | T  | T  | F  | E2+E3   |
    // | M3   | T  | T  | T  | T  | T  | E4      |
    // FMECA: this detects lost harvest, stale resurrection, invalid-byte
    // ingestion and Pod/Worker replacement reading a local cache as authority.
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
    std::fs::write(first_projection.join("invalid.bin"), [0xff, 0xfe]).expect("write invalid");
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
        "M2+M3/E2+E4"
    );
    assert_eq!(
        std::fs::read_to_string(second_projection.join("nested/new.md")).expect("read creation"),
        "new",
        "M2+M3/E2+E4"
    );
    assert!(!second_projection.join("deleted.md").exists(), "M2/E2");
    assert!(!second_projection.join("invalid.bin").exists(), "M2/E3");
    assert!(
        replacement_repository
            .get_by_path(store_id, "/invalid.bin")
            .await
            .expect("query invalid path")
            .is_none(),
        "M2/E3"
    );

    replacement_mount.teardown().await.unwrap();
    std::fs::remove_dir_all(root).ok();
}
