//! The in-memory backend passes the shared store conformance suite (ADR-0039 2.6).

use awaken_store_inmem::MemoryCommitCoordinator;

#[tokio::test]
async fn commit_then_read() {
    awaken_store_conformance::commit_then_read(&MemoryCommitCoordinator::new()).await;
}

#[tokio::test]
async fn events_ordered_and_paged() {
    awaken_store_conformance::events_ordered_and_paged(&MemoryCommitCoordinator::new()).await;
}

#[tokio::test]
async fn commits_accumulate() {
    awaken_store_conformance::commits_accumulate(&MemoryCommitCoordinator::new()).await;
}

#[tokio::test]
async fn terminal_run_is_fenced() {
    awaken_store_conformance::terminal_run_is_fenced(&MemoryCommitCoordinator::new()).await;
}

#[tokio::test]
async fn waiting_ticket_parks_then_clears() {
    awaken_store_conformance::waiting_ticket_parks_then_clears(&MemoryCommitCoordinator::new())
        .await;
}

#[tokio::test]
async fn concurrent_appends_are_dense_and_distinct() {
    awaken_store_conformance::concurrent_appends_are_dense_and_distinct(
        &MemoryCommitCoordinator::new(),
    )
    .await;
}

#[tokio::test]
async fn empty_store_reads_are_absent() {
    awaken_store_conformance::empty_store_reads_are_absent(&MemoryCommitCoordinator::new()).await;
}

#[tokio::test]
async fn committed_state_replays() {
    awaken_store_conformance::committed_state_replays(&MemoryCommitCoordinator::new()).await;
}

// The multi-thread isolation case (`two_threads_in_one_store_are_isolated`) is
// intentionally NOT run here: the in-memory reference flattens to a single thread
// (one `thread_id`/message vector), so it cannot isolate two threads in one store.
// That flattening is the documented single-thread contract of this reference; the
// filesystem store inherits it and characterizes the divergence explicitly.
