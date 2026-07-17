//! The `memoryd` role's copy realization end-to-end, in-process: seed a sqlite-backed
//! memory store, run the sidecar's copy cycle, edit the projected files as the agent
//! would, and prove the edits are harvested back to the store on shutdown (ADR-0053).
//! Portable — the copy path needs no `/dev/fuse`, so it runs in CI; the FUSE path is
//! covered by the memoryd crate's own fuse tests and falls back to this on a locked node.
#![cfg(feature = "memoryd")]

use std::time::Duration;

use awaken_memory_store::{MemoryFs, SqliteMemoryFs};
use awaken_sandbox::memoryd::{MemorydConfig, serve, serve_copy};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn copy_cycle_materializes_then_harvests_agent_edits() {
    let dir = std::env::temp_dir().join(format!("awaken-memoryd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store_dir = dir.join("store");
    let mount = dir.join("mnt");
    std::fs::create_dir_all(&store_dir).expect("store dir");
    std::fs::create_dir_all(&mount).expect("mount dir");

    // A durable store seeded with one memory the sidecar will project as a file.
    let fs = SqliteMemoryFs::open(store_dir.join("memory.db").to_str().unwrap()).expect("open");
    fs.create("s", "/notes/todo.md", "hello")
        .await
        .expect("seed");

    // The agent edits the existing file and drops a new one, then signals shutdown so
    // the sidecar harvests. `join!` (not spawn) keeps the shared `&fs` borrow single-task.
    let (tx, rx) = tokio::sync::oneshot::channel();
    let editor = async {
        let todo = mount.join("notes/todo.md");
        for _ in 0..200 {
            if todo.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(todo.exists(), "the store must be materialized to the mount");
        std::fs::write(&todo, "hello world").expect("edit");
        std::fs::write(mount.join("notes/new.md"), "fresh").expect("create");
        let _ = tx.send(());
    };
    let server = serve_copy(&fs, "s", &mount, async {
        let _ = rx.await;
    });
    let (res, ()) = tokio::join!(server, editor);
    res.expect("serve_copy completes cleanly");

    // The edits round-tripped back into the durable store.
    let todo = fs
        .get_by_path("s", "/notes/todo.md")
        .await
        .expect("get todo")
        .expect("todo exists");
    assert_eq!(
        todo.content.as_deref(),
        Some("hello world"),
        "the agent's edit was harvested back to the store"
    );
    let created = fs
        .get_by_path("s", "/notes/new.md")
        .await
        .expect("get new")
        .expect("new memory harvested");
    assert_eq!(
        created.content.as_deref(),
        Some("fresh"),
        "a file the agent created became a new memory"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Drive the full `serve` entrypoint (not just `serve_copy`): it opens the store-owned
/// sqlite db, creates the backing + mount dirs, selects the copy realization (no FUSE
/// requested), and round-trips. Covers `serve` + the store-open + dir-creation path the
/// container e2e exercises against the real binary, but here without a daemon.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_entrypoint_selects_copy_and_round_trips() {
    let dir = std::env::temp_dir().join(format!("awaken-memoryd-serve-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store_dir = dir.join("store");
    let mount = dir.join("mnt");
    std::fs::create_dir_all(&store_dir).expect("store dir");
    let db = store_dir.join("memory.db");
    // Seed the store db that `serve` will open (a separate connection, then dropped).
    {
        let fs = SqliteMemoryFs::open(db.to_str().unwrap()).expect("seed open");
        fs.create("s", "/seed.md", "seeded").await.expect("seed");
    }

    let cfg = MemorydConfig {
        store_id: "s".into(),
        mount_path: mount.clone(),
        store_dir: store_dir.clone(),
        want_fuse: false,
    };
    let (tx, rx) = tokio::sync::oneshot::channel();
    let editor = async {
        let seed = mount.join("seed.md");
        for _ in 0..200 {
            if seed.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            seed.exists(),
            "serve materialized the seeded store to the mount"
        );
        assert_eq!(std::fs::read_to_string(&seed).unwrap().trim(), "seeded");
        std::fs::write(mount.join("fresh.md"), "created").expect("agent write");
        let _ = tx.send(());
    };
    let server = serve(&cfg, async {
        let _ = rx.await;
    });
    let (res, ()) = tokio::join!(server, editor);
    res.expect("serve completes cleanly");

    // The agent's new file was harvested into the durable store.
    let fs = SqliteMemoryFs::open(db.to_str().unwrap()).expect("assert open");
    let harvested = fs
        .get_by_path("s", "/fresh.md")
        .await
        .expect("get fresh")
        .expect("fresh harvested");
    assert_eq!(harvested.content.as_deref(), Some("created"));

    let _ = std::fs::remove_dir_all(&dir);
}
