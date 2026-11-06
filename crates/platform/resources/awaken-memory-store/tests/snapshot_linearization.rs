//! T23-C01 (并发单 store 快照): two files are committed alternately while
//! `snapshot_heads` runs concurrently, with the writes and the snapshots on
//! independently opened clients of one durable database.
//!
//! | Row | Backend | Writer / reader | Checker |
//! |---|---|---|---|
//! | L1 | real-file SQLite | two `open(path)` handles | `snapshot_heads_follow_one_committed_cross_file_prefix_through` |
//! | L2 | PostgreSQL | two pools on one fresh schema | same |
//! | NC | real-file SQLite | a list-then-get-per-file reader | the same prefix model must reject its mixed state |
//!
//! The volatile and single-handle rows are in `conformance.rs`.
//! Oracles (groups-4): O0 each snapshot is one complete committed prefix;
//! O1 no list+get mixture of versions; O2 the foreign store is isolated and
//! the order is stable (asserted inside the shared checker).

#![cfg(any(feature = "sqlite", feature = "postgres"))]

use std::sync::Arc;
#[cfg(feature = "sqlite")]
use std::sync::Mutex;
#[cfg(feature = "sqlite")]
use std::time::Duration;

#[cfg(feature = "sqlite")]
use awaken_memory_store::{
    MemErr, Memory, MemoryQueries, MemorySnapshotSource, MemoryWriter, SqliteMemoryRepository,
};
#[cfg(feature = "sqlite")]
use tokio::sync::oneshot;

#[path = "common/memory_repository.rs"]
mod shared;

#[cfg(feature = "postgres")]
#[path = "common/pg_schema.rs"]
mod pg;

#[cfg(feature = "sqlite")]
const STORE: &str = "stitched";

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_sqlite_independent_clients_snapshot_one_committed_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("snapshots.db");
    let path = path.to_str().unwrap();
    let writer = SqliteMemoryRepository::open(path, awaken_time::system_clock())
        .await
        .unwrap();
    let reader = SqliteMemoryRepository::open(path, awaken_time::system_clock())
        .await
        .unwrap();
    shared::snapshot_heads_follow_one_committed_cross_file_prefix_through(
        Arc::new(writer),
        Arc::new(reader),
    )
    .await;
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_independent_pools_snapshot_one_committed_prefix() {
    use awaken_memory_store::PostgresMemoryRepository;
    let Some((url, schema)) = pg::fresh_schema("t23_snapshot_prefix").await else {
        return;
    };
    let writer = PostgresMemoryRepository::with_pool(
        pg::connect(&url, &schema, "t23-writer").await,
        awaken_time::system_clock(),
    );
    writer.ensure_schema().await.unwrap();
    let reader = PostgresMemoryRepository::with_existing_pool(
        pg::connect(&url, &schema, "t23-reader").await,
        awaken_time::system_clock(),
    )
    .await
    .unwrap();
    shared::snapshot_heads_follow_one_committed_cross_file_prefix_through(
        Arc::new(writer),
        Arc::new(reader),
    )
    .await;
    pg::drop_schema(&url, &schema).await;
}

#[cfg(feature = "postgres")]
#[test]
fn postgres_independent_pools_snapshot_one_committed_prefix_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_independent_pools_snapshot_one_committed_prefix",
    );
}

/// The prefix model: the index of the committed prefix `heads` equals, if any.
/// `writes` are the committed heads in commit order over `initial`.
#[cfg(feature = "sqlite")]
fn committed_prefix(initial: &[Memory], writes: &[Memory], heads: &[Memory]) -> Option<usize> {
    let mut model = initial.to_vec();
    for prefix in 0..=writes.len() {
        if prefix > 0 {
            let head = &writes[prefix - 1];
            let slot = model.iter().position(|m| m.path == head.path).unwrap();
            model[slot] = head.clone();
        }
        if model == heads {
            return Some(prefix);
        }
    }
    None
}

/// A reader that lists the store and then reads each file on its own, pausing
/// after the first file so commits can land between the per-file reads.
#[cfg(feature = "sqlite")]
struct StitchedSnapshot {
    inner: SqliteMemoryRepository,
    paused: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
}

#[cfg(feature = "sqlite")]
#[async_trait::async_trait]
impl MemorySnapshotSource for StitchedSnapshot {
    async fn snapshot_heads(&self, store: &str) -> Result<Vec<Memory>, MemErr> {
        let entries = MemoryQueries::list(&self.inner, store, "/").await?;
        let mut heads = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            let head = MemoryQueries::get_by_path(&self.inner, store, &entry.path)
                .await?
                .expect("listed file exists");
            heads.push(head);
            if index == 0 {
                let pause = self.paused.lock().unwrap().take();
                if let Some((reached, proceed)) = pause {
                    reached.send(()).expect("the test awaits the pause");
                    tokio::time::timeout(Duration::from_secs(30), proceed)
                        .await
                        .expect("the test releases the stitched reader")
                        .expect("release signal");
                }
            }
        }
        Ok(heads)
    }
}

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_then_get_snapshot_is_rejected_by_the_prefix_model() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stitched.db");
    let path = path.to_str().unwrap();
    let writer = SqliteMemoryRepository::open(path, awaken_time::system_clock())
        .await
        .unwrap();
    let a = MemoryWriter::create(&writer, STORE, "/a.md", "0")
        .await
        .unwrap();
    let b = MemoryWriter::create(&writer, STORE, "/b.md", "0")
        .await
        .unwrap();
    let initial = vec![a.clone(), b.clone()];

    let (reached_tx, reached_rx) = oneshot::channel();
    let (proceed_tx, proceed_rx) = oneshot::channel();
    let stitched = Arc::new(StitchedSnapshot {
        inner: SqliteMemoryRepository::open(path, awaken_time::system_clock())
            .await
            .unwrap(),
        paused: Mutex::new(Some((reached_tx, proceed_rx))),
    });
    let observing = {
        let stitched = stitched.clone();
        tokio::spawn(async move { stitched.snapshot_heads(STORE).await })
    };
    tokio::time::timeout(Duration::from_secs(30), reached_rx)
        .await
        .expect("the stitched reader pauses after its first file")
        .expect("pause signal");
    // Two commits land between the stitched reader's per-file reads.
    let a1 = MemoryWriter::update(&writer, STORE, &a.generation(), "1")
        .await
        .unwrap();
    let b1 = MemoryWriter::update(&writer, STORE, &b.generation(), "1")
        .await
        .unwrap();
    let writes = vec![a1, b1];
    proceed_tx
        .send(())
        .expect("the stitched reader awaits release");
    let mixed = tokio::time::timeout(Duration::from_secs(30), observing)
        .await
        .expect("the stitched snapshot completes")
        .unwrap()
        .unwrap();

    assert_eq!(
        committed_prefix(&initial, &writes, &mixed),
        None,
        "NC/O1: the list+get reader returned a state no commit prefix produced: {mixed:?}"
    );
    // The production snapshot over the same database is one whole prefix.
    let atomic = MemorySnapshotSource::snapshot_heads(&stitched.inner, STORE)
        .await
        .unwrap();
    assert_eq!(
        committed_prefix(&initial, &writes, &atomic),
        Some(writes.len()),
        "O0: the production snapshot is the complete committed prefix"
    );
}
