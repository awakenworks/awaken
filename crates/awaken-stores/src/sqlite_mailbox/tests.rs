use super::*;

fn make_dispatch(id: &str, thread_id: &str) -> RunDispatch {
    RunDispatch::queued(
        id.to_string(),
        thread_id.to_string(),
        format!("run-{id}"),
        1000,
    )
}

#[tokio::test]
async fn enqueue_and_load() {
    let store = SqliteMailboxStore::open_memory().unwrap();
    let dispatch = make_dispatch("dispatch-1", "thread-a");

    store.enqueue(&dispatch).await.unwrap();

    let loaded = store.load_dispatch("dispatch-1").await.unwrap();
    assert!(loaded.is_some());
    let loaded = loaded.unwrap();
    assert_eq!(loaded.dispatch_id(), "dispatch-1");
    assert_eq!(loaded.thread_id(), "thread-a");
    assert_eq!(loaded.run_id(), "run-dispatch-1");
    assert_eq!(loaded.status(), RunDispatchStatus::Queued);
    assert_eq!(loaded.dispatch_epoch(), 0);
    assert_eq!(loaded.priority(), 128);

    // Non-existent dispatch returns None.
    let missing = store.load_dispatch("no-such-dispatch").await.unwrap();
    assert!(missing.is_none());
}

#[tokio::test]
async fn enqueue_dedupe_rejects_duplicate() {
    let store = SqliteMailboxStore::open_memory().unwrap();

    let dispatch1 =
        make_dispatch("dispatch-1", "thread-a").with_dedupe_key(Some("dk-1".to_string()));
    store.enqueue(&dispatch1).await.unwrap();

    // Second enqueue with same dedupe_key should fail.
    let dispatch2 =
        make_dispatch("dispatch-2", "thread-a").with_dedupe_key(Some("dk-1".to_string()));
    let result = store.enqueue(&dispatch2).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        StorageError::AlreadyExists(msg) => assert!(msg.contains("dk-1")),
        other => panic!("expected AlreadyExists, got: {other:?}"),
    }

    // Different dedupe_key should succeed.
    let dispatch3 =
        make_dispatch("dispatch-3", "thread-a").with_dedupe_key(Some("dk-2".to_string()));
    store.enqueue(&dispatch3).await.unwrap();

    // Same dedupe_key in a different thread should succeed.
    let dispatch4 =
        make_dispatch("dispatch-4", "thread-b").with_dedupe_key(Some("dk-1".to_string()));
    store.enqueue(&dispatch4).await.unwrap();
}

#[tokio::test]
async fn list_dispatches_filters_by_status() {
    let store = SqliteMailboxStore::open_memory().unwrap();

    // Enqueue 3 dispatches for the same thread.
    for i in 0..3 {
        let dispatch = make_dispatch(&format!("dispatch-{i}"), "thread-a");
        store.enqueue(&dispatch).await.unwrap();
    }

    // Also enqueue one for a different thread (should not appear).
    let other = make_dispatch("dispatch-other", "thread-b");
    store.enqueue(&other).await.unwrap();

    // List all dispatches for thread-a (no status filter).
    let all = store
        .list_dispatches("thread-a", None, 100, 0)
        .await
        .unwrap();
    assert_eq!(all.len(), 3);

    // Filter by Queued status.
    let queued = store
        .list_dispatches("thread-a", Some(&[RunDispatchStatus::Queued]), 100, 0)
        .await
        .unwrap();
    assert_eq!(queued.len(), 3);

    // Filter by Claimed (none exist).
    let claimed = store
        .list_dispatches("thread-a", Some(&[RunDispatchStatus::Claimed]), 100, 0)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 0);

    // Test limit.
    let limited = store.list_dispatches("thread-a", None, 2, 0).await.unwrap();
    assert_eq!(limited.len(), 2);

    // Test offset.
    let offset = store
        .list_dispatches("thread-a", None, 100, 2)
        .await
        .unwrap();
    assert_eq!(offset.len(), 1);
}

#[tokio::test]
async fn list_dispatches_sorted_by_priority_then_created_at() {
    let store = SqliteMailboxStore::open_memory().unwrap();

    let j1 = make_dispatch("dispatch-low", "thread-a")
        .with_priority(200)
        .with_created_at(100);
    store.enqueue(&j1).await.unwrap();

    let j2 = make_dispatch("dispatch-high", "thread-a")
        .with_priority(10)
        .with_created_at(200);
    store.enqueue(&j2).await.unwrap();

    let j3 = make_dispatch("dispatch-high-early", "thread-a")
        .with_priority(10)
        .with_created_at(50);
    store.enqueue(&j3).await.unwrap();

    let list = store
        .list_dispatches("thread-a", None, 100, 0)
        .await
        .unwrap();
    assert_eq!(list.len(), 3);
    // priority 10 created_at 50
    assert_eq!(list[0].dispatch_id(), "dispatch-high-early");
    // priority 10 created_at 200
    assert_eq!(list[1].dispatch_id(), "dispatch-high");
    // priority 200
    assert_eq!(list[2].dispatch_id(), "dispatch-low");
}

#[tokio::test]
async fn enqueue_sets_dispatch_epoch_from_store() {
    let store = SqliteMailboxStore::open_memory().unwrap();

    let dispatch = make_dispatch("dispatch-1", "thread-a").with_dispatch_epoch(999);
    store.enqueue(&dispatch).await.unwrap();

    let loaded = store.load_dispatch("dispatch-1").await.unwrap().unwrap();
    assert_eq!(
        loaded.dispatch_epoch(),
        0,
        "dispatch_epoch should come from store, not from input"
    );
}

#[tokio::test]
async fn claim_and_ack() {
    let store = SqliteMailboxStore::open_memory().unwrap();
    let dispatch = make_dispatch("dispatch-1", "thread-a");
    store.enqueue(&dispatch).await.unwrap();

    // Claim the dispatch.
    let claimed = store
        .claim("thread-a", "consumer-1", 30_000, 2000, 10)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].dispatch_id(), "dispatch-1");
    assert_eq!(claimed[0].status(), RunDispatchStatus::Claimed);
    assert!(claimed[0].claim_token().is_some());
    assert_eq!(claimed[0].claimed_by(), Some("consumer-1"));
    assert_eq!(claimed[0].lease_until(), Some(32_000));

    // Cannot double-claim while a dispatch is Claimed in this mailbox.
    let dispatch2 = make_dispatch("dispatch-2", "thread-a").with_created_at(2000);
    store.enqueue(&dispatch2).await.unwrap();
    let double = store
        .claim("thread-a", "consumer-2", 30_000, 2000, 10)
        .await
        .unwrap();
    assert!(
        double.is_empty(),
        "should not claim while another is Claimed"
    );

    // Ack the first dispatch.
    let token = claimed[0].claim_token().unwrap();
    store.ack("dispatch-1", token, 3000).await.unwrap();

    let loaded = store.load_dispatch("dispatch-1").await.unwrap().unwrap();
    assert_eq!(loaded.status(), RunDispatchStatus::Acked);
}

#[tokio::test]
async fn claim_honors_batch_limit_without_active_claim() {
    let store = SqliteMailboxStore::open_memory().unwrap();
    for id in ["dispatch-1", "dispatch-2", "dispatch-3"] {
        store.enqueue(&make_dispatch(id, "thread-a")).await.unwrap();
    }

    let claimed = store
        .claim("thread-a", "consumer-1", 30_000, 2000, 2)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 2);
    assert!(
        claimed
            .iter()
            .all(|dispatch| dispatch.status() == RunDispatchStatus::Claimed)
    );
}

#[tokio::test]
async fn nack_increments_attempt_and_requeues() {
    let store = SqliteMailboxStore::open_memory().unwrap();
    let dispatch = make_dispatch("dispatch-1", "thread-a").with_max_attempts(3);
    store.enqueue(&dispatch).await.unwrap();

    // Claim then nack.
    let claimed = store
        .claim("thread-a", "c1", 30_000, 1000, 10)
        .await
        .unwrap();
    let token = claimed[0].claim_token().unwrap();

    store
        .nack("dispatch-1", token, 5000, "transient error", 2000)
        .await
        .unwrap();

    let loaded = store.load_dispatch("dispatch-1").await.unwrap().unwrap();
    assert_eq!(loaded.status(), RunDispatchStatus::Queued);
    assert_eq!(loaded.attempt_count(), 1);
    assert_eq!(loaded.available_at(), 5000);
    assert_eq!(loaded.last_error(), Some("transient error"));
    assert!(loaded.claim_token().is_none());
    assert!(loaded.claimed_by().is_none());
    assert!(loaded.lease_until().is_none());
}

#[tokio::test]
async fn nack_dead_letters_on_max_attempts() {
    let store = SqliteMailboxStore::open_memory().unwrap();
    let dispatch = make_dispatch("dispatch-1", "thread-a").with_max_attempts(1);
    store.enqueue(&dispatch).await.unwrap();

    let claimed = store
        .claim("thread-a", "c1", 30_000, 1000, 10)
        .await
        .unwrap();
    let token = claimed[0].claim_token().unwrap();

    store
        .nack("dispatch-1", token, 5000, "fatal", 2000)
        .await
        .unwrap();

    let loaded = store.load_dispatch("dispatch-1").await.unwrap().unwrap();
    assert_eq!(loaded.status(), RunDispatchStatus::DeadLetter);
    assert_eq!(loaded.attempt_count(), 1);
    assert_eq!(loaded.last_error(), Some("fatal"));
}

#[tokio::test]
async fn dead_letter_explicit() {
    let store = SqliteMailboxStore::open_memory().unwrap();
    let dispatch = make_dispatch("dispatch-1", "thread-a");
    store.enqueue(&dispatch).await.unwrap();

    let claimed = store
        .claim("thread-a", "c1", 30_000, 1000, 10)
        .await
        .unwrap();
    let token = claimed[0].claim_token().unwrap();

    store
        .dead_letter("dispatch-1", token, "permanent failure", 2000)
        .await
        .unwrap();

    let loaded = store.load_dispatch("dispatch-1").await.unwrap().unwrap();
    assert_eq!(loaded.status(), RunDispatchStatus::DeadLetter);
    assert_eq!(loaded.last_error(), Some("permanent failure"));
    assert!(loaded.claim_token().is_none());
    assert!(loaded.claimed_by().is_none());
    assert!(loaded.lease_until().is_none());
}

#[tokio::test]
async fn ack_wrong_token_fails() {
    let store = SqliteMailboxStore::open_memory().unwrap();
    let dispatch = make_dispatch("dispatch-1", "thread-a");
    store.enqueue(&dispatch).await.unwrap();

    let claimed = store
        .claim("thread-a", "c1", 30_000, 1000, 10)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);

    let result = store.ack("dispatch-1", "wrong-token", 2000).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        StorageError::VersionConflict { .. } => {}
        other => panic!("expected VersionConflict, got: {other:?}"),
    }
}

#[tokio::test]
async fn records_dispatch_start_and_run_result_separately_from_ack() {
    use awaken_server_contract::contract::lifecycle::TerminationReason;

    let store = SqliteMailboxStore::open_memory().unwrap();
    let dispatch = make_dispatch("dispatch-1", "thread-a");
    store.enqueue(&dispatch).await.unwrap();

    let claimed = store
        .claim("thread-a", "c1", 30_000, 1000, 1)
        .await
        .unwrap();
    let token = claimed[0].claim_token().unwrap();

    store
        .record_dispatch_start("dispatch-1", token, "dispatch-1", 1500)
        .await
        .unwrap();
    let running = store.load_dispatch("dispatch-1").await.unwrap().unwrap();
    assert_eq!(running.status(), RunDispatchStatus::Claimed);
    assert_eq!(running.run_id(), dispatch.run_id());
    assert_eq!(running.dispatch_instance_id(), Some("dispatch-1"));
    assert_eq!(running.run_status(), Some(RunStatus::Running));
    assert!(running.termination().is_none());
    assert!(running.completed_at().is_none());

    let result = RunDispatchResult {
        run_id: "run-1".into(),
        dispatch_instance_id: "dispatch-1".into(),
        status: RunStatus::Done,
        termination: Some(TerminationReason::Blocked("policy".into())),
        response: None,
        error: Some("policy".into()),
    };
    store
        .record_run_result("dispatch-1", token, &result, 1800)
        .await
        .unwrap();
    let completed = store.load_dispatch("dispatch-1").await.unwrap().unwrap();
    assert_eq!(completed.status(), RunDispatchStatus::Claimed);
    assert_eq!(completed.run_status(), Some(RunStatus::Done));
    assert_eq!(
        completed.termination(),
        Some(&TerminationReason::Blocked("policy".into()))
    );
    assert_eq!(completed.run_error(), Some("policy"));
    assert_eq!(completed.completed_at(), Some(1800));

    store.ack("dispatch-1", token, 2000).await.unwrap();
    let acked = store.load_dispatch("dispatch-1").await.unwrap().unwrap();
    assert_eq!(acked.status(), RunDispatchStatus::Acked);
    assert_eq!(acked.run_status(), Some(RunStatus::Done));
    assert_eq!(acked.run_error(), Some("policy"));
}

#[tokio::test]
async fn record_dispatch_start_rejects_stale_claim_token() {
    let store = SqliteMailboxStore::open_memory().unwrap();
    let dispatch = make_dispatch("dispatch-1", "thread-a");
    store.enqueue(&dispatch).await.unwrap();
    store
        .claim("thread-a", "c1", 30_000, 1000, 1)
        .await
        .unwrap();

    let result = store
        .record_dispatch_start("dispatch-1", "wrong-token", "dispatch-1", 1500)
        .await;
    assert!(matches!(result, Err(StorageError::VersionConflict { .. })));

    let loaded = store.load_dispatch("dispatch-1").await.unwrap().unwrap();
    assert_eq!(loaded.run_id(), dispatch.run_id());
    assert!(loaded.run_status().is_none());
}

#[tokio::test]
async fn cancel_queued_dispatch() {
    let store = SqliteMailboxStore::open_memory().unwrap();
    let dispatch = make_dispatch("dispatch-1", "thread-a");
    store.enqueue(&dispatch).await.unwrap();

    let cancelled = store.cancel("dispatch-1", 2000).await.unwrap();
    assert!(cancelled.is_some());
    let cancelled = cancelled.unwrap();
    assert_eq!(cancelled.status(), RunDispatchStatus::Cancelled);
    assert_eq!(cancelled.updated_at(), 2000);

    // Verify persisted.
    let loaded = store.load_dispatch("dispatch-1").await.unwrap().unwrap();
    assert_eq!(loaded.status(), RunDispatchStatus::Cancelled);

    // Cancel a non-Queued dispatch returns None.
    let again = store.cancel("dispatch-1", 3000).await.unwrap();
    assert!(again.is_none());

    // Cancel non-existent dispatch returns None.
    let missing = store.cancel("no-such-dispatch", 3000).await.unwrap();
    assert!(missing.is_none());
}

#[tokio::test]
async fn interrupt_supersedes_queued() {
    let store = SqliteMailboxStore::open_memory().unwrap();
    store
        .enqueue(&make_dispatch("dispatch-1", "thread-a"))
        .await
        .unwrap();
    store
        .enqueue(&make_dispatch("dispatch-2", "thread-a"))
        .await
        .unwrap();

    let result = store.interrupt("thread-a", 2000).await.unwrap();
    assert_eq!(result.new_dispatch_epoch, 1);
    assert_eq!(result.superseded_count, 2);
    assert!(result.active_dispatch.is_none());

    // Verify dispatches are Superseded.
    let listed = store
        .list_dispatches("thread-a", Some(&[RunDispatchStatus::Superseded]), 100, 0)
        .await
        .unwrap();
    assert_eq!(listed.len(), 2);
}

#[tokio::test]
async fn interrupt_returns_active_claimed_dispatch() {
    let store = SqliteMailboxStore::open_memory().unwrap();
    let dispatch1 = make_dispatch("dispatch-1", "thread-a");
    store.enqueue(&dispatch1).await.unwrap();

    // Claim the first dispatch.
    let claimed = store
        .claim("thread-a", "consumer-1", 30_000, 1000, 1)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);

    // Enqueue a second dispatch (Queued).
    store
        .enqueue(&make_dispatch("dispatch-2", "thread-a"))
        .await
        .unwrap();

    let result = store.interrupt("thread-a", 2000).await.unwrap();
    assert_eq!(result.new_dispatch_epoch, 1);
    assert_eq!(result.superseded_count, 1); // only dispatch-2 was Queued
    assert!(result.active_dispatch.is_some());
    assert_eq!(result.active_dispatch.unwrap().dispatch_id(), "dispatch-1");
}

#[tokio::test]
async fn extend_lease_succeeds() {
    let store = SqliteMailboxStore::open_memory().unwrap();
    let dispatch = make_dispatch("dispatch-1", "thread-a");
    store.enqueue(&dispatch).await.unwrap();

    let claimed = store
        .claim("thread-a", "consumer-1", 30_000, 1000, 1)
        .await
        .unwrap();
    let token = claimed[0].claim_token().unwrap();

    let ok = store
        .extend_lease("dispatch-1", &token, 60_000, 15_000)
        .await
        .unwrap();
    assert!(ok);

    let loaded = store.load_dispatch("dispatch-1").await.unwrap().unwrap();
    assert_eq!(loaded.lease_until(), Some(75_000));

    // Wrong token returns false.
    let nope = store
        .extend_lease("dispatch-1", "wrong-token", 60_000, 20_000)
        .await
        .unwrap();
    assert!(!nope);

    // Non-existent dispatch returns false.
    let nope2 = store
        .extend_lease("no-such-dispatch", &token, 60_000, 20_000)
        .await
        .unwrap();
    assert!(!nope2);
}

#[tokio::test]
async fn reclaim_expired_leases() {
    let store = SqliteMailboxStore::open_memory().unwrap();
    let dispatch = make_dispatch("dispatch-1", "thread-a");
    store.enqueue(&dispatch).await.unwrap();

    // Claim with a short lease.
    let claimed = store
        .claim("thread-a", "consumer-1", 1_000, 1000, 1)
        .await
        .unwrap();
    assert_eq!(claimed[0].lease_until(), Some(2_000));

    // At now=3000, lease is expired.
    let reclaimed = store.reclaim_expired_leases(3000, 10).await.unwrap();
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].dispatch_id(), "dispatch-1");
    assert_eq!(reclaimed[0].status(), RunDispatchStatus::Queued);
    assert_eq!(reclaimed[0].attempt_count(), 1);
    assert!(reclaimed[0].claim_token().is_none());
    assert!(reclaimed[0].claimed_by().is_none());
    assert!(reclaimed[0].lease_until().is_none());

    // Not expired yet: no reclaims.
    let claimed2 = store
        .claim("thread-a", "consumer-2", 100_000, 4000, 1)
        .await
        .unwrap();
    assert_eq!(claimed2.len(), 1);
    let none = store.reclaim_expired_leases(5000, 10).await.unwrap();
    assert!(none.is_empty());
}

#[tokio::test]
async fn purge_terminal_removes_old() {
    let store = SqliteMailboxStore::open_memory().unwrap();

    // Create and cancel a dispatch (terminal).
    let dispatch1 = make_dispatch("dispatch-1", "thread-a");
    store.enqueue(&dispatch1).await.unwrap();
    store.cancel("dispatch-1", 1000).await.unwrap();

    // Create a Queued dispatch (non-terminal, should not be purged).
    let dispatch2 = make_dispatch("dispatch-2", "thread-a");
    store.enqueue(&dispatch2).await.unwrap();

    // Purge with threshold after the cancelled dispatch's updated_at.
    let purged = store.purge_terminal(2000).await.unwrap();
    assert_eq!(purged, 1);

    // The cancelled dispatch is gone.
    let loaded = store.load_dispatch("dispatch-1").await.unwrap();
    assert!(loaded.is_none());

    // The queued dispatch remains.
    let loaded2 = store.load_dispatch("dispatch-2").await.unwrap();
    assert!(loaded2.is_some());
}

#[tokio::test]
async fn queued_thread_ids() {
    let store = SqliteMailboxStore::open_memory().unwrap();

    // No dispatches yet.
    let ids = store.queued_thread_ids().await.unwrap();
    assert!(ids.is_empty());

    // Add queued dispatches in two threads.
    store
        .enqueue(&make_dispatch("dispatch-1", "thread-b"))
        .await
        .unwrap();
    store
        .enqueue(&make_dispatch("dispatch-2", "thread-a"))
        .await
        .unwrap();
    store
        .enqueue(&make_dispatch("dispatch-3", "thread-a"))
        .await
        .unwrap();

    let ids = store.queued_thread_ids().await.unwrap();
    assert_eq!(ids, vec!["thread-a", "thread-b"]);

    // Cancel all dispatches in thread-a.
    store.cancel("dispatch-2", 2000).await.unwrap();
    store.cancel("dispatch-3", 2000).await.unwrap();

    let ids = store.queued_thread_ids().await.unwrap();
    assert_eq!(ids, vec!["thread-b"]);
}

#[test]
fn open_creates_run_dispatches_schema() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("mailbox.sqlite");

    let store = SqliteMailboxStore::open(&db_path).unwrap();
    let conn = store.conn.try_lock().unwrap();
    for column in [
        "run_id",
        "dispatch_epoch",
        "dispatch_instance_id",
        "run_status",
        "termination",
        "run_response",
        "run_error",
        "completed_at",
    ] {
        let exists: bool = conn
            .prepare_cached(
                "SELECT EXISTS(
                    SELECT 1 FROM pragma_table_info('run_dispatches')
                    WHERE name = ?1
                )",
            )
            .unwrap()
            .query_row(params![column], |row| row.get(0))
            .unwrap();
        assert!(exists, "missing run dispatch column {column}");
    }
}
