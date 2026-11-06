// @formal surface exclude=test_support_crate
// @formal owner crates/platform/environment/awaken-work-store/tests/work_queue_conformance.rs
// @formal reason: Trait-generic WorkQueue conformance suite (ADR-0081 §7). It is a tooling-only test
// @formal reason: library every backend's test target calls; no product crate links or invokes it, and it
// @formal reason: admits nothing at runtime.
//! Trait-generic conformance suite for the `WorkQueue` port (ADR-0081 §7).
//!
//! Every backend — the in-memory reference, SQLite, PostgreSQL, and any closed
//! substrate — runs [`run_suite`] with its own `fresh_with` factory and its own
//! lease TTL. A divergence here is a parity bug: the single-active-lease cap,
//! reclaim-at-TTL, environment isolation, stop-frees-next, remove_env purge,
//! Session ownership fencing, injected-clock observation and the one-clock
//! lease law (every lease granted, renewed and judged on the store's own clock)
//! are the port's observable contract, not one backend's behaviour.

use awaken_environment_contract::EnvironmentRevision;
use awaken_executable_environment_contract::{
    EnvironmentWorkLifecycle, EnvironmentWorkRevisionError,
    EnvironmentWorkRevisionOperation as RevisionOperation,
    EnvironmentWorkRevisionOutcome as RevisionOutcome, HeartbeatCondition, HeartbeatResult,
    LeaseHeartbeat, SessionAccessAuthenticator, SessionWorkLeaseControl, SessionWorkStore,
    SessionWorkerLeaseStore, WorkConsumer, WorkMetadataPatch, WorkMutationResult, WorkQueries,
    WorkState, WorkerLeaseObservation,
};
use awaken_lease_testkit::ManualClock;
use awaken_session_types::SessionId;
use awaken_time::WallClock;
use awaken_time::utc::epoch_millis_to_rfc3339;
use std::{collections::BTreeMap, sync::Arc};

/// Fixture digest for Session access assertions: a deterministic 64-hex value.
#[must_use]
pub fn session_access_digest(
    seed: u64,
) -> awaken_executable_environment_contract::SessionAccessDigest {
    awaken_executable_environment_contract::SessionAccessDigest::from_hex(format!("{seed:064x}"))
        .expect("fixture digest is hex")
}

// ── The port contract, trait-generic over any WorkQueue backend ──────────────────

/// The single-active-lease cap: with items queued, repeated claims at one instant hand
/// out exactly ONE lease, and exactly one item is Active.
pub async fn single_active_cap<Q: WorkBackend>(q: &Q, clock: &ManualClock) {
    for i in 0..5 {
        q.enqueue_session("env", &SessionId::from(format!("s{i}")))
            .await
            .expect("enqueue");
    }
    let handed = {
        let mut n = 0;
        for _ in 0..4 {
            clock.set(0);
            if q.claim("env", "w").await.expect("claim").is_some() {
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
pub async fn reclaim_at_ttl<Q: WorkBackend>(q: &Q, clock: &ManualClock, lease_ttl_ms: u64) {
    q.enqueue_session("env", &SessionId::from("s0"))
        .await
        .expect("enqueue");
    assert!(
        q.claim("env", "a").await.expect("claim").is_some(),
        "first claim leases"
    );
    clock.set(lease_ttl_ms - 1);
    assert!(
        q.claim("env", "b").await.expect("claim").is_none(),
        "held until the last ms before expiry"
    );
    clock.set(lease_ttl_ms);
    assert!(
        q.claim("env", "b").await.expect("claim").is_some(),
        "reclaimable at exactly the TTL boundary"
    );
}

/// Requested reclaim age is measured from the last lease refresh, independently
/// of the TTL selected by that heartbeat.
pub async fn requested_reclaim_uses_refresh_clock<Q: WorkBackend>(q: &Q, clock: &ManualClock) {
    let id = q
        .enqueue_session("env", &SessionId::from("s0"))
        .await
        .expect("enqueue");
    q.claim("env", "worker-a")
        .await
        .expect("claim query")
        .expect("initial claim");
    clock.set(1_000);
    assert!(matches!(
        q.heartbeat(
            "env",
            &id,
            "worker-a",
            LeaseHeartbeat {
                condition: HeartbeatCondition::First,
                desired_ttl_seconds: Some(120),
            },
        )
        .await
        .expect("heartbeat"),
        HeartbeatResult::Accepted(_)
    ));
    clock.set(1_001);
    assert!(
        q.claim_with_reclaim("env", "worker-b", "worker-b", Some(2))
            .await
            .expect("claim")
            .is_none(),
        "a one-millisecond-old refresh is younger than the requested age"
    );
    clock.set(1_002);
    assert!(
        q.claim_with_reclaim("env", "worker-b", "worker-b", Some(2))
            .await
            .expect("claim")
            .is_some(),
        "reclaim age is exact even when the live lease has a 120-second ttl"
    );
}

/// Environments lease independently: a claim in one never touches another's cap.
pub async fn env_isolation<Q: WorkBackend>(q: &Q) {
    q.enqueue_session("env_a", &SessionId::from("a0"))
        .await
        .expect("enqueue a");
    q.enqueue_session("env_b", &SessionId::from("b0"))
        .await
        .expect("enqueue b");
    let la = q
        .claim("env_a", "w")
        .await
        .expect("claim query")
        .expect("env_a leases");
    let lb = q
        .claim("env_b", "w")
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
pub async fn stop_frees_next<Q: WorkBackend>(q: &Q) {
    for i in 0..3 {
        q.enqueue_session("env", &SessionId::from(format!("s{i}")))
            .await
            .expect("enqueue");
    }
    let first = q
        .claim("env", "w")
        .await
        .expect("claim query")
        .expect("first");
    assert!(
        q.claim("env", "w").await.expect("claim").is_none(),
        "capped while active"
    );
    q.stop("env", &first.id, "w").await.expect("stop");
    assert!(
        q.claim("env", "w").await.expect("claim").is_some(),
        "next claimable after stop"
    );
    assert_eq!(
        q.list("env").await.expect("list").len(),
        3,
        "item count conserved"
    );
}

/// remove_env purges everything: no items, nothing to claim.
pub async fn remove_env_purges<Q: WorkBackend>(q: &Q) {
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
        q.claim("env", "w").await.expect("claim").is_none(),
        "nothing to claim after remove_env"
    );
}

/// Session dispatch is a durable projection: exact replay converges to one row,
/// while either identity coordinate changing creates independent work.
pub async fn session_enqueue_is_idempotent<Q: WorkBackend>(q: &Q) {
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
pub async fn heartbeat_compare_and_extend<Q: WorkBackend>(q: &Q, clock: &ManualClock) {
    let id = q
        .enqueue_session("env", &SessionId::from("s0"))
        .await
        .expect("enqueue");
    q.claim("env", "worker")
        .await
        .expect("claim query")
        .expect("claim");

    clock.set(1);
    assert!(matches!(
        q.heartbeat(
            "env",
            &id,
            "other-worker",
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

    clock.set(2);
    assert!(matches!(
        q.heartbeat(
            "env",
            &id,
            "worker",
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

    clock.set(3);
    assert!(matches!(
        q.heartbeat(
            "env",
            &id,
            "worker",
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
pub async fn session_ownership_lifecycle_is_single_and_fenced<Q: WorkBackend>(
    q: &Q,
    clock: &ManualClock,
) {
    let id = q
        .enqueue_session("env", &SessionId::from("session"))
        .await
        .expect("L1");
    let first = q
        .acquire_session("env", &SessionId::from("session"), "owner-a")
        .await
        .expect("L1")
        .expect("L1 lease");
    assert_eq!((first.work_id(), first.epoch()), (id.as_str(), 1), "L1/E1");

    clock.set(10);
    let renewed = q
        .acquire_session("env", &SessionId::from("session"), "owner-a")
        .await
        .expect("L2")
        .expect("L2 lease");
    assert_eq!(renewed.epoch(), first.epoch(), "L2/E1");
    assert!(
        renewed.expires_at_unix_ms() > first.expires_at_unix_ms(),
        "L2/E1"
    );
    clock.set(11);
    let fenced = q
        .acquire_session("env", &SessionId::from("session"), "owner-b")
        .await
        .expect("L3")
        .expect("L3 returns the current lease for typed fencing");
    assert_eq!(fenced.owner(), "owner-a", "L3/E2");
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
    clock.set(20);
    assert!(
        q.acquire_session("env", &SessionId::from("session"), "owner-b")
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
        .acquire_session("env", &SessionId::from("session"), "owner-a")
        .await
        .expect("L5")
        .expect("L5 lease");
    assert_eq!(replacement.epoch(), 2, "L5/E4");
    assert!(
        !q.release_session(&first).await.expect("L6 stale release"),
        "L6/E2"
    );
    assert_eq!(
        q.get("env", &id).await.unwrap().unwrap().state,
        WorkState::Active,
        "L6/E2 successor remains authoritative"
    );
    let projection = |work: &awaken_executable_environment_contract::WorkItem| {
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
    let with = |work: &str, environment: &str, session: &str, owner: &str, epoch: u64| {
        awaken_session_types::SessionWorkLease::from_persisted(
            work,
            environment,
            SessionId::from(session),
            owner,
            epoch,
            replacement.expires_at_unix_ms(),
        )
        .expect("nonzero epoch")
    };
    let (work, environment, session) = (
        replacement.work_id(),
        replacement.environment_id(),
        replacement.session_id().as_str(),
    );
    let (owner, epoch) = (replacement.owner(), replacement.epoch());
    let wrong_owner = with(work, environment, session, "owner-b", epoch);
    let wrong_epoch = with(work, environment, session, owner, first.epoch());
    let wrong_work = with("absent-work", environment, session, owner, epoch);
    let wrong_environment = with(work, "other-env", session, owner, epoch);
    let wrong_session = with(work, environment, "other-session", owner, epoch);
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
        clock.set(20);
        // The existing acquire contract returns another owner's current lease
        // without renewing it; this observes owner/epoch/expiry, not a repair.
        let current = q
            .acquire_session("env", &SessionId::from("session"), "observer")
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
        clock.set(20);
        assert!(
            q.acquire_session("env", &SessionId::from("session"), "observer")
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
pub async fn graceful_owner_release_is_exact<Q: WorkBackend>(q: &Q, clock: &ManualClock) {
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
    q.claim("env-a", "worker:1:old").await.unwrap().unwrap();
    q.claim("env-b", "worker:1:old").await.unwrap().unwrap();
    q.claim("env-c", "worker:2:new").await.unwrap().unwrap();

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
    clock.set(1);
    assert!(
        q.claim("env-a", "worker:2:new")
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

pub async fn exact_owner_observation_is_read_only(
    q: &impl WorkBackend,
    clock: &ManualClock,
    lease_ttl_ms: u64,
) {
    // Causes: O1 no lease; O2 a Session in env-a belongs to generation 1;
    // O3 a healthcheck in env-b belongs to generation 2; O4 time crosses expiry;
    // O5 the ordinary queue owner releases/reclaims a lease. Effects: E1 absent
    // exact owner is false; E2 any payload/environment owned by the exact owner
    // is true; E3 another incarnation never aliases it; E4 reads, including an
    // expired lease, leave state and heartbeat unchanged; E5 only the existing
    // mutation makes the original owner's observation false. Rules:
    // QO1=O1=>E1; QO2=O2+O3=>E2+E3; QO3=O4=>E2+E4; QO4=O5=>E5.
    let old = "worker:1:old";
    let new = "worker:2:new";
    assert!(!q.has_owner_claims(old).await.expect("QO1"));
    let session = q
        .enqueue_session("owner-env-a", &SessionId::from("owner-session"))
        .await
        .expect("fixture Session");
    let health = q
        .enqueue_healthcheck("owner-env-b")
        .await
        .expect("fixture healthcheck");
    q.claim("owner-env-a", old).await.unwrap().unwrap();
    q.claim("owner-env-b", new).await.unwrap().unwrap();
    let before_session = q.get("owner-env-a", &session).await.unwrap().unwrap();
    let before_health = q.get("owner-env-b", &health).await.unwrap().unwrap();
    assert!(q.has_owner_claims(old).await.expect("QO2 Session"));
    assert!(q.has_owner_claims(new).await.expect("QO2 healthcheck"));
    assert!(
        !q.has_owner_claims("worker:1:new")
            .await
            .expect("QO2 exact incarnation")
    );
    // A read after the lease deadline must not invoke the queue's reclamation.
    let after_expiry = lease_ttl_ms
        .checked_add(1)
        .expect("fixture expiry has a successor");
    clock.set(after_expiry);
    assert!(
        q.has_owner_claims(old)
            .await
            .expect("QO3 unreleased expiry")
    );
    let after_session = q.get("owner-env-a", &session).await.unwrap().unwrap();
    let after_health = q.get("owner-env-b", &health).await.unwrap().unwrap();
    assert_eq!(after_session.state, before_session.state, "QO3 state");
    assert_eq!(
        after_session.latest_heartbeat_at, before_session.latest_heartbeat_at,
        "QO3 heartbeat"
    );
    assert_eq!(after_health.state, before_health.state, "QO3 other payload");
    assert_eq!(
        q.release_owner(old).await.expect("QO4 canonical release"),
        1
    );
    assert!(!q.has_owner_claims(old).await.expect("QO4 released"));
    assert!(
        q.has_owner_claims(new)
            .await
            .expect("QO4 other owner retained")
    );
    q.claim("owner-env-b", "worker:3:replacement")
        .await
        .unwrap()
        .unwrap();
    assert!(
        !q.has_owner_claims(new)
            .await
            .expect("QO4 canonical reclaim")
    );
    assert!(
        q.has_owner_claims("worker:3:replacement")
            .await
            .expect("QO4 replacement")
    );
}

pub async fn reclaim_age_boundary_cases(q: &impl WorkBackend, clock: &ManualClock) {
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
    assert_eq!(q.claim(environment, "a").await.unwrap().unwrap().id, id);
    for (now, age, expected) in [
        (1, None, false),
        (1, Some(2), false),
        (1, Some(1), true),
        (1_001, Some(1_000), true),
    ] {
        clock.set(now);
        let actual = q
            .claim_with_reclaim(environment, "b", "b", age)
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
/// Decision table. Causes: C1 the queue accepts an item; C2 a claim, which
/// carries no time; C3 an acknowledgement, which carries none; C4 a stop,
/// which carries none; C5 the store is asked for queue stats; C6 the queue is
/// drained. Effects: E1 `created_at` is the injected reading at acceptance;
/// E2 `started_at` is the injected reading at the claim, the reading its lease
/// window is minted from, so the two cannot disagree; E3/E4 the acknowledgement
/// and stop instants are the injected reading *at that moment*, which is how
/// advancing the clock between commands is observable; E5 `oldest_queued_at` is
/// the oldest unfinished item's own `created_at`; E6 a drained queue reports
/// absence rather than a stand-in. Rules R1..R6 pair C1..C6 with E1..E6.
///
/// Diffusion: the sibling objects in the same store — the Environment's
/// healthcheck item and a second Session item accepted later — are checked too,
/// so a fix that reached only the one converted path fails here.
pub async fn observation_timestamps_are_the_injected_clock<Q: WorkBackend>(
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
            awaken_executable_environment_contract::WorkPayload::Session { id }
                if id == &second =>
            {
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

    clock.set(CLAIMED);
    assert_eq!(
        q.stats("obs-env").await.expect("R5").oldest_queued_at,
        Some(epoch_millis_to_rfc3339(ACCEPTED)),
        "R5 oldest unfinished item's own acceptance instant"
    );

    // R2: the claim samples the store clock once, under its write lock; the
    // reading moved since acceptance, so a reused acceptance stamp is caught.
    let claimed = q
        .claim("obs-env", "obs-worker")
        .await
        .expect("R2")
        .expect("R2 lease");
    assert_eq!(claimed.id, work_id, "R2 oldest first");
    assert_eq!(
        claimed.started_at,
        Some(epoch_millis_to_rfc3339(CLAIMED)),
        "R2 started_at is the store reading at the claim"
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
            clock.set(STOPPED);
            q.claim("obs-env", "obs-worker").await.expect("R6 claim");
            let _ = q
                .stop("obs-env", &item.id, "obs-worker")
                .await
                .expect("R6 stop");
        }
    }
    clock.set(STOPPED);
    let drained = q.stats("obs-env").await.expect("R6");
    assert_eq!(drained.depth, 0, "R6 drained");
    assert_eq!(drained.pending, 0, "R6 drained");
    assert_eq!(drained.oldest_queued_at, None, "R6 absence is stated");
    q.remove_env("obs-env").await.expect("R6 cleanup");
}

/// Run the whole contract against a freshly-built backend `Q`. Every rule gets
/// its own backend over its own [`ManualClock`] starting at zero: the store's
/// clock is the only time any rule can drive, because no Work lease operation
/// accepts a caller's reading.
pub async fn run_suite<Q: WorkBackend, F: std::future::Future<Output = Q>>(
    fresh_with: impl Fn(Arc<dyn WallClock>) -> F,
    lease_ttl_ms: u64,
) {
    let (q, _) = on_fresh_clock(&fresh_with).await;
    environment_revision_replay_is_atomic(&q).await;
    let (q, _) = on_fresh_clock(&fresh_with).await;
    environment_revision_races_preserve_the_maximum(&q).await;
    let (q, clock) = on_fresh_clock(&fresh_with).await;
    single_active_cap(&q, &clock).await;
    let (q, clock) = on_fresh_clock(&fresh_with).await;
    reclaim_at_ttl(&q, &clock, lease_ttl_ms).await;
    let (q, clock) = on_fresh_clock(&fresh_with).await;
    requested_reclaim_uses_refresh_clock(&q, &clock).await;
    let (q, _) = on_fresh_clock(&fresh_with).await;
    env_isolation(&q).await;
    let (q, _) = on_fresh_clock(&fresh_with).await;
    stop_frees_next(&q).await;
    let (q, _) = on_fresh_clock(&fresh_with).await;
    remove_env_purges(&q).await;
    let (q, _) = on_fresh_clock(&fresh_with).await;
    session_enqueue_is_idempotent(&q).await;
    let (q, clock) = on_fresh_clock(&fresh_with).await;
    heartbeat_compare_and_extend(&q, &clock).await;
    let (q, clock) = on_fresh_clock(&fresh_with).await;
    session_ownership_lifecycle_is_single_and_fenced(&q, &clock).await;
    let (q, clock) = on_fresh_clock(&fresh_with).await;
    graceful_owner_release_is_exact(&q, &clock).await;
    let (q, clock) = on_fresh_clock(&fresh_with).await;
    exact_owner_observation_is_read_only(&q, &clock, lease_ttl_ms).await;
    let (q, clock) = on_fresh_clock(&fresh_with).await;
    reclaim_age_boundary_cases(&q, &clock).await;
    let (q, clock) = on_fresh_clock(&fresh_with).await;
    session_access::capability_and_metadata_contract(&q, &clock, lease_ttl_ms).await;
    let (q, clock) = on_fresh_clock(&fresh_with).await;
    role_views_share_state_without_widening_authority(&q, &clock, lease_ttl_ms).await;
    let (q, clock) = on_fresh_clock(&fresh_with).await;
    observation_timestamps_are_the_injected_clock(&q, &clock).await;
    let (q, clock) = on_fresh_clock(&fresh_with).await;
    lease_time_is_the_store_clock(&q, &clock, lease_ttl_ms).await;
}

/// A backend built over a new [`ManualClock`] at zero, and that clock.
async fn on_fresh_clock<Q, F: std::future::Future<Output = Q>>(
    fresh_with: &impl Fn(Arc<dyn WallClock>) -> F,
) -> (Q, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::new(0));
    (fresh_with(clock.clone()).await, clock)
}

/// One lease, one clock: every Work lease is granted, renewed and judged on the
/// store's own injected clock, and no operation accepts a caller's time.
///
/// The failure this replaces: `claim` and `heartbeat` took the caller's
/// `now_ms` while `ack`, `stop` and their Session-access forms read the store
/// clock, so a lease's deadline was minted on one clock and judged on another.
/// A caller running ahead of the store had its live lease refused at
/// acknowledgement; a caller running behind minted a deadline the store had
/// already passed, or kept an expired lease acknowledgeable. The caller now
/// supplies no reading at all (K1, enforced by the port signatures), so the
/// rows below vary only the store's reading and must hold for every backend.
///
/// Causes: C1 the store reading at the grant; C2 the store reading at the
/// judgement, relative to the deadline (D-1, D); C3 a heartbeat renews with a
/// requested TTL; C4 the grant is a Worker claim or a Session acquisition.
/// Effects: E1 the deadline is the grant's store reading plus the TTL and
/// `started_at` is that same reading; E2 a live lease is acknowledged; E3 an
/// expired lease is refused acknowledgement and left unchanged; E4 a heartbeat
/// moves the deadline to its own store reading plus the requested TTL; E5
/// bearer authentication is live strictly before the deadline and absent at it.
///
/// | Rule | C1 grant | C2 judgement     | C3 renew | C4 grant kind | Effect     |
/// |------|----------|------------------|----------|---------------|------------|
/// | S1   | T0       | -                | -        | claim         | E1         |
/// | S2   | G        | ack at G+L-1     | -        | claim         | E2         |
/// | S3   | G        | ack at G+L       | -        | claim         | E3         |
/// | S4   | T0       | -                | at T1    | claim         | E4         |
/// | S5   | T0       | auth at D-1, D   | at T1    | claim         | E5 on E4's D |
/// | S6   | T2       | -                | -        | acquisition   | E1         |
///
/// S2 and S3 are the two skew directions of the replaced defect: a caller ahead
/// of the store could refuse S2, and a caller behind it could accept S3.
pub async fn lease_time_is_the_store_clock<Q: WorkBackend>(
    q: &Q,
    clock: &ManualClock,
    lease_ttl_ms: u64,
) {
    const T0: u64 = 1_700_000_000_000;
    const RENEWED_TTL_SECONDS: u64 = 120;

    // S1: the deadline and `started_at` are the grant's store reading.
    clock.set(T0);
    let renewed = q
        .enqueue_session("clock-renewed", &SessionId::from("renewed"))
        .await
        .expect("S1 enqueue");
    let claim = q
        .claim_with_session_access(
            "clock-renewed",
            "owner",
            "owner",
            None,
            &session_access_digest(41),
        )
        .await
        .expect("S1 claim")
        .expect("S1 lease");
    assert_eq!(
        claim.item.started_at,
        Some(epoch_millis_to_rfc3339(T0)),
        "S1/E1 started_at is the store reading at the grant"
    );
    let token = claim.session_access.expect("S1 bound digest");
    let granted = q
        .authenticate_session_access(&token)
        .await
        .expect("S1 authenticate")
        .expect("S1 live bearer");
    assert_eq!(
        granted.expires_at_unix_ms,
        T0 + lease_ttl_ms,
        "S1/E1 the deadline is the store reading plus the TTL"
    );

    // S4: the heartbeat extends from its own store reading.
    let renewed_at = T0 + lease_ttl_ms / 2;
    clock.set(renewed_at);
    let receipt = q
        .heartbeat(
            "clock-renewed",
            &renewed,
            "owner",
            LeaseHeartbeat {
                condition: HeartbeatCondition::First,
                desired_ttl_seconds: Some(RENEWED_TTL_SECONDS),
            },
        )
        .await
        .expect("S4 heartbeat")
        .into_receipt()
        .expect("S4 accepted");
    assert!(receipt.lease_extended, "S4/E4 an active lease extends");
    let deadline = renewed_at + RENEWED_TTL_SECONDS * 1_000;
    assert_eq!(
        q.authenticate_session_access(&token)
            .await
            .expect("S4 authenticate")
            .expect("S4 live")
            .expires_at_unix_ms,
        deadline,
        "S4/E4 the renewed deadline is the heartbeat's store reading plus its TTL"
    );

    // S5: the renewed deadline is judged on the same clock that set it.
    clock.set(deadline - 1);
    assert!(
        q.authenticate_session_access(&token)
            .await
            .expect("S5 authenticate")
            .is_some(),
        "S5/E5 live strictly before the deadline"
    );
    clock.set(deadline);
    assert!(
        q.authenticate_session_access(&token)
            .await
            .expect("S5 authenticate")
            .is_none(),
        "S5/E5 absent at the deadline"
    );

    // S2/S3: the same deadline judged on the same clock, either side of it.
    // Each row grants at its own later base so the timeline only moves forward.
    let mut granted_at = deadline;
    for (rule, held_for, accepted) in [("S2", lease_ttl_ms - 1, true), ("S3", lease_ttl_ms, false)]
    {
        let env = format!("clock-ack-{rule}");
        granted_at += lease_ttl_ms * 2;
        let judged_at = granted_at + held_for;
        clock.set(granted_at);
        let work = q
            .enqueue_session(&env, &SessionId::from(rule))
            .await
            .expect("S2/S3 enqueue");
        q.claim(&env, "owner")
            .await
            .expect("S2/S3 claim")
            .expect("S2/S3 lease");
        let before = q.get(&env, &work).await.unwrap().unwrap();
        clock.set(judged_at);
        let acked = q.ack(&env, &work, "owner").await.expect("S2/S3 ack");
        if accepted {
            let WorkMutationResult::Accepted(item) = acked else {
                panic!("{rule}/E2 a live lease must be acknowledged, got {acked:?}");
            };
            assert_eq!(
                item.acknowledged_at,
                Some(epoch_millis_to_rfc3339(judged_at)),
                "{rule}/E2 acknowledged on the store reading"
            );
        } else {
            assert!(
                matches!(acked, WorkMutationResult::PreconditionFailed),
                "{rule}/E3 an expired lease must be refused, got {acked:?}"
            );
            let after = q.get(&env, &work).await.unwrap().unwrap();
            assert_eq!(after.state, before.state, "{rule}/E3 state unchanged");
            assert_eq!(after.acknowledged_at, None, "{rule}/E3 nothing stamped");
        }
    }

    // S6: a Session acquisition grants on the same clock.
    let acquired_at = granted_at + lease_ttl_ms * 2;
    clock.set(acquired_at);
    q.enqueue_session("clock-acquire", &SessionId::from("acquired"))
        .await
        .expect("S6 enqueue");
    let lease = q
        .acquire_session("clock-acquire", &SessionId::from("acquired"), "owner")
        .await
        .expect("S6 acquire")
        .expect("S6 lease");
    assert_eq!(
        lease.expires_at_unix_ms(),
        acquired_at + lease_ttl_ms,
        "S6/E1 the acquisition deadline is the store reading plus the TTL"
    );
    let acquired = q
        .get("clock-acquire", lease.work_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        acquired.started_at,
        Some(epoch_millis_to_rfc3339(acquired_at)),
        "S6/E1 started_at is the same reading"
    );
}

/// Decision sequence for separately injected roles over the same backend.
/// R1 a metadata upsert/delete is visible to queries but preserves the complete
/// live lease and lifecycle; R2 a foreign Environment or absent Work stays
/// absent; R3 same-owner reclaim advances epoch and invalidates the old bearer;
/// R4 exact-epoch mutation refuses the old proof while the owner-only protocol
/// still accepts the current credential owner; R5 withdrawal clears item,
/// credential and owner-occupancy observations.
pub async fn role_views_share_state_without_widening_authority<Q: WorkBackend>(
    backend: &Q,
    clock: &ManualClock,
    lease_ttl_ms: u64,
) {
    let queries: &dyn WorkQueries = backend;
    let consumer: &dyn WorkConsumer = backend;
    let authentication: &dyn SessionAccessAuthenticator = backend;
    let control: &dyn SessionWorkLeaseControl = backend;
    let metadata: &dyn WorkMetadataPatch = backend;
    let assignment: &dyn SessionWorkStore = backend;
    let lifecycle: &dyn EnvironmentWorkLifecycle = backend;
    let occupancy: &dyn WorkerLeaseObservation = backend;
    let work_id = assignment
        .enqueue_session(
            "role-env",
            &awaken_session_types::SessionId::from("role-session"),
        )
        .await
        .unwrap();
    let first_digest = session_access_digest(91);
    consumer
        .claim_with_session_access("role-env", "same-owner", "poller", None, &first_digest)
        .await
        .unwrap()
        .expect("R1 claim");
    let first_access = authentication
        .authenticate_session_access(&first_digest)
        .await
        .unwrap()
        .expect("R1 complete proof");
    let before = queries.get("role-env", &work_id).await.unwrap().unwrap();
    metadata
        .update_metadata(
            "role-env",
            &work_id,
            BTreeMap::from([
                ("keep".into(), Some("one".into())),
                ("remove".into(), Some("old".into())),
            ]),
        )
        .await
        .unwrap()
        .unwrap();
    let patched = metadata
        .update_metadata(
            "role-env",
            &work_id,
            BTreeMap::from([("keep".into(), Some("two".into())), ("remove".into(), None)]),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        patched.metadata,
        BTreeMap::from([("keep".into(), "two".into())]),
        "R1 patch semantics"
    );
    let read = queries.get("role-env", &work_id).await.unwrap().unwrap();
    assert_eq!(read.metadata, patched.metadata, "R1 shared state");
    assert_eq!(read.state, before.state, "R1 state");
    assert_eq!(read.created_at, before.created_at, "R1 acceptance");
    assert_eq!(read.started_at, before.started_at, "R1 started");
    assert_eq!(read.acknowledged_at, before.acknowledged_at, "R1 ack");
    assert_eq!(
        read.latest_heartbeat_at, before.latest_heartbeat_at,
        "R1 heartbeat"
    );
    assert_eq!(
        read.stop_requested_at, before.stop_requested_at,
        "R1 stop intent"
    );
    assert_eq!(read.stopped_at, before.stopped_at, "R1 terminal");
    assert_eq!(
        authentication
            .authenticate_session_access(&first_digest)
            .await
            .unwrap(),
        Some(first_access.clone()),
        "R1 authority unchanged"
    );
    for (environment, work) in [("foreign-env", work_id.as_str()), ("role-env", "missing")] {
        assert!(
            metadata
                .update_metadata(environment, work, BTreeMap::from([("keep".into(), None)]))
                .await
                .unwrap()
                .is_none(),
            "R2 isolation"
        );
    }
    assert_eq!(
        queries
            .get("role-env", &work_id)
            .await
            .unwrap()
            .unwrap()
            .metadata,
        patched.metadata,
        "R2 no side effect"
    );
    let next_digest = session_access_digest(92);
    clock.set(lease_ttl_ms);
    consumer
        .claim_with_session_access("role-env", "same-owner", "poller", None, &next_digest)
        .await
        .unwrap()
        .expect("R3 exact expiry reclaim");
    let next_access = authentication
        .authenticate_session_access(&next_digest)
        .await
        .unwrap()
        .expect("R3 successor");
    assert!(
        next_access.lease_epoch > first_access.lease_epoch,
        "R3 epoch advances"
    );
    assert!(
        authentication
            .authenticate_session_access(&first_digest)
            .await
            .unwrap()
            .is_none(),
        "R3 old bearer revoked"
    );
    assert!(
        matches!(
            control
                .ack_with_session_access(&first_access)
                .await
                .unwrap(),
            WorkMutationResult::PreconditionFailed
        ),
        "R4 old ack"
    );
    assert!(
        matches!(
            control
                .heartbeat_with_session_access(&first_access, LeaseHeartbeat::unconditional())
                .await
                .unwrap(),
            HeartbeatResult::PreconditionFailed
        ),
        "R4 old heartbeat"
    );
    assert!(
        matches!(
            control
                .stop_with_session_access(&first_access)
                .await
                .unwrap(),
            WorkMutationResult::PreconditionFailed
        ),
        "R4 old stop"
    );
    assert!(
        consumer
            .ack("role-env", &work_id, "same-owner")
            .await
            .unwrap()
            .is_accepted(),
        "R4 owner-only protocol"
    );
    clock.set(lease_ttl_ms + 1);
    assert!(
        control
            .heartbeat_with_session_access(&next_access, LeaseHeartbeat::unconditional())
            .await
            .unwrap()
            .into_receipt()
            .is_some(),
        "R4 exact current proof"
    );
    assert!(
        authentication
            .authenticate_session_access(&next_digest)
            .await
            .unwrap()
            .is_some(),
        "R5 live authority before purge"
    );
    lifecycle.remove_env("role-env").await.unwrap();
    assert!(
        !occupancy.has_owner_claims("same-owner").await.unwrap(),
        "R5 no dangling owner occupancy"
    );
    assert!(
        queries.list("role-env").await.unwrap().is_empty(),
        "R5 purge visible"
    );
    assert!(
        authentication
            .authenticate_session_access(&next_digest)
            .await
            .unwrap()
            .is_none(),
        "R5 authority gone"
    );
}

// ── Backend rows: each must pass the identical suite ─────────────────────────────

pub mod session_access {
    //! The portable capability/metadata sequence formerly covered only PostgreSQL.
    use super::{ManualClock, WorkBackend, session_access_digest};
    use awaken_executable_environment_contract::{
        HeartbeatResult, LeaseHeartbeat, WorkMutationResult,
    };
    use std::collections::BTreeMap;

    /// Cause/effect sequence: queued healthcheck + two Sessions => FIFO/cap; an
    /// active owner targeting queued work => precondition failure; wrong environment
    /// => absent; metadata patch => visible value; stop => next Session claimable;
    /// current token => exact identity; reclaim => old access fenced and new access
    /// accepted; purge => empty. These are sequential state constraints rather than
    /// independent Boolean combinations. Each transition checks its effect before
    /// triggering the next, and all backends run this same sequence.
    pub async fn capability_and_metadata_contract(
        q: &impl WorkBackend,
        clock: &ManualClock,
        lease_ttl_ms: u64,
    ) {
        let hc = q.enqueue_healthcheck("env_a").await.expect("enqueue");
        let w1 = q
            .enqueue_session("env_a", &awaken_session_types::SessionId::from("s1"))
            .await
            .expect("enqueue");
        let w2 = q
            .enqueue_session("env_a", &awaken_session_types::SessionId::from("s2"))
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
            q.claim("env_a", "w")
                .await
                .expect("claim query")
                .expect("lease")
                .id,
            hc
        );
        assert!(
            q.claim("env_a", "w").await.expect("claim").is_none(),
            "single active lease"
        );
        // The active healthcheck owner cannot acknowledge another queued item.
        assert!(matches!(
            q.ack("env_a", &w1, "w").await.expect("ack query"),
            WorkMutationResult::PreconditionFailed
        ));
        assert!(matches!(
            q.heartbeat("env_a", &w1, "w", LeaseHeartbeat::unconditional())
                .await
                .expect("heartbeat"),
            HeartbeatResult::PreconditionFailed
        ));
        assert!(
            q.get("env_b", &w1).await.expect("get").is_none(),
            "wrong env → none"
        );
        assert!(
            q.heartbeat("env_b", &w1, "w", LeaseHeartbeat::unconditional())
                .await
                .expect("heartbeat")
                .is_not_found()
        );
        // stats reflect the active healthcheck and queued Session work.
        let st = q.stats("env_a").await.expect("stats");
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
        clock.set(1);
        let session_claim = q
            .claim_with_session_access("env_a", "w", "w", None, &session_access_digest(1))
            .await
            .expect("session claim")
            .expect("queued Session");
        let token = session_claim
            .session_access
            .as_ref()
            .expect("Session claim token");
        let access = q
            .authenticate_session_access(token)
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
        clock.set(lease_ttl_ms + 1);
        let replacement = q
            .claim_with_session_access(
                "env_a",
                "w-replacement",
                "w-replacement",
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
        clock.set(lease_ttl_ms + 2);
        assert!(
            q.heartbeat_with_session_access(&replacement_access, LeaseHeartbeat::unconditional())
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

/// Test-only aggregate for the shared backend contract suite.
pub trait WorkBackend:
    WorkQueries
    + WorkConsumer
    + SessionAccessAuthenticator
    + SessionWorkLeaseControl
    + SessionWorkStore
    + SessionWorkerLeaseStore
    + WorkerLeaseObservation
    + WorkMetadataPatch
    + EnvironmentWorkLifecycle
{
}
impl<
    T: WorkQueries
        + WorkConsumer
        + SessionAccessAuthenticator
        + SessionWorkLeaseControl
        + SessionWorkStore
        + SessionWorkerLeaseStore
        + WorkerLeaseObservation
        + WorkMetadataPatch
        + EnvironmentWorkLifecycle,
> WorkBackend for T
{
}

/// Revision effect matrix, run unchanged against every backend. Live Session
/// identity and bearer remain unchanged during Cloud revision or Archive.
/// Withdraw removes only healthchecks; direct remove_env retains its purge contract.
pub async fn environment_revision_replay_is_atomic<Q: WorkBackend>(q: &Q) {
    use RevisionOperation::{Register, Withdraw};
    use RevisionOutcome::{AlreadyApplied, Applied, Superseded};
    let env = "revision-matrix";
    let session = q
        .enqueue_session(env, &SessionId::from("frozen-session"))
        .await
        .unwrap();
    let digest = session_access_digest(701);
    q.claim_with_session_access(env, "original-owner", "poller", None, &digest)
        .await
        .unwrap()
        .unwrap();
    let proof = q
        .authenticate_session_access(&digest)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        q.apply_environment_revision(env, EnvironmentRevision(3), Register { healthcheck: true })
            .await
            .unwrap(),
        Applied
    );
    let before: Vec<_> = q
        .list(env)
        .await
        .unwrap()
        .iter()
        .map(|item| format!("{item:?}"))
        .collect();
    for operation in [Register { healthcheck: false }, Withdraw] {
        assert_eq!(
            q.apply_environment_revision(env, EnvironmentRevision(2), operation)
                .await
                .unwrap(),
            Superseded
        );
        assert!(matches!(
            q.apply_environment_revision(env, EnvironmentRevision(3), operation).await,
            Err(EnvironmentWorkRevisionError::Conflict { revision, current, proposed })
                if revision == EnvironmentRevision(3) && current == (Register { healthcheck: true }) && proposed == operation
        ));
        assert_eq!(
            q.list(env)
                .await
                .unwrap()
                .iter()
                .map(|item| format!("{item:?}"))
                .collect::<Vec<_>>(),
            before
        );
    }
    assert_eq!(
        q.apply_environment_revision(env, EnvironmentRevision(3), Register { healthcheck: true })
            .await
            .unwrap(),
        AlreadyApplied
    );
    assert_eq!(
        q.apply_environment_revision(env, EnvironmentRevision(4), Register { healthcheck: false })
            .await
            .unwrap(),
        Applied
    );
    assert_eq!(
        q.list(env)
            .await
            .unwrap()
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        [session.as_str()]
    );
    assert_eq!(
        q.authenticate_session_access(&digest).await.unwrap(),
        Some(proof.clone())
    );
    assert!(
        q.enqueue_healthcheck(env).await.is_err(),
        "Cloud cannot be bypassed"
    );
    assert!(q.ensure_healthcheck(env).await.is_err());
    assert_eq!(
        q.apply_environment_revision(env, EnvironmentRevision(5), Withdraw)
            .await
            .unwrap(),
        Applied
    );
    assert_eq!(
        q.apply_environment_revision(env, EnvironmentRevision(5), Withdraw)
            .await
            .unwrap(),
        AlreadyApplied
    );
    assert_eq!(
        q.apply_environment_revision(env, EnvironmentRevision(4), Register { healthcheck: true })
            .await
            .unwrap(),
        Superseded
    );
    let retained = q.list(env).await.unwrap();
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].id, session);
    assert_eq!(
        q.authenticate_session_access(&digest).await.unwrap(),
        Some(proof)
    );
    assert!(q.enqueue_healthcheck(env).await.is_err());
    assert!(q.ensure_healthcheck(env).await.is_err());
    assert!(matches!(
        q.apply_environment_revision(env, EnvironmentRevision(0), Withdraw)
            .await,
        Err(EnvironmentWorkRevisionError::Invalid(
            awaken_executable_environment_contract::EnvironmentWorkRevisionInputError::ZeroRevision
        ))
    ));
    assert!(matches!(
        q.apply_environment_revision("", EnvironmentRevision(6), Withdraw)
            .await,
        Err(EnvironmentWorkRevisionError::Invalid(awaken_executable_environment_contract::EnvironmentWorkRevisionInputError::MissingEnvironmentId))
    ));
    // A real higher registration is admitted; stale Withdraw must never delete it.
    assert_eq!(
        q.apply_environment_revision(
            env,
            EnvironmentRevision(u64::MAX),
            Register { healthcheck: true }
        )
        .await
        .unwrap(),
        Applied
    );
    assert_eq!(
        q.apply_environment_revision(env, EnvironmentRevision(5), Withdraw)
            .await
            .unwrap(),
        Superseded
    );
    assert_eq!(q.list(env).await.unwrap().len(), 2);
}

/// Opposing revisions, same-revision conflicting operations and a legacy
/// healthcheck helper contend on the backend's real mutation boundary.
pub async fn environment_revision_races_preserve_the_maximum<Q: WorkBackend>(q: &Q) {
    use RevisionOperation::{Register, Withdraw};
    use RevisionOutcome::{Applied, Superseded};
    for round in 0..12 {
        let env = format!("revision-race-{round}");
        let (older, newest) = tokio::join!(
            q.apply_environment_revision(
                &env,
                EnvironmentRevision(1),
                Register { healthcheck: true }
            ),
            q.apply_environment_revision(&env, EnvironmentRevision(2), Withdraw),
        );
        assert!(matches!(older.unwrap(), Applied | Superseded));
        assert_eq!(newest.unwrap(), Applied);
        assert!(q.list(&env).await.unwrap().is_empty());
        assert_eq!(
            q.apply_environment_revision(
                &env,
                EnvironmentRevision(1),
                Register { healthcheck: true }
            )
            .await
            .unwrap(),
            Superseded
        );
        let env = format!("revision-conflict-{round}");
        let (left, right) = tokio::join!(
            q.apply_environment_revision(
                &env,
                EnvironmentRevision(7),
                Register { healthcheck: true }
            ),
            q.apply_environment_revision(
                &env,
                EnvironmentRevision(7),
                Register { healthcheck: false }
            ),
        );
        match (left, right) {
            (Ok(Applied), Err(_)) => assert_eq!(q.list(&env).await.unwrap().len(), 1),
            (Err(_), Ok(Applied)) => assert!(q.list(&env).await.unwrap().is_empty()),
            other => panic!("same revision admitted incompatible operations: {other:?}"),
        }
        let (helper, withdrawn) = tokio::join!(
            q.ensure_healthcheck(&env),
            q.apply_environment_revision(&env, EnvironmentRevision(8), Withdraw),
        );
        let _ = helper; // May have linearized before withdrawal or been refused.
        assert_eq!(withdrawn.unwrap(), Applied);
        assert!(q.list(&env).await.unwrap().is_empty());
        assert!(q.ensure_healthcheck(&env).await.is_err());
    }
}

/// T34-C01 reopen row for the `WorkQueue` port. Writes through `first`, drops
/// it, and re-asserts through the handle `reopen` returns:
///
/// | Step | Through | Effect |
/// |---|---|---|
/// | S1 | `first` | Environment revision 2 registered; two Session works enqueued |
/// | S2 | `first` | `owner-a` acquires the Session lease (epoch 1) on the first work |
/// | R1 | `reopen()` | identical rows (ids, states, metadata, `*_at` clock readings) |
/// | R2 | `reopen()` | Session enqueue replay returns the original work id; no new row |
/// | R3 | `reopen()` | the durable lease still fences: `owner-b` sees `owner-a`/epoch 1, its ack/stop fail |
/// | R4 | `reopen()` | revision 2 replay is AlreadyApplied, revision 1 is Superseded |
/// | R5 | `reopen()` | the original owner's ack is accepted |
///
/// A factory that does not reach the same durable authority fails R1.
/// `clock` is the one clock both handles were opened with, so the reopened
/// store judges the surviving lease on the reading that granted it.
pub async fn reopen_preserves_authority<Q: WorkBackend, F: std::future::Future<Output = Q>>(
    first: Q,
    reopen: impl Fn() -> F,
    clock: &ManualClock,
) {
    use RevisionOperation::Register;
    use RevisionOutcome::{AlreadyApplied, Applied, Superseded};
    let env = "t34-reopen";
    let image = async |q: &Q| -> Vec<String> {
        let mut rows: Vec<String> = q
            .list(env)
            .await
            .expect("list")
            .iter()
            .map(|item| format!("{item:?}"))
            .collect();
        rows.sort();
        rows
    };

    // S1
    assert_eq!(
        first
            .apply_environment_revision(
                env,
                EnvironmentRevision(2),
                Register { healthcheck: false }
            )
            .await
            .expect("S1 revision"),
        Applied
    );
    let a = first
        .enqueue_session(env, &SessionId::from("reopen-a"))
        .await
        .expect("S1 enqueue a");
    let b = first
        .enqueue_session(env, &SessionId::from("reopen-b"))
        .await
        .expect("S1 enqueue b");
    assert_ne!(a, b, "S1 distinct identities");
    // S2
    let lease = first
        .acquire_session(env, &SessionId::from("reopen-a"), "owner-a")
        .await
        .expect("S2 acquire")
        .expect("S2 lease");
    assert_eq!((lease.work_id(), lease.epoch()), (a.as_str(), 1), "S2");
    let before = image(&first).await;
    assert_eq!(before.len(), 2, "S1 two rows");

    drop(first);
    let reopened = reopen().await;

    // R1
    assert_eq!(image(&reopened).await, before, "R1 reopen image");
    // R2
    assert_eq!(
        reopened
            .enqueue_session(env, &SessionId::from("reopen-a"))
            .await
            .expect("R2 replay"),
        a,
        "R2 the replay returns the original work id"
    );
    assert_eq!(image(&reopened).await, before, "R2 no new row");
    // R3
    clock.set(1);
    let fenced = reopened
        .acquire_session(env, &SessionId::from("reopen-a"), "owner-b")
        .await
        .expect("R3 acquire")
        .expect("R3 returns the current lease for typed fencing");
    assert_eq!(
        (fenced.owner(), fenced.epoch()),
        ("owner-a", 1),
        "R3 the lease survived the reopen"
    );
    assert!(
        matches!(
            reopened.ack(env, &a, "owner-b").await.expect("R3 ack"),
            WorkMutationResult::PreconditionFailed
        ),
        "R3 a foreign ack is fenced after reopen"
    );
    assert!(
        matches!(
            reopened.stop(env, &a, "owner-b").await.expect("R3 stop"),
            WorkMutationResult::PreconditionFailed
        ),
        "R3 a foreign stop is fenced after reopen"
    );
    // R4
    assert_eq!(
        reopened
            .apply_environment_revision(
                env,
                EnvironmentRevision(2),
                Register { healthcheck: false }
            )
            .await
            .expect("R4 replay"),
        AlreadyApplied,
        "R4 revision replay"
    );
    assert_eq!(
        reopened
            .apply_environment_revision(
                env,
                EnvironmentRevision(1),
                Register { healthcheck: false }
            )
            .await
            .expect("R4 stale"),
        Superseded,
        "R4 stale revision"
    );
    // R5
    assert!(
        reopened
            .ack(env, &a, "owner-a")
            .await
            .expect("R5 ack")
            .is_accepted(),
        "R5 the original owner still holds the lease"
    );
}
