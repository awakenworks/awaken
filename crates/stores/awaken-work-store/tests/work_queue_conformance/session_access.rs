//! The portable capability/metadata sequence formerly covered only PostgreSQL.
use awaken_session_contract::work_queue::{
    HeartbeatResult, LeaseHeartbeat, WorkMutationResult, WorkQueue,
};
use awaken_work_store::{LEASE_TTL_MS, test_session_access_digest};
use std::collections::BTreeMap;

/// Cause/effect sequence: queued healthcheck + two Sessions => FIFO/cap; an
/// active owner targeting queued work => precondition failure; wrong environment
/// => absent; metadata patch => visible value; stop => next Session claimable;
/// current token => exact identity; reclaim => old access fenced and new access
/// accepted; purge => empty. These are sequential state constraints rather than
/// independent Boolean combinations. Each transition checks its effect before
/// triggering the next, and all backends run this same sequence.
pub async fn capability_and_metadata_contract(q: &impl WorkQueue) {
    let hc = q.enqueue_healthcheck("env_a").await.expect("enqueue");
    let w1 = q
        .enqueue_session("env_a", &awaken_session_contract::SessionId::from("s1"))
        .await
        .expect("enqueue");
    let w2 = q
        .enqueue_session("env_a", &awaken_session_contract::SessionId::from("s2"))
        .await
        .expect("enqueue");
    // list is enqueue-ordered
    let ids: Vec<String> = q
        .list("env_a")
        .await
        .expect("list")
        .into_iter()
        .map(|w| w.id)
        .collect();
    assert_eq!(ids, vec![hc.clone(), w1.clone(), w2]);
    // claim leases the oldest + single-active cap
    assert_eq!(
        q.claim("env_a", "w", 0)
            .await
            .expect("claim query")
            .expect("lease")
            .id,
        hc
    );
    assert!(
        q.claim("env_a", "w", 0).await.expect("claim").is_none(),
        "single active lease"
    );
    // The active healthcheck owner cannot acknowledge another queued item.
    assert!(matches!(
        q.ack("env_a", &w1, "w").await.expect("ack query"),
        WorkMutationResult::PreconditionFailed
    ));
    assert!(matches!(
        q.heartbeat("env_a", &w1, "w", 0, LeaseHeartbeat::unconditional())
            .await
            .expect("heartbeat"),
        HeartbeatResult::PreconditionFailed
    ));
    assert!(
        q.get("env_b", &w1).await.expect("get").is_none(),
        "wrong env → none"
    );
    assert!(
        q.heartbeat("env_b", &w1, "w", 0, LeaseHeartbeat::unconditional())
            .await
            .expect("heartbeat")
            .is_not_found()
    );
    // stats reflect the active healthcheck and queued Session work.
    let st = q.stats("env_a", 0).await.expect("stats");
    assert!(st.pending >= 1 && st.workers_polling == 1);
    // metadata + stop + remove_env
    let patch = BTreeMap::from([("k".to_string(), Some("v".to_string()))]);
    assert_eq!(
        q.update_metadata("env_a", &w1, patch)
            .await
            .expect("update query")
            .expect("md")
            .metadata
            .get("k")
            .map(String::as_str),
        Some("v")
    );
    assert!(q.stop("env_a", &hc, "w").await.expect("stop").is_accepted());
    let session_claim = q
        .claim_with_session_access("env_a", "w", "w", 1, None, &test_session_access_digest(1))
        .await
        .expect("session claim")
        .expect("queued Session");
    let token = session_claim
        .session_access
        .as_ref()
        .expect("Session claim token");
    let access = q
        .authenticate_session_access(token, 1)
        .await
        .expect("token lookup")
        .expect("current token");
    assert_eq!(access.work_id, w1);
    assert_eq!(access.session_id, "s1");
    // Cause/effect graph for the capability parity row: C1 epoch
    // one expires; C2 the same Work is reclaimed under epoch two; C3 an
    // authenticated epoch-one mutation arrives late. Effects: E1 the old
    // proof is fenced, E2 the epoch-two proof remains live. Constraint K1
    // the adapter command + lease_epoch is the sole mutation authority. Decision
    // row PT1=C1+C2+C3->E1+E2.
    let replacement = q
        .claim_with_session_access(
            "env_a",
            "w-replacement",
            "w-replacement",
            LEASE_TTL_MS + 1,
            None,
            &test_session_access_digest(2),
        )
        .await
        .expect("replacement claim")
        .expect("expired Work reclaimed");
    let replacement_access = q
        .authenticate_session_access(
            replacement
                .session_access
                .as_ref()
                .expect("replacement token"),
            LEASE_TTL_MS + 1,
        )
        .await
        .expect("replacement lookup")
        .expect("replacement access");
    assert!(
        matches!(
            q.stop_with_session_access(&access).await.unwrap(),
            WorkMutationResult::PreconditionFailed
        ),
        "PT1/E1"
    );
    assert!(
        q.heartbeat_with_session_access(
            &replacement_access,
            LEASE_TTL_MS + 2,
            LeaseHeartbeat::unconditional(),
        )
        .await
        .unwrap()
        .into_receipt()
        .is_some(),
        "PT1/E2"
    );
    q.remove_env("env_a").await.unwrap();
    assert!(
        q.list("env_a").await.expect("list").is_empty(),
        "remove_env purges"
    );
}
