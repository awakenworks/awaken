// @formal evidence checkpoint.crash_preserves_durable_truth
//! The filesystem checkpoint store is durable across process restarts: a fresh
//! instance over the same directory reads a checkpoint a prior instance wrote —
//! this is what makes an interrupted inference step resumable in a new process.

use awaken_agent_contract::observation::{Observed, UnknownCause};
use awaken_agent_contract::stream::checkpoint::{StreamCheckpoint, StreamCheckpointStore};
use awaken_reliability_testkit::CrashProcess;
use awaken_store_fs::FsStreamCheckpointStore;
use std::sync::Arc;
use std::time::Duration;

const CRASH_MODE: &str = "AWAKEN_FS_CHECKPOINT_CRASH_MODE";
const CRASH_DIRECTORY: &str = "AWAKEN_FS_CHECKPOINT_CRASH_DIRECTORY";
const CRASH_MARKER: &str = "AWAKEN_FS_CHECKPOINT_CRASH_MARKER";

/// The checkpoint an observation found, for the assertions whose premise is
/// that the store answered. An `Unknown` fails the test rather than reading as
/// absence — the collapse the tri-state read exists to prevent.
fn present(observed: Observed<StreamCheckpoint>) -> StreamCheckpoint {
    match observed {
        Observed::Present(checkpoint) => checkpoint,
        Observed::Absent => panic!("expected a checkpoint, the store reported absence"),
        Observed::Unknown(cause) => panic!("the store could not answer: {cause}"),
    }
}

use awaken_store_conformance::stream_checkpoint::{delete_is_idempotent, sample, writes_are_exact};

#[tokio::test]
async fn acknowledged_checkpoint_mutations_survive_real_process_kill()
-> Result<(), Box<dyn std::error::Error>> {
    // Causes: C1 an old checkpoint A exists; C2 a separate process receives Ok
    // from put(B) or delete; C3 that process is killed immediately after its
    // acknowledgement marker. Effects: E1 reopen after acknowledged put returns
    // the exact B; E2 reopen after acknowledged delete returns None. Rules:
    // R1=C1+put(B)-Ok+C3=>E1; R2=E1+delete-Ok+C3=>E2.
    // Constraint: CrashProcess performs a real process kill without graceful
    // cleanup. This checks the production fsync/rename/directory-fsync path on
    // the current filesystem; power loss, kernel semantics and hardware caches
    // remain external boundaries.
    if let Some(mode) = std::env::var_os(CRASH_MODE) {
        let directory = std::path::PathBuf::from(std::env::var_os(CRASH_DIRECTORY).unwrap());
        let marker = std::path::PathBuf::from(std::env::var_os(CRASH_MARKER).unwrap());
        let store = FsStreamCheckpointStore::open(directory)?;
        match mode.to_str() {
            Some("put") => {
                let mut checkpoint = sample("crash-run");
                checkpoint.partial_text = "after-acknowledged-put".to_string();
                store.put(checkpoint).await?;
            }
            Some("delete") => store.delete("crash-run").await?,
            _ => return Err("unknown checkpoint crash-child mode".into()),
        }
        std::fs::write(marker, b"acknowledged")?;
        tokio::time::sleep(Duration::from_secs(30)).await;
        return Err("parent did not terminate checkpoint crash child".into());
    }

    let root = tempfile::tempdir()?;
    let directory = root.path().join("checkpoints");
    let marker = root.path().join("acknowledged.marker");
    let store = FsStreamCheckpointStore::open(&directory)?;
    store.put(sample("crash-run")).await?;
    drop(store);

    for mode in ["put", "delete"] {
        let _ = std::fs::remove_file(&marker);
        let status = CrashProcess::new(
            "acknowledged_checkpoint_mutations_survive_real_process_kill",
            &marker,
        )
        .env(CRASH_MODE, mode)
        .env(CRASH_DIRECTORY, &directory)
        .env(CRASH_MARKER, &marker)
        .run()
        .map_err(std::io::Error::other)?;
        assert!(!status.success(), "{mode} child must be killed");

        let reopened = FsStreamCheckpointStore::open(&directory)?;
        match mode {
            "put" => assert_eq!(
                present(reopened.get("crash-run").await),
                StreamCheckpoint {
                    partial_text: "after-acknowledged-put".to_string(),
                    ..sample("crash-run")
                },
            ),
            "delete" => assert_eq!(reopened.get("crash-run").await, Observed::Absent, "R2/E2"),
            _ => unreachable!(),
        }
    }
    Ok(())
}

#[tokio::test]
async fn a_checkpoint_survives_a_fresh_instance_over_the_same_dir() {
    // Test design — mutable durable-register model:
    // Missing --put(A)--> Present(A) --restart--> Present(A) --delete--> Missing.
    // Delete is idempotent and no acknowledged A may disappear on restart.
    let dir = std::env::temp_dir().join("awaken_ckpt_reopen");
    let _ = std::fs::remove_dir_all(&dir);

    let expected = {
        let store = FsStreamCheckpointStore::open(&dir).expect("open");
        writes_are_exact(&store, "run-1").await
    };
    let store = FsStreamCheckpointStore::open(&dir).expect("reopen");
    assert_eq!(present(store.get("run-1").await), expected);
    delete_is_idempotent(&store, "run-1").await;

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_put_overwrites_and_path_bearing_ids_stay_scoped_to_the_dir() {
    // Test design — last-write/register and namespace invariants:
    // sequential put(A), put(B) reads B; injective filename encoding keeps every
    // opaque run id inside the configured persistence root.
    let dir = std::env::temp_dir().join("awaken_ckpt_overwrite");
    let _ = std::fs::remove_dir_all(&dir);
    let store = FsStreamCheckpointStore::open(&dir).expect("open");

    // A run id with path separators must not escape the directory or collide.
    let mut first = sample("../evil/run 1");
    first.partial_text = "first".to_string();
    store.put(first).await.expect("first put");
    let mut second = sample("../evil/run 1");
    second.partial_text = "second".to_string();
    store.put(second).await.expect("overwrite");

    let recovered = present(store.get("../evil/run 1").await);
    assert_eq!(recovered.partial_text, "second");
    // The file landed inside `dir`, not at the escaped path.
    assert!(dir.read_dir().expect("readable").count() == 1);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_same_run_puts_leave_one_complete_reopenable_checkpoint() {
    // Test design — concurrent linearizable-register stress:
    // N writers race on one key. The allowed final states are exactly one of the
    // N complete values; torn JSON, missing state, and leftover temp ownership are
    // forbidden. A fresh instance validates that the winner is durable.
    let dir = std::env::temp_dir().join("awaken_ckpt_concurrent_same_run");
    let _ = std::fs::remove_dir_all(&dir);
    let first_store = Arc::new(FsStreamCheckpointStore::open(&dir).expect("open first"));
    let second_store = Arc::new(FsStreamCheckpointStore::open(&dir).expect("open second"));
    let mut tasks = Vec::new();

    for writer in 0..32 {
        let store = if writer % 2 == 0 {
            Arc::clone(&first_store)
        } else {
            Arc::clone(&second_store)
        };
        tasks.push(tokio::spawn(async move {
            let mut value = sample("one-run");
            value.partial_text = format!("writer-{writer}");
            store.put(value).await.expect("concurrent put");
        }));
    }
    for task in tasks {
        task.await.expect("writer task");
    }
    drop(first_store);
    drop(second_store);

    let reopened = FsStreamCheckpointStore::open(&dir).expect("reopen");
    let recovered = present(reopened.get("one-run").await);
    assert!(recovered.partial_text.starts_with("writer-"));
    let entries = std::fs::read_dir(&dir)
        .expect("checkpoint directory")
        .map(|entry| entry.expect("directory entry").file_name())
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 1, "temporary files must be cleaned up");
    assert!(entries[0].to_string_lossy().ends_with(".json"));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn corrupt_checkpoint_is_reported_instead_of_looking_absent() {
    // Test design — observational distinction required by the persistence
    // contract: Missing reads as Observed::Absent, while Present --corrupt
    // bytes--> must read as Observed::Unknown(Storage). Mapping both states to
    // Absent would let a caller claim successful recovery while silently
    // discarding durable evidence.
    let directory = std::env::temp_dir().join(format!(
        "awaken_ckpt_corrupt_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    ));
    let store = FsStreamCheckpointStore::open(&directory).expect("open");
    store
        .put(sample("corrupt-run"))
        .await
        .expect("initial checkpoint");
    let path = std::fs::read_dir(&directory)
        .expect("read checkpoint directory")
        .find_map(|entry| {
            let path = entry.ok()?.path();
            path.extension()
                .is_some_and(|extension| extension == "json")
                .then_some(path)
        })
        .expect("checkpoint file");
    std::fs::write(path, b"not-json").expect("inject corruption");
    assert!(matches!(
        store.get("corrupt-run").await,
        Observed::Unknown(UnknownCause::Storage(_))
    ));
    assert_eq!(store.get("missing-run").await, Observed::Absent);
    std::fs::remove_dir_all(directory).expect("cleanup checkpoint directory");
}
