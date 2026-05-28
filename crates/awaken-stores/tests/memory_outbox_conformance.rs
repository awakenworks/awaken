use awaken_server_contract::contract::outbox::{
    OutboxMessageDraft, OutboxNackOutcome, OutboxStatus, OutboxStore,
};
use awaken_stores::InMemoryOutboxStore;

fn draft(payload: i64) -> OutboxMessageDraft {
    OutboxMessageDraft::new(
        "canonical",
        "projector",
        serde_json::json!({ "value": payload }),
    )
    .unwrap()
}

#[tokio::test]
async fn enqueue_claim_and_ack_delivery() {
    let store = InMemoryOutboxStore::new();
    let first = store.enqueue_outbox(draft(1)).await.unwrap().message;
    store.enqueue_outbox(draft(2)).await.unwrap();

    let claimed = store
        .claim_outbox("canonical", "projector", 1, 1_000, "worker-a", 10)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].outbox_id, first.outbox_id);
    assert_eq!(claimed[0].attempt_count, 1);

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
async fn claim_respects_lane_target_available_at_and_limit() {
    let store = InMemoryOutboxStore::new();
    let mut delayed = draft(1);
    delayed.available_at = 100;
    store.enqueue_outbox(delayed).await.unwrap();
    store
        .enqueue_outbox(
            OutboxMessageDraft::new("protocol_replay", "projector", serde_json::json!({})).unwrap(),
        )
        .await
        .unwrap();
    store.enqueue_outbox(draft(2)).await.unwrap();

    let claimed = store
        .claim_outbox("canonical", "projector", 10, 1_000, "worker-a", 50)
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].payload["value"], 2);
}

#[tokio::test]
async fn expired_claim_can_be_reclaimed_at_least_once() {
    let store = InMemoryOutboxStore::new();
    let message = store.enqueue_outbox(draft(1)).await.unwrap().message;
    let claimed = store
        .claim_outbox("canonical", "projector", 1, 10, "worker-a", 10)
        .await
        .unwrap();
    assert_eq!(claimed[0].outbox_id, message.outbox_id);

    let reclaimed = store
        .claim_outbox("canonical", "projector", 1, 10, "worker-b", 21)
        .await
        .unwrap();
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].attempt_count, 2);
    assert_eq!(reclaimed[0].claimed_by.as_deref(), Some("worker-b"));
}

#[tokio::test]
async fn nack_requeues_then_dead_letters_at_max_attempts() {
    let store = InMemoryOutboxStore::new();
    let mut draft = draft(1);
    draft.max_attempts = 2;
    let message = store.enqueue_outbox(draft).await.unwrap().message;

    let claimed = store
        .claim_outbox("canonical", "projector", 1, 10, "worker-a", 10)
        .await
        .unwrap();
    let token = claimed[0].claim_token.as_deref().unwrap();
    let outcome = store
        .nack_outbox(&message.outbox_id, token, "temporary", 50, 20)
        .await
        .unwrap();
    assert_eq!(outcome, OutboxNackOutcome::Requeued);

    let claimed = store
        .claim_outbox("canonical", "projector", 1, 10, "worker-a", 50)
        .await
        .unwrap();
    let token = claimed[0].claim_token.as_deref().unwrap();
    let outcome = store
        .nack_outbox(&message.outbox_id, token, "permanent", 60, 55)
        .await
        .unwrap();
    assert_eq!(outcome, OutboxNackOutcome::DeadLettered);

    let dead = store
        .list_outbox(Some(OutboxStatus::DeadLetter), 10)
        .await
        .unwrap();
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0].last_error.as_deref(), Some("permanent"));
}

#[tokio::test]
async fn dedupe_key_retries_return_original_and_conflicts_on_changed_payload() {
    let store = InMemoryOutboxStore::new();
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
