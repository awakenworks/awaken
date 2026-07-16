//! The filesystem checkpoint store is durable across process restarts: a fresh
//! instance over the same directory reads a checkpoint a prior instance wrote —
//! this is what makes an interrupted inference step resumable in a new process.

use awaken_agent_contract::stream::checkpoint::{
    PartialToolCall, StreamCheckpoint, StreamCheckpointStore,
};
use awaken_store_fs::FsStreamCheckpointStore;

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
    let dir = std::env::temp_dir().join("awaken_ckpt_reopen");
    let _ = std::fs::remove_dir_all(&dir);

    {
        let store = FsStreamCheckpointStore::open(&dir).expect("open");
        store.put(sample("run-1")).await;
        // dropped here — simulate a process restart
    }

    let store = FsStreamCheckpointStore::open(&dir).expect("reopen");
    let recovered = store.get("run-1").await.expect("checkpoint survives");
    assert_eq!(recovered, sample("run-1"));

    store.delete("run-1").await;
    assert!(store.get("run-1").await.is_none());
    // delete is idempotent.
    store.delete("run-1").await;

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_put_overwrites_and_path_bearing_ids_stay_scoped_to_the_dir() {
    let dir = std::env::temp_dir().join("awaken_ckpt_overwrite");
    let _ = std::fs::remove_dir_all(&dir);
    let store = FsStreamCheckpointStore::open(&dir).expect("open");

    // A run id with path separators must not escape the directory or collide.
    let mut first = sample("../evil/run 1");
    first.partial_text = "first".to_string();
    store.put(first).await;
    let mut second = sample("../evil/run 1");
    second.partial_text = "second".to_string();
    store.put(second).await;

    let recovered = store.get("../evil/run 1").await.expect("found");
    assert_eq!(recovered.partial_text, "second");
    // The file landed inside `dir`, not at the escaped path.
    assert!(dir.read_dir().expect("readable").count() == 1);

    let _ = std::fs::remove_dir_all(&dir);
}
