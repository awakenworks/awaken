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
