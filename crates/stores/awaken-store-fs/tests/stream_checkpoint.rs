//! The filesystem checkpoint store is durable across process restarts: a fresh
//! instance over the same directory reads a checkpoint a prior instance wrote —
//! this is what makes an interrupted inference step resumable in a new process.

use awaken_agent_contract::stream::checkpoint::{
    PartialToolCall, StreamCheckpoint, StreamCheckpointStore,
};
use awaken_store_fs::FsStreamCheckpointStore;
use std::sync::Arc;

fn sample(run_id: &str) -> StreamCheckpoint {
    StreamCheckpoint {
        run_id: run_id.to_string(),
        thread_id: "t1".to_string(),
        model: "m".to_string(),
        partial_text: "the partial so far".to_string(),
        partial_tools: vec![PartialToolCall {
            call_id: "c1".to_string(),
            tool_id: "search".to_string(),
            raw_arguments: r#"{"q":"ru"#.to_string(),
        }],
    }
}

#[tokio::test]
async fn a_checkpoint_survives_a_fresh_instance_over_the_same_dir() {
    // Test design — mutable durable-register model:
    // Missing --put(A)--> Present(A) --restart--> Present(A) --delete--> Missing.
    // Delete is idempotent and no acknowledged A may disappear on restart.
    let dir = std::env::temp_dir().join("awaken_ckpt_reopen");
    let _ = std::fs::remove_dir_all(&dir);

    {
        let store = FsStreamCheckpointStore::open(&dir).expect("open");
        store.put(sample("run-1")).await.expect("durable put");
        // dropped here — simulate a process restart
    }

    let store = FsStreamCheckpointStore::open(&dir).expect("reopen");
    let recovered = store
        .get("run-1")
        .await
        .expect("read succeeds")
        .expect("checkpoint survives");
    assert_eq!(recovered, sample("run-1"));

    store.delete("run-1").await.expect("delete");
    assert!(store.get("run-1").await.expect("read").is_none());
    // delete is idempotent.
    store.delete("run-1").await.expect("idempotent delete");

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

    let recovered = store
        .get("../evil/run 1")
        .await
        .expect("read")
        .expect("found");
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
    let recovered = reopened
        .get("one-run")
        .await
        .expect("read")
        .expect("complete winner");
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
    // contract: Missing reads as Ok(None), while Present --corrupt bytes--> must
    // read as Err(Storage). Mapping both states to None would let a caller claim
    // successful recovery while silently discarding durable evidence.
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
    assert!(store.get("corrupt-run").await.is_err());
    assert!(
        store
            .get("missing-run")
            .await
            .expect("missing read")
            .is_none()
    );
    std::fs::remove_dir_all(directory).expect("cleanup checkpoint directory");
}
