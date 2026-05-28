#![cfg(feature = "postgres")]

use awaken_server_contract::contract::outbox::{
    OutboxMessageDraft, OutboxNackOutcome, OutboxStatus, OutboxStore,
};
use awaken_stores::PostgresStore;
use sqlx::{PgPool, Row};
use std::sync::Arc;

async fn test_pool() -> PgPool {
    let url = std::env::var("PG_TEST_URL")
        .unwrap_or_else(|_| "postgres://localhost/awaken_test".to_string());
    PgPool::connect(&url).await.unwrap()
}

async fn test_store(prefix: &str) -> PostgresStore {
    PostgresStore::with_prefix(test_pool().await, prefix)
}

fn unique_prefix(name: &str) -> String {
    let uuid_short = uuid::Uuid::now_v7().simple().to_string();
    format!("pgo_{}_{}", &uuid_short[..12], &name[..name.len().min(8)])
}

fn draft(payload: i64) -> OutboxMessageDraft {
    OutboxMessageDraft::new(
        "canonical",
        "projector",
        serde_json::json!({ "value": payload }),
    )
    .unwrap()
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL or local postgres://localhost/awaken_test"]
async fn postgres_outbox_schema_initializes_idempotently() {
    let prefix = unique_prefix("schema");
    let store = test_store(&prefix).await;
    store.ensure_schema().await.unwrap();
    store.ensure_schema().await.unwrap();

    let pool = test_pool().await;
    let count: i64 = sqlx::query(
        "SELECT COUNT(*)::BIGINT AS count
         FROM information_schema.tables
         WHERE table_schema = current_schema()
           AND table_name = $1",
    )
    .bind(format!("{prefix}_outbox"))
    .fetch_one(&pool)
    .await
    .unwrap()
    .try_get("count")
    .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL or local postgres://localhost/awaken_test"]
async fn postgres_outbox_enqueue_claim_ack() {
    let store = test_store(&unique_prefix("ack")).await;
    let first = store.enqueue_outbox(draft(1)).await.unwrap().message;
    store.enqueue_outbox(draft(2)).await.unwrap();

    let claimed = store
        .claim_outbox("canonical", "projector", 1, 1_000, "worker-a", 10)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].outbox_id, first.outbox_id);

    let token = claimed[0].claim_token.as_deref().unwrap();
    assert!(store.ack_outbox(&first.outbox_id, token, 20).await.unwrap());
    assert!(!store.ack_outbox(&first.outbox_id, token, 21).await.unwrap());

    let delivered = store
        .list_outbox(Some(OutboxStatus::Delivered), 10)
        .await
        .unwrap();
    assert_eq!(delivered.len(), 1);
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL or local postgres://localhost/awaken_test"]
async fn postgres_outbox_concurrent_claim_same_row_has_single_owner() {
    let store = Arc::new(test_store(&unique_prefix("claim_race")).await);
    let message = store.enqueue_outbox(draft(1)).await.unwrap().message;

    let attempts = (0..16)
        .map(|idx| {
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                store
                    .claim_outbox(
                        "canonical",
                        "projector",
                        1,
                        1_000,
                        &format!("worker-{idx}"),
                        10,
                    )
                    .await
                    .unwrap()
            })
        })
        .collect::<Vec<_>>();

    let mut winners = Vec::new();
    for attempt in attempts {
        winners.extend(attempt.await.expect("claim task"));
    }

    assert_eq!(
        winners.len(),
        1,
        "Postgres SKIP LOCKED claim must produce exactly one owner"
    );
    assert_eq!(winners[0].outbox_id, message.outbox_id);
    assert_eq!(winners[0].status, OutboxStatus::Claimed);
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL or local postgres://localhost/awaken_test"]
async fn postgres_outbox_reclaim_rejects_late_old_owner_ack() {
    let store = test_store(&unique_prefix("late_ack")).await;
    let message = store.enqueue_outbox(draft(1)).await.unwrap().message;

    let claimed = store
        .claim_outbox("canonical", "projector", 1, 100, "worker-a", 1_000)
        .await
        .unwrap();
    let old_token = claimed[0].claim_token.clone().expect("old token");

    let reclaimed = store
        .claim_outbox("canonical", "projector", 1, 30_000, "worker-b", 1_100)
        .await
        .unwrap();
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].outbox_id, message.outbox_id);
    let new_token = reclaimed[0].claim_token.clone().expect("new token");

    assert!(
        !store
            .ack_outbox(&message.outbox_id, &old_token, 1_101)
            .await
            .unwrap(),
        "late ack from old owner must not clear a reclaimed outbox row"
    );
    assert!(
        store
            .ack_outbox(&message.outbox_id, &new_token, 1_102)
            .await
            .unwrap(),
        "new owner ack must succeed"
    );
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL or local postgres://localhost/awaken_test"]
async fn postgres_outbox_retry_window_respects_available_at_boundary() {
    let store = test_store(&unique_prefix("retry_window")).await;
    let message = store.enqueue_outbox(draft(1)).await.unwrap().message;

    let claimed = store
        .claim_outbox("canonical", "projector", 1, 30_000, "worker-a", 1_000)
        .await
        .unwrap();
    let token = claimed[0].claim_token.clone().expect("claim token");
    let outcome = store
        .nack_outbox(&message.outbox_id, &token, "retry later", 2_000, 1_001)
        .await
        .unwrap();
    assert_eq!(outcome, OutboxNackOutcome::Requeued);

    assert!(
        store
            .claim_outbox("canonical", "projector", 1, 30_000, "worker-b", 1_999)
            .await
            .unwrap()
            .is_empty(),
        "outbox row must not be claimable before retry_at"
    );
    let claimed_at_retry = store
        .claim_outbox("canonical", "projector", 1, 30_000, "worker-b", 2_000)
        .await
        .unwrap();
    assert_eq!(claimed_at_retry.len(), 1);
    assert_eq!(claimed_at_retry[0].outbox_id, message.outbox_id);
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL or local postgres://localhost/awaken_test"]
async fn postgres_outbox_concurrent_dedupe_enqueue_returns_single_row() {
    let store = Arc::new(test_store(&unique_prefix("dedupe_race")).await);
    let mut template = draft(1);
    template.dedupe_key = Some("same-event/projector".into());

    let attempts = (0..16)
        .map(|_| {
            let store = Arc::clone(&store);
            let draft = template.clone();
            tokio::spawn(async move { store.enqueue_outbox(draft).await.unwrap().message })
        })
        .collect::<Vec<_>>();

    let mut outbox_ids = Vec::new();
    for attempt in attempts {
        outbox_ids.push(attempt.await.expect("enqueue task").outbox_id);
    }
    outbox_ids.sort();
    outbox_ids.dedup();

    assert_eq!(
        outbox_ids.len(),
        1,
        "concurrent idempotent enqueue with one dedupe key must converge to one row"
    );
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL or local postgres://localhost/awaken_test"]
async fn postgres_outbox_nack_requeue_dead_letter_and_reclaim() {
    let store = test_store(&unique_prefix("nack")).await;
    let mut draft = draft(1);
    draft.max_attempts = 2;
    let message = store.enqueue_outbox(draft).await.unwrap().message;

    let claimed = store
        .claim_outbox("canonical", "projector", 1, 10, "worker-a", 10)
        .await
        .unwrap();
    let reclaimed = store
        .claim_outbox("canonical", "projector", 1, 10, "worker-b", 21)
        .await
        .unwrap();
    assert_eq!(reclaimed[0].attempt_count, 2);
    assert_eq!(reclaimed[0].claimed_by.as_deref(), Some("worker-b"));

    let stale_token = claimed[0].claim_token.as_deref().unwrap();
    let outcome = store
        .nack_outbox(&message.outbox_id, stale_token, "stale", 40, 30)
        .await
        .unwrap();
    assert_eq!(outcome, OutboxNackOutcome::LostClaim);

    let token = reclaimed[0].claim_token.as_deref().unwrap();
    let outcome = store
        .nack_outbox(&message.outbox_id, token, "permanent", 50, 35)
        .await
        .unwrap();
    assert_eq!(outcome, OutboxNackOutcome::DeadLettered);
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL or local postgres://localhost/awaken_test"]
async fn enqueue_outbox_in_transaction_shares_caller_transaction() {
    // ADR-0034 D9: transactional enqueue lets control-plane writers
    // commit the canonical event row, domain table changes, and the
    // outbox row together. Rolling back the caller's transaction must
    // also discard the outbox row.
    let prefix = unique_prefix("tx_enqueue");
    let store = test_store(&prefix).await;
    store.ensure_schema().await.unwrap();
    let pool = test_pool().await;

    // Successful commit path persists the row.
    let mut tx = pool.begin().await.unwrap();
    let result = awaken_stores::enqueue_outbox_in_transaction(&store, &mut tx, draft(11))
        .await
        .expect("enqueue in tx");
    tx.commit().await.unwrap();
    let listed = store.list_outbox(None, 10).await.unwrap();
    assert!(
        listed
            .iter()
            .any(|m| m.outbox_id == result.message.outbox_id)
    );

    // Rollback path discards the row.
    let mut tx = pool.begin().await.unwrap();
    let rolled = awaken_stores::enqueue_outbox_in_transaction(&store, &mut tx, draft(22))
        .await
        .expect("enqueue in tx");
    tx.rollback().await.unwrap();
    let listed = store.list_outbox(None, 10).await.unwrap();
    assert!(
        !listed
            .iter()
            .any(|m| m.outbox_id == rolled.message.outbox_id),
        "rolled-back outbox row must not be visible"
    );
}

#[tokio::test]
#[ignore = "requires PG_TEST_URL or local postgres://localhost/awaken_test"]
async fn postgres_outbox_dedupe_retry_and_conflict() {
    let store = test_store(&unique_prefix("dedupe")).await;
    let mut one = draft(1);
    one.dedupe_key = Some("event-1/projector".into());
    let first = store.enqueue_outbox(one.clone()).await.unwrap().message;
    let second = store.enqueue_outbox(one).await.unwrap().message;
    assert_eq!(first.outbox_id, second.outbox_id);

    let mut changed = draft(2);
    changed.dedupe_key = Some("event-1/projector".into());
    let err = store.enqueue_outbox(changed).await.unwrap_err();
    assert!(matches!(
        err,
        awaken_server_contract::contract::outbox::OutboxError::Conflict(_)
    ));
}
