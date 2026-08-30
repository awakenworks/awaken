//! Process-level integration proof for the canonical copy-only `MemoryMounter`.
//! This replaces the retired `awaken-sandbox memoryd` executable test so there is
//! one production realization path and one matching lifecycle test owner.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use awaken_memory_store::{MemoryRepository, SqliteMemoryRepository};
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
