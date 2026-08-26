use std::sync::Arc;
use std::time::Duration;

use awaken_session_contract::work_queue::WorkQueue;
use rusqlite::{Connection, TransactionBehavior};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn colocated_process_stores_share_wal_and_wait_for_transient_writers() {
    // Cause graph:
    // C1 AllInOne colocates Session, application-access, and WorkQueue schemas.
    // C2 a sibling connection owns the writer reservation briefly.
    // E1 every adapter keeps the shared file in WAL mode.
    // E2 the WorkQueue waits and commits instead of surfacing `database is locked`.
    let dir = tempfile::tempdir().expect("temporary process data root");
    let path = dir.path().join("sessions.db");
    let path = path.to_string_lossy().into_owned();

    let queue = Arc::new(awaken_work_store::SqliteWorkQueue::open(&path).expect("WorkQueue"));
    let _sessions =
        awaken_session_store::SqliteManagedSessionRepository::open(&path).expect("Sessions");
    let _application_access =
        awaken_coordinator::application_access_store::ApplicationAccessStore::open_sqlite(&path)
            .await
            .expect("application access");

    let mut writer = Connection::open(&path).expect("independent writer");
    let journal_mode: String = writer
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("journal mode");
    assert_eq!(journal_mode.to_ascii_lowercase(), "wal", "E1");

    let transaction = writer
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("C2 writer reservation");
    let queued = {
        let queue = queue.clone();
        tokio::spawn(async move { queue.enqueue_session("env_local", "session_waits").await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    transaction.commit().expect("release writer reservation");

    let work_id = tokio::time::timeout(Duration::from_secs(5), queued)
        .await
        .expect("E2 bounded wait")
        .expect("WorkQueue task")
        .expect("E2 WorkQueue commit");
    assert!(!work_id.is_empty(), "E2");
}
