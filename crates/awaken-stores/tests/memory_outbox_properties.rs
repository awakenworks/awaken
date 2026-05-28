use std::collections::{BTreeMap, BTreeSet};

use awaken_server_contract::contract::outbox::{
    OUTBOX_LANE_CANONICAL, OUTBOX_TARGET_PROTOCOL_PROJECTOR, OutboxMessageDraft, OutboxStatus,
    OutboxStore,
};
use awaken_stores::InMemoryOutboxStore;
use proptest::prelude::*;

fn draft(payload: u8) -> OutboxMessageDraft {
    OutboxMessageDraft::new(
        OUTBOX_LANE_CANONICAL,
        OUTBOX_TARGET_PROTOCOL_PROJECTOR,
        serde_json::json!({ "value": payload }),
    )
    .expect("draft should validate")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn dedupe_replay_is_idempotent_for_duplicate_event_keys(
        keys in prop::collection::vec(0u8..24, 1..96)
    ) {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        runtime.block_on(async move {
            let store = InMemoryOutboxStore::new();
            let mut first_by_key = BTreeMap::new();

            for key in &keys {
                let mut draft = draft(*key);
                draft.dedupe_key = Some(format!("event-{key}/projector"));

                let first = store
                    .enqueue_outbox(draft.clone())
                    .await
                    .expect("first enqueue")
                    .message;
                let replay = store
                    .enqueue_outbox(draft)
                    .await
                    .expect("duplicate enqueue")
                    .message;

                prop_assert_eq!(
                    first.outbox_id.clone(),
                    replay.outbox_id,
                    "same dedupe input must return the original outbox row"
                );
                if let Some(existing) = first_by_key.insert(*key, first.outbox_id.clone()) {
                    prop_assert_eq!(
                        existing,
                        first.outbox_id,
                        "replayed duplicate key must converge to the first outbox id"
                    );
                }
            }

            let pending = store
                .list_outbox(Some(OutboxStatus::Pending), usize::MAX)
                .await
                .expect("list pending rows");
            let unique_keys = keys.into_iter().collect::<BTreeSet<_>>().len();
            prop_assert_eq!(
                pending.len(),
                unique_keys,
                "duplicate event keys must not create extra outbox rows"
            );
            Ok(())
        })?;
    }

    #[test]
    fn claim_order_is_stable_for_out_of_order_available_times(
        inputs in prop::collection::vec((0u8..64, 0u64..1_000), 1..96),
        now in 0u64..1_000,
    ) {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        runtime.block_on(async move {
            let store = InMemoryOutboxStore::new();
            for (payload, available_at) in &inputs {
                let mut draft = draft(*payload);
                draft.available_at = *available_at;
                store.enqueue_outbox(draft).await.expect("enqueue outbox row");
            }

            let mut expected = store
                .list_outbox(Some(OutboxStatus::Pending), usize::MAX)
                .await
                .expect("list pending before claim")
                .into_iter()
                .filter(|message| message.available_at <= now)
                .collect::<Vec<_>>();
            expected.sort_by_key(|message| {
                (
                    message.available_at,
                    message.created_at,
                    message.outbox_id.clone(),
                )
            });

            let claimed = store
                .claim_outbox(
                    OUTBOX_LANE_CANONICAL,
                    OUTBOX_TARGET_PROTOCOL_PROJECTOR,
                    inputs.len(),
                    5_000,
                    "property-worker",
                    now,
                )
                .await
                .expect("claim outbox rows");

            prop_assert_eq!(
                claimed.iter().map(|message| &message.outbox_id).collect::<Vec<_>>(),
                expected.iter().map(|message| &message.outbox_id).collect::<Vec<_>>(),
                "claims must be deterministic even when rows are enqueued out of availability order"
            );
            prop_assert!(
                claimed.iter().all(|message| message.status == OutboxStatus::Claimed
                    && message.claimed_by.as_deref() == Some("property-worker")
                    && message.attempt_count == 1
                    && message.lease_expires_at == Some(now.saturating_add(5_000))),
                "claimed rows must carry a valid lease and first-attempt metadata"
            );
            Ok(())
        })?;
    }
}
