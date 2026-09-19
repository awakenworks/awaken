//! Trait-generic conformance suite for the `WorkQueue` port (ADR-0081 §7).
//!
//! Every backend — the in-memory reference, SQLite, PostgreSQL, and any closed
//! substrate — runs [`run_suite`] with its own `fresh_with` factory and its own
//! lease TTL. A divergence here is a parity bug: the single-active-lease cap,
//! reclaim-at-TTL, environment isolation, stop-frees-next, remove_env purge,
//! Session ownership fencing, and injected-clock observation are the port's
//! observable contract, not one backend's behaviour.

use awaken_agent_contract::clock::utc::epoch_millis_to_rfc3339;
use awaken_agent_contract::clock::{Clock, ManualClock, system_clock};
use awaken_session_contract::SessionId;
use awaken_session_contract::work_queue::{
    HeartbeatCondition, HeartbeatResult, LeaseHeartbeat, WorkMutationResult, WorkQueue, WorkState,
};
use std::sync::Arc;

/// Fixture digest for Session access assertions: a deterministic 64-hex value.
#[must_use]
pub fn session_access_digest(
    seed: u64,
) -> awaken_session_contract::work_queue::SessionAccessDigest {
    awaken_session_contract::work_queue::SessionAccessDigest::from_hex(format!("{seed:064x}"))
        .expect("fixture digest is hex")
}

// ── The port contract, trait-generic over any WorkQueue backend ──────────────────

/// The single-active-lease cap: with items queued, repeated claims at one instant hand
/// out exactly ONE lease, and exactly one item is Active.
pub async fn single_active_cap<Q: WorkQueue>(q: &Q) {
    for i in 0..5 {
        q.enqueue_session("env", &SessionId::from(format!("s{i}")))
            .await
            .expect("enqueue");
    }
    let handed = {
        let mut n = 0;
        for _ in 0..4 {
            if q.claim("env", "w", 0).await.expect("claim").is_some() {
                n += 1;
            }
        }
        n
    };
    assert_eq!(handed, 1, "more than one lease handed out at once");
    let active = q
        .list("env")
        .await
        .expect("list")
        .into_iter()
        .filter(|w| w.state == WorkState::Active)
        .count();
    assert_eq!(active, 1, "exactly one item must be Active");
}

/// Reclaim is exact at the TTL boundary: a live lease caps the env until `now == expiry`,
/// then the lapsed item is reclaimable.
pub async fn reclaim_at_ttl<Q: WorkQueue>(q: &Q, lease_ttl_ms: u64) {
    q.enqueue_session("env", &SessionId::from("s0"))
        .await
        .expect("enqueue");
    assert!(
        q.claim("env", "a", 0).await.expect("claim").is_some(),
        "first claim leases"
    );
    assert!(
        q.claim("env", "b", lease_ttl_ms - 1)
            .await
            .expect("claim")
            .is_none(),
        "held until the last ms before expiry"
    );
    assert!(
        q.claim("env", "b", lease_ttl_ms)
            .await
            .expect("claim")
            .is_some(),
        "reclaimable at exactly the TTL boundary"
    );
}

/// Requested reclaim age is measured from the last lease refresh, independently
/// of the TTL selected by that heartbeat.
pub async fn requested_reclaim_uses_refresh_clock<Q: WorkQueue>(q: &Q) {
    let id = q
        .enqueue_session("env", &SessionId::from("s0"))
        .await
        .expect("enqueue");
    q.claim("env", "worker-a", 0)
        .await
        .expect("claim query")
        .expect("initial claim");
    assert!(matches!(
        q.heartbeat(
            "env",
            &id,
            "worker-a",
            1_000,
            LeaseHeartbeat {
                condition: HeartbeatCondition::First,
                desired_ttl_seconds: Some(120),
            },
        )
        .await
        .expect("heartbeat"),
        HeartbeatResult::Accepted(_)
    ));
    assert!(
        q.claim_with_reclaim("env", "worker-b", "worker-b", 1_001, Some(2))
            .await
            .expect("claim")
            .is_none(),
        "a one-millisecond-old refresh is younger than the requested age"
    );
    assert!(
        q.claim_with_reclaim("env", "worker-b", "worker-b", 1_002, Some(2))
            .await
            .expect("claim")
            .is_some(),
        "reclaim age is exact even when the live lease has a 120-second ttl"
    );
}

/// Environments lease independently: a claim in one never touches another's cap.
pub async fn env_isolation<Q: WorkQueue>(q: &Q) {
    q.enqueue_session("env_a", &SessionId::from("a0"))
        .await
        .expect("enqueue a");
    q.enqueue_session("env_b", &SessionId::from("b0"))
        .await
        .expect("enqueue b");
    let la = q
        .claim("env_a", "w", 0)
        .await
        .expect("claim query")
        .expect("env_a leases");
    let lb = q
        .claim("env_b", "w", 0)
        .await
        .expect("claim query")
        .expect("env_b leases independently");
    assert_eq!(
        q.get("env_a", &la.id)
            .await
            .expect("get")
            .map(|w| w.environment_id),
        Some("env_a".into())
    );
    assert_eq!(
        q.get("env_b", &lb.id)
            .await
            .expect("get")
            .map(|w| w.environment_id),
        Some("env_b".into())
    );
}

/// Stopping the active item frees the next, and no item is lost or duplicated.
pub async fn stop_frees_next<Q: WorkQueue>(q: &Q) {
    for i in 0..3 {
        q.enqueue_session("env", &SessionId::from(format!("s{i}")))
            .await
            .expect("enqueue");
    }
    let first = q
        .claim("env", "w", 0)
        .await
        .expect("claim query")
        .expect("first");
    assert!(
        q.claim("env", "w", 0).await.expect("claim").is_none(),
        "capped while active"
    );
    q.stop("env", &first.id, "w").await.expect("stop");
    assert!(
        q.claim("env", "w", 0).await.expect("claim").is_some(),
        "next claimable after stop"
    );
    assert_eq!(
        q.list("env").await.expect("list").len(),
        3,
        "item count conserved"
    );
}

/// remove_env purges everything: no items, nothing to claim.
pub async fn remove_env_purges<Q: WorkQueue>(q: &Q) {
    for i in 0..3 {
        q.enqueue_session("env", &SessionId::from(format!("s{i}")))
            .await
            .expect("enqueue");
    }
    q.remove_env("env").await.unwrap();
    assert!(
        q.list("env").await.expect("list").is_empty(),
        "list empty after remove_env"
    );
    assert!(
        q.claim("env", "w", 0).await.expect("claim").is_none(),
        "nothing to claim after remove_env"
    );
}

/// Session dispatch is a durable projection: exact replay converges to one row,
/// while either identity coordinate changing creates independent work.
pub async fn session_enqueue_is_idempotent<Q: WorkQueue>(q: &Q) {
    // Cause/effect graph: C1 same Environment; C2 same Session; C3 concurrent or
    // sequential replay. Effects: E1 C1+C2 always returns one canonical work id;
    // E2 changing either coordinate produces a distinct item. Constraints: a
    // WorkQueue key is exactly (environment_id, session_id).
    //
    // | Rule | environment | session | replay | identity / row count |
    // | D1 | same | same | yes | same id / one row |
    // | D2 | same | different | no | distinct id / two rows |
    // | D3 | different | same | no | distinct id in other queue |
    let first = q
        .enqueue_session("env", &SessionId::from("session-a"))
        .await
        .expect("D1");
    let replay = q
        .enqueue_session("env", &SessionId::from("session-a"))
        .await
        .expect("D1");
    assert_eq!(replay, first, "D1 canonical identity");
    assert_eq!(q.list("env").await.expect("D1 list").len(), 1, "D1");

    let other_session = q
        .enqueue_session("env", &SessionId::from("session-b"))
        .await
        .expect("D2");
    assert_ne!(other_session, first, "D2");
    assert_eq!(q.list("env").await.expect("D2 list").len(), 2, "D2");

    let other_env = q
        .enqueue_session("env-b", &SessionId::from("session-a"))
        .await
        .expect("D3");
    assert_ne!(other_env, first, "D3");
    assert_eq!(q.list("env-b").await.expect("D3 list").len(), 1, "D3");
}

/// The official heartbeat compare token is an atomic CAS: first succeeds once,
/// the returned token advances monotonically, and a stale token cannot extend
/// the lease after a newer heartbeat has committed.
pub async fn heartbeat_compare_and_extend<Q: WorkQueue>(q: &Q) {
    let id = q
        .enqueue_session("env", &SessionId::from("s0"))
        .await
        .expect("enqueue");
    q.claim("env", "worker", 0)
        .await
        .expect("claim query")
        .expect("claim");

    assert!(matches!(
        q.heartbeat(
            "env",
            &id,
            "other-worker",
            1,
            LeaseHeartbeat {
                condition: HeartbeatCondition::First,
                desired_ttl_seconds: None,
            },
        )
        .await
        .expect("heartbeat"),
        HeartbeatResult::PreconditionFailed
    ));

    let first = match q
        .heartbeat(
            "env",
            &id,
            "worker",
            1,
            LeaseHeartbeat {
                condition: HeartbeatCondition::First,
                desired_ttl_seconds: Some(7),
            },
        )
        .await
        .expect("heartbeat")
    {
        HeartbeatResult::Accepted(receipt) => receipt,
        other => panic!("first heartbeat rejected: {other:?}"),
    };
    assert!(first.lease_extended);
    assert_eq!(first.ttl_seconds, 7);

    assert!(matches!(
        q.heartbeat(
            "env",
            &id,
            "worker",
            2,
            LeaseHeartbeat {
                condition: HeartbeatCondition::First,
                desired_ttl_seconds: None,
            },
        )
        .await
        .expect("heartbeat"),
        HeartbeatResult::PreconditionFailed
    ));

    let second = q
        .heartbeat(
            "env",
            &id,
            "worker",
            2,
            LeaseHeartbeat {
                condition: HeartbeatCondition::Matching(first.last_heartbeat.clone()),
                desired_ttl_seconds: None,
            },
        )
        .await
        .expect("heartbeat")
        .into_receipt()
        .expect("matching heartbeat");
    assert_ne!(second.last_heartbeat, first.last_heartbeat);

    assert!(matches!(
        q.heartbeat(
            "env",
            &id,
            "worker",
            3,
            LeaseHeartbeat {
                condition: HeartbeatCondition::Matching(first.last_heartbeat),
                desired_ttl_seconds: None,
            },
        )
        .await
        .expect("heartbeat"),
        HeartbeatResult::PreconditionFailed
    ));
}

/// Session ownership FMECA/cause-effect graph. Causes: C1 exact Session is
/// queued/stopped/active; C2 caller is current owner/other owner; C3 lease is
/// live/expired; C4 trigger is reconciliation enqueue, driving-event wake, or
/// exact Worker release; C5 receipt identity (Work/Environment/Session), owner,
/// and epoch each match or exactly one is altered. Effects: E1 one monotonic
/// lease; E2 stale mutations fail closed; E3 reconciliation never resurrects completed Work; E4 driving event
/// explicitly wakes it; E5 only the exact current epoch releases, and replay is
/// idempotent.
///
/// | Rule | State | Owner | Trigger | Effect |
/// |---|---|---|---|---|
/// | L1 | queued | A | acquire | E1 epoch 1 |
/// | L2 | active/live | A | acquire | E1 renew, same epoch |
/// | L3 | active/live | B | acquire/ack/stop | E2 |
/// | L4 | stopped | n/a | enqueue/acquire | E3 |
/// | L5 | stopped | A again | wake+acquire | E4, epoch 2 |
/// | L6 | active epoch 2 | stale epoch 1 | release | E2 |
/// | L7 | active epoch 2 | one C5 field mismatches | release | E2, all authority unchanged |
/// | L8 | active epoch 2 | all C5 fields match | release | E5, clear authority |
/// | L9 | stopped epoch 2 | same identity/epoch, exact or other owner | release | E5, no effect |
///
/// L7 varies one field at a time so each production predicate must independently
/// reject; checking the complete lease and Work projection detects a false
/// return after a partial mutation. L9 is an accepted replay, not a new owner-
/// authorized effect: the stopped row intentionally no longer stores its owner.
pub async fn session_ownership_lifecycle_is_single_and_fenced<Q: WorkQueue>(q: &Q) {
    let id = q
        .enqueue_session("env", &SessionId::from("session"))
        .await
        .expect("L1");
    let first = q
        .acquire_session("env", &SessionId::from("session"), "owner-a", 0)
        .await
        .expect("L1")
        .expect("L1 lease");
    assert_eq!(
        (first.work_id.as_str(), first.epoch),
        (id.as_str(), 1),
        "L1/E1"
    );

    let renewed = q
        .acquire_session("env", &SessionId::from("session"), "owner-a", 10)
        .await
        .expect("L2")
        .expect("L2 lease");
    assert_eq!(renewed.epoch, first.epoch, "L2/E1");
    assert!(
        renewed.expires_at_unix_ms > first.expires_at_unix_ms,
        "L2/E1"
    );
    let fenced = q
        .acquire_session("env", &SessionId::from("session"), "owner-b", 11)
        .await
        .expect("L3")
        .expect("L3 returns the current lease for typed fencing");
    assert_eq!(fenced.owner, "owner-a", "L3/E2");
    assert!(matches!(
        q.ack("env", &id, "owner-b").await.expect("L3 ack"),
        WorkMutationResult::PreconditionFailed
    ));
    assert!(
        q.ack("env", &id, "owner-a")
            .await
            .expect("L2 ack")
            .is_accepted()
    );
    assert!(matches!(
        q.stop("env", &id, "owner-b").await.expect("L3 stop"),
        WorkMutationResult::PreconditionFailed
    ));
    assert!(
        q.stop("env", &id, "owner-a")
            .await
            .expect("L4 stop")
            .is_accepted()
    );

    assert_eq!(
        q.enqueue_session("env", &SessionId::from("session"))
            .await
            .expect("L4"),
        id
    );
    assert_eq!(
        q.get("env", &id).await.unwrap().unwrap().state,
        WorkState::Stopped,
        "L4/E3"
    );
    assert!(
        q.acquire_session("env", &SessionId::from("session"), "owner-b", 20)
            .await
            .expect("L4")
            .is_none(),
        "L4/E3 exact acquire cannot bypass explicit wake"
    );
    assert_eq!(
        q.wake_session("env", &SessionId::from("session"))
            .await
            .expect("L5"),
        id
    );
    let replacement = q
        .acquire_session("env", &SessionId::from("session"), "owner-a", 20)
        .await
        .expect("L5")
        .expect("L5 lease");
    assert_eq!(replacement.epoch, 2, "L5/E4");
    assert!(
        !q.release_session(&first).await.expect("L6 stale release"),
        "L6/E2"
    );
    assert_eq!(
        q.get("env", &id).await.unwrap().unwrap().state,
        WorkState::Active,
        "L6/E2 successor remains authoritative"
    );
    let projection = |work: &awaken_session_contract::work_queue::WorkItem| {
        (
            work.id.clone(),
            work.environment_id.clone(),
            work.data.clone(),
            work.metadata.clone(),
            work.state,
            work.acknowledged_at.clone(),
            work.latest_heartbeat_at.clone(),
            work.started_at.clone(),
            work.stop_requested_at.clone(),
            work.stopped_at.clone(),
        )
    };
    let active = q.get("env", &id).await.unwrap().unwrap();
    let mut wrong_owner = replacement.clone();
    wrong_owner.owner = "owner-b".into();
    let mut wrong_epoch = replacement.clone();
    wrong_epoch.epoch = first.epoch;
    let mut wrong_work = replacement.clone();
    wrong_work.work_id = "absent-work".into();
    let mut wrong_environment = replacement.clone();
    wrong_environment.environment_id = "other-env".into();
    let mut wrong_session = replacement.clone();
    wrong_session.session_id = "other-session".into();
    for (field, receipt) in [
        ("owner", wrong_owner.clone()),
        ("epoch", wrong_epoch),
        ("work_id", wrong_work),
        ("environment_id", wrong_environment),
        ("session_id", wrong_session),
    ] {
        assert!(
            !q.release_session(&receipt).await.expect("L7 release"),
            "L7/E2 mismatched {field} must reject"
        );
        let after = q.get("env", &id).await.unwrap().unwrap();
        assert_eq!(
            projection(&after),
            projection(&active),
            "L7/E2 mismatched {field} must preserve the complete Work projection"
        );
        // The existing acquire contract returns another owner's current lease
        // without renewing it; this observes owner/epoch/expiry, not a repair.
        let current = q
            .acquire_session("env", &SessionId::from("session"), "observer", 20)
            .await
            .expect("L7 read current authority");
        assert_eq!(current.as_ref(), Some(&replacement), "L7/E2 {field}");
    }
    assert!(
        q.release_session(&replacement)
            .await
            .expect("L8 exact release"),
        "L8/E5"
    );
    let stopped = q.get("env", &id).await.unwrap().unwrap();
    assert_eq!(stopped.state, WorkState::Stopped, "L8/E5");
    for receipt in [&replacement, &wrong_owner] {
        assert!(
            q.release_session(receipt).await.expect("L9 replay"),
            "L9/E5 same-epoch stopped replay does not require the former owner"
        );
        let after = q.get("env", &id).await.unwrap().unwrap();
        assert_eq!(projection(&after), projection(&stopped), "L9/E5");
        assert!(
            q.acquire_session("env", &SessionId::from("session"), "observer", 20)
                .await
                .expect("L9 no authority")
                .is_none(),
            "L9/E5 replay must not recreate authority"
        );
    }
}

/// Graceful Worker teardown cause/effect graph: C1 exact incarnation owns active
/// Session Work in two Environments; C2 another incarnation owns a third; C3 the
/// exact owner loses authority. Effects: E1 all and only C1 rows return to the
/// queue atomically per backend; E2 C2 remains active; E3 the same Sessions become
/// claimable by a fresh incarnation before later queued work. Decision
/// rules: G1=C1+C3->E1+E3, G2=C2+C3->E2. FMECA: retaining a graceful predecessor's
/// 60-second lease stalls every restart; permanently stopping a non-terminal
/// Session makes recovery impossible; releasing by reusable worker id would revoke
/// a replacement generation. Exact incarnation requeue avoids all three.
pub async fn graceful_owner_release_is_exact<Q: WorkQueue>(q: &Q) {
    let first = q
        .enqueue_session("env-a", &SessionId::from("first"))
        .await
        .expect("G1");
    q.enqueue_session("env-a", &SessionId::from("next"))
        .await
        .expect("G1 successor");
    let second = q
        .enqueue_session("env-b", &SessionId::from("second"))
        .await
        .expect("G1");
    let unrelated = q
        .enqueue_session("env-c", &SessionId::from("other"))
        .await
        .expect("G2");
    q.claim("env-a", "worker:1:old", 0).await.unwrap().unwrap();
    q.claim("env-b", "worker:1:old", 0).await.unwrap().unwrap();
    q.claim("env-c", "worker:2:new", 0).await.unwrap().unwrap();

    assert_eq!(
        q.release_owner("worker:1:old").await.expect("G1"),
        2,
        "G1/E1"
    );
    assert_eq!(
        q.get("env-a", &first).await.unwrap().unwrap().state,
        WorkState::Queued,
        "G1/E1"
    );
    assert_eq!(
        q.get("env-b", &second).await.unwrap().unwrap().state,
        WorkState::Queued,
        "G1/E1"
    );
    assert_eq!(
        q.get("env-c", &unrelated).await.unwrap().unwrap().state,
        WorkState::Active,
        "G2/E2"
    );
    assert!(
        q.claim("env-a", "worker:2:new", 1)
            .await
            .unwrap()
            .is_some_and(|work| work.id == first),
        "G1/E3"
    );
    assert_eq!(
        q.release_owner("worker:1:old").await.expect("G1 replay"),
        0,
        "G1 idempotent"
    );
}

pub async fn reclaim_age_boundary_cases(q: &impl WorkQueue) {
    // Adapter decision table: live TTL throughout; absent age or age > now
    // must retain the current lease. age == now selects refresh zero;
    // after re-claim at time one, age 1000 at time 1001 selects refresh one.
    // These distinguish None from Some(0), catch an omitted shared cutoff,
    // and assert the same Work identity survives each accepted re-claim.
    let environment = "env_reclaim_cutoff";
    let id = q
        .enqueue_session(environment, &SessionId::from("s1"))
        .await
        .unwrap();
    assert_eq!(q.claim(environment, "a", 0).await.unwrap().unwrap().id, id);
    for (now, age, expected) in [
        (1, None, false),
        (1, Some(2), false),
        (1, Some(1), true),
        (1_001, Some(1_000), true),
    ] {
        let actual = q
            .claim_with_reclaim(environment, "b", "b", now, age)
            .await
            .unwrap();
        assert_eq!(
            actual.is_some(),
            expected,
            "refresh cutoff row {now}/{age:?}"
        );
        if let Some(item) = actual {
            assert_eq!(item.id, id);
            assert_eq!(item.state, WorkState::Active);
        }
    }
    q.remove_env(environment).await.unwrap();
}

/// Every observation timestamp a backend writes is a reading of the clock it was
/// built with — never a constant, and never the host's wall clock.
///
/// Decision table. Causes: C1 the queue accepts an item; C2 a claim carries its
/// own sampled `now_ms`; C3 an acknowledgement, which carries none; C4 a stop,
/// which carries none; C5 the store is asked for queue stats; C6 the queue is
/// drained. Effects: E1 `created_at` is the injected reading at acceptance;
/// E2 `started_at` is the claim's own `now_ms`, so the durable start and the
/// lease window cannot disagree; E3/E4 the acknowledgement and stop instants are
/// the injected reading *at that moment*, which is how advancing the clock
/// between commands is observable; E5 `oldest_queued_at` is the oldest
/// unfinished item's own `created_at`; E6 a drained queue reports absence rather
/// than a stand-in. Rules R1..R6 pair C1..C6 with E1..E6.
///
/// Diffusion: the sibling objects in the same store — the Environment's
/// healthcheck item and a second Session item accepted later — are checked too,
/// so a fix that reached only the one converted path fails here.
pub async fn observation_timestamps_are_the_injected_clock<Q: WorkQueue>(
    q: &Q,
    clock: &ManualClock,
) {
    const ACCEPTED: u64 = 1_700_000_000_000;
    const LATER: u64 = ACCEPTED + 5_000;
    const CLAIMED: u64 = ACCEPTED + 9_000;
    const ACKED: u64 = ACCEPTED + 11_000;
    const STOPPED: u64 = ACCEPTED + 17_000;

    clock.set(ACCEPTED);
    let first = SessionId::from("obs-first");
    let work_id = q.enqueue_session("obs-env", &first).await.expect("R1");
    q.enqueue_healthcheck("obs-env").await.expect("R1 sibling");
    clock.set(LATER);
    let second = SessionId::from("obs-second");
    q.enqueue_session("obs-env", &second)
        .await
        .expect("R1 later");

    let listed = q.list("obs-env").await.expect("R1 list");
    assert_eq!(listed.len(), 3, "R1 fixture");
    for item in &listed {
        let expected = match &item.data {
            awaken_session_contract::work_queue::WorkPayload::Session { id } if id == &second => {
                LATER
            }
            _ => ACCEPTED,
        };
        assert_eq!(
            item.created_at,
            epoch_millis_to_rfc3339(expected),
            "R1 {:?} carries the reading at its own acceptance",
            item.data
        );
    }

    assert_eq!(
        q.stats("obs-env", CLAIMED)
            .await
            .expect("R5")
            .oldest_queued_at,
        Some(epoch_millis_to_rfc3339(ACCEPTED)),
        "R5 oldest unfinished item's own acceptance instant"
    );

    // R2: the claim samples its own instant; the clock is deliberately parked
    // somewhere else so a backend that reached for the ambient reading is caught.
    clock.set(ACCEPTED);
    let claimed = q
        .claim("obs-env", "obs-worker", CLAIMED)
        .await
        .expect("R2")
        .expect("R2 lease");
    assert_eq!(claimed.id, work_id, "R2 oldest first");
    assert_eq!(
        claimed.started_at,
        Some(epoch_millis_to_rfc3339(CLAIMED)),
        "R2 started_at is the claim's own now_ms"
    );
    assert_eq!(
        claimed.created_at,
        epoch_millis_to_rfc3339(ACCEPTED),
        "R2 acceptance is not rewritten by a claim"
    );

    clock.set(ACKED);
    let acked = q.ack("obs-env", &work_id, "obs-worker").await.expect("R3");
    let WorkMutationResult::Accepted(acked) = acked else {
        panic!("R3 acknowledgement accepted");
    };
    assert_eq!(
        acked.acknowledged_at,
        Some(epoch_millis_to_rfc3339(ACKED)),
        "R3 acknowledged_at is the injected reading at acknowledgement"
    );

    clock.set(STOPPED);
    let stopped = q.stop("obs-env", &work_id, "obs-worker").await.expect("R4");
    let WorkMutationResult::Accepted(stopped) = stopped else {
        panic!("R4 stop accepted");
    };
    assert_eq!(
        stopped.stopped_at,
        Some(epoch_millis_to_rfc3339(STOPPED)),
        "R4 stopped_at is the injected reading at stop"
    );
    assert_eq!(
        stopped.stop_requested_at, stopped.stopped_at,
        "R4 one reading, two fields"
    );
    assert_ne!(
        stopped.acknowledged_at, stopped.stopped_at,
        "R3/R4 advancing the clock between commands is observable"
    );

    // R6: drain everything; absence is then the answer, not a stand-in value.
    for item in q.list("obs-env").await.expect("R6 list") {
        if item.state != WorkState::Stopped {
            q.claim("obs-env", "obs-worker", STOPPED)
                .await
                .expect("R6 claim");
            let _ = q
                .stop("obs-env", &item.id, "obs-worker")
                .await
                .expect("R6 stop");
        }
    }
    let drained = q.stats("obs-env", STOPPED).await.expect("R6");
    assert_eq!(drained.depth, 0, "R6 drained");
    assert_eq!(drained.pending, 0, "R6 drained");
    assert_eq!(drained.oldest_queued_at, None, "R6 absence is stated");
    q.remove_env("obs-env").await.expect("R6 cleanup");
}

/// Run the whole contract against a freshly-built backend `Q`. The factory takes
/// the clock so the observation rules can name the instants they expect.
pub async fn run_suite<Q: WorkQueue, F: std::future::Future<Output = Q>>(
    fresh_with: impl Fn(Arc<dyn Clock>) -> F,
    lease_ttl_ms: u64,
) {
    let fresh = || fresh_with(system_clock());
    single_active_cap(&fresh().await).await;
    reclaim_at_ttl(&fresh().await, lease_ttl_ms).await;
    requested_reclaim_uses_refresh_clock(&fresh().await).await;
    env_isolation(&fresh().await).await;
    stop_frees_next(&fresh().await).await;
    remove_env_purges(&fresh().await).await;
    session_enqueue_is_idempotent(&fresh().await).await;
    heartbeat_compare_and_extend(&fresh().await).await;
    session_ownership_lifecycle_is_single_and_fenced(&fresh().await).await;
    graceful_owner_release_is_exact(&fresh().await).await;
    reclaim_age_boundary_cases(&fresh().await).await;
    session_access::capability_and_metadata_contract(&fresh().await, lease_ttl_ms).await;
    let clock = Arc::new(ManualClock::new(0));
    let queue = fresh_with(clock.clone()).await;
    observation_timestamps_are_the_injected_clock(&queue, &clock).await;
}

// ── Backend rows: each must pass the identical suite ─────────────────────────────

pub mod session_access {
    //! The portable capability/metadata sequence formerly covered only PostgreSQL.
    use super::session_access_digest;
    use awaken_session_contract::work_queue::{
        HeartbeatResult, LeaseHeartbeat, WorkMutationResult, WorkQueue,
    };
    use std::collections::BTreeMap;

    /// Cause/effect sequence: queued healthcheck + two Sessions => FIFO/cap; an
    /// active owner targeting queued work => precondition failure; wrong environment
    /// => absent; metadata patch => visible value; stop => next Session claimable;
    /// current token => exact identity; reclaim => old access fenced and new access
    /// accepted; purge => empty. These are sequential state constraints rather than
    /// independent Boolean combinations. Each transition checks its effect before
    /// triggering the next, and all backends run this same sequence.
    pub async fn capability_and_metadata_contract(q: &impl WorkQueue, lease_ttl_ms: u64) {
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
            .claim_with_session_access("env_a", "w", "w", 1, None, &session_access_digest(1))
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
                lease_ttl_ms + 1,
                None,
                &session_access_digest(2),
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
                lease_ttl_ms + 1,
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
                lease_ttl_ms + 2,
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
}
