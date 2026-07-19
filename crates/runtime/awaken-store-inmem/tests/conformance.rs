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
async fn resume_ticket_awaits_then_clears() {
    awaken_store_conformance::resume_ticket_awaits_then_clears(&MemoryCommitCoordinator::new())
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

#[tokio::test]
async fn delegation_and_tool_state_commit_atomically() {
    awaken_store_conformance::delegation_and_tool_state_commit_atomically(
        &MemoryCommitCoordinator::new(),
    )
    .await;
}

// The in-memory reference keys committed truth by thread, so two threads committed to
// one store stay isolated (each reads only its own transcript/state/latest run) — it
// runs the shared multi-thread isolation case, as the SQLite / Postgres backends do.
// The filesystem store, which reuses this reference as its read model, inherits the
// isolation.
#[tokio::test]
async fn two_threads_in_one_store_are_isolated() {
    awaken_store_conformance::two_threads_in_one_store_are_isolated(&MemoryCommitCoordinator::new())
        .await;
}
