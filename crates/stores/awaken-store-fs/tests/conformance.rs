//! The filesystem backend passes the shared store conformance suite (ADR-0039 2.6).

use awaken_store_fs::FsCommitCoordinator;

async fn fresh(name: &str) -> FsCommitCoordinator {
    let dir = std::env::temp_dir().join(format!("awaken_store_fs_conf_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    FsCommitCoordinator::open(dir).await.expect("open")
}

#[tokio::test]
async fn commit_then_read() {
    awaken_store_conformance::commit_then_read(&fresh("read").await).await;
}

#[tokio::test]
async fn events_ordered_and_paged() {
    awaken_store_conformance::events_ordered_and_paged(&fresh("events").await).await;
}

#[tokio::test]
async fn commits_accumulate() {
    awaken_store_conformance::commits_accumulate(&fresh("acc").await).await;
}

#[tokio::test]
async fn terminal_run_is_fenced() {
    awaken_store_conformance::terminal_run_is_fenced(&fresh("fence").await).await;
}

#[tokio::test]
async fn waiting_ticket_parks_then_clears() {
    awaken_store_conformance::waiting_ticket_parks_then_clears(&fresh("wait").await).await;
}

#[tokio::test]
async fn concurrent_appends_are_dense_and_distinct() {
    awaken_store_conformance::concurrent_appends_are_dense_and_distinct(&fresh("concurrent").await)
        .await;
}

#[tokio::test]
async fn empty_store_reads_are_absent() {
    awaken_store_conformance::empty_store_reads_are_absent(&fresh("empty").await).await;
}

#[tokio::test]
async fn committed_state_replays() {
    awaken_store_conformance::committed_state_replays(&fresh("state").await).await;
}

// The multi-thread isolation case (`two_threads_in_one_store_are_isolated`) is NOT
// run here: the fs store reuses the single-thread in-memory reference as its read
// model, so it inherits the same flattening. That divergence is characterized in
// `thread_isolation_and_waiting.rs`.
