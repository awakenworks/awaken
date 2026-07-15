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
