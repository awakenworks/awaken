//! Trait-generic **conformance suite** for the `WorkQueue` port — the shared behavioural
//! contract EVERY backend must satisfy, run here against the in-memory reference,
//! SQLite, and PostgreSQL backends (ADR-0059 / ADR-0039-slice-2.6 pattern, mirroring
//! `awaken-store-conformance`).
//!
//! Why this exists and what it drives: the Phase-1 `proptest` properties fuzz the *one*
//! in-memory backend. This suite LIFTS the same invariants to the PORT: it asserts the
//! durable backends are observably identical to the reference on the safety-critical
//! semantics — the single-active-lease cap, reclaim-at-TTL,
//! environment isolation, stop-frees-next, and remove_env-purges. A divergence here is a
//! real parity bug (the kind that ships a run twice on one deployment mode but not
//! another); making it a checked contract forces the backends to agree by construction.
//! Postgres joins the same suite behind its DB harness (`pg_tests.sh`).

use awaken_session_contract::SessionId;
use awaken_session_contract::work_queue::{
    HeartbeatCondition, HeartbeatResult, LeaseHeartbeat, WorkMutationResult, WorkQueue, WorkState,
};
use awaken_work_store::{InMemoryWorkQueue, LEASE_TTL_MS, PostgresWorkQueue, SqliteWorkQueue};

mod work_queue_conformance {
    pub mod session_access;
}

fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime")
        .block_on(f)
}

// ── The port contract, trait-generic over any WorkQueue backend ──────────────────

/// The single-active-lease cap: with items queued, repeated claims at one instant hand
/// out exactly ONE lease, and exactly one item is Active.
async fn single_active_cap<Q: WorkQueue>(q: &Q) {
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
async fn reclaim_at_ttl<Q: WorkQueue>(q: &Q) {
    q.enqueue_session("env", &SessionId::from("s0"))
        .await
        .expect("enqueue");
    assert!(
        q.claim("env", "a", 0).await.expect("claim").is_some(),
        "first claim leases"
    );
    assert!(
        q.claim("env", "b", LEASE_TTL_MS - 1)
            .await
            .expect("claim")
            .is_none(),
        "held until the last ms before expiry"
    );
    assert!(
        q.claim("env", "b", LEASE_TTL_MS)
            .await
            .expect("claim")
            .is_some(),
        "reclaimable at exactly the TTL boundary"
    );
}

/// Requested reclaim age is measured from the last lease refresh, independently
/// of the TTL selected by that heartbeat.
async fn requested_reclaim_uses_refresh_clock<Q: WorkQueue>(q: &Q) {
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
async fn env_isolation<Q: WorkQueue>(q: &Q) {
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
async fn stop_frees_next<Q: WorkQueue>(q: &Q) {
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
async fn remove_env_purges<Q: WorkQueue>(q: &Q) {
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
async fn session_enqueue_is_idempotent<Q: WorkQueue>(q: &Q) {
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
async fn heartbeat_compare_and_extend<Q: WorkQueue>(q: &Q) {
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
async fn session_ownership_lifecycle_is_single_and_fenced<Q: WorkQueue>(q: &Q) {
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
async fn graceful_owner_release_is_exact<Q: WorkQueue>(q: &Q) {
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

async fn reclaim_age_boundary_cases(q: &impl WorkQueue) {
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

/// Run the whole contract against a freshly-built backend `Q`.
async fn run_suite<Q: WorkQueue, F: std::future::Future<Output = Q>>(fresh: impl Fn() -> F) {
    single_active_cap(&fresh().await).await;
    reclaim_at_ttl(&fresh().await).await;
    requested_reclaim_uses_refresh_clock(&fresh().await).await;
    env_isolation(&fresh().await).await;
    stop_frees_next(&fresh().await).await;
    remove_env_purges(&fresh().await).await;
    session_enqueue_is_idempotent(&fresh().await).await;
    heartbeat_compare_and_extend(&fresh().await).await;
    session_ownership_lifecycle_is_single_and_fenced(&fresh().await).await;
    graceful_owner_release_is_exact(&fresh().await).await;
    reclaim_age_boundary_cases(&fresh().await).await;
    work_queue_conformance::session_access::capability_and_metadata_contract(&fresh().await).await;
}

// ── Backend rows: each must pass the identical suite ─────────────────────────────

#[test]
fn in_memory_backend_conforms() {
    block(run_suite(async || InMemoryWorkQueue::new()));
}

#[test]
fn sqlite_backend_conforms() {
    block(run_suite(async || {
        SqliteWorkQueue::open_in_memory().expect("sqlite in-memory queue")
    }));
}

/// Fixture decision table: absent URL => explicit local skip; supplied URL plus
/// connection/schema failure => fail; supplied reachable URL => every shared
/// rule runs with fresh tables. Each schema has a generated unique name, so
/// simultaneous test processes cannot erase each other's authority or data.
#[tokio::test]
async fn postgres_backend_conforms() {
    use sqlx::Executor;
    use sqlx::postgres::{PgPool, PgPoolOptions};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let Ok(url) = std::env::var("AWAKEN_TEST_DATABASE_URL") else {
        eprintln!("[skip] postgres_backend_conforms: AWAKEN_TEST_DATABASE_URL absent");
        return;
    };
    let admin = PgPool::connect(&url)
        .await
        .expect("explicit PostgreSQL URL must connect");
    let prefix = format!(
        "work_conformance_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let sequence = AtomicUsize::new(0);
    run_suite(async || {
        let index = sequence.fetch_add(1, Ordering::Relaxed);
        let schema = format!("{prefix}_{index}");
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .expect("isolated schema");
        let pool = PgPoolOptions::new()
            .after_connect(move |connection, _| {
                let statement = format!("SET search_path = {schema}");
                Box::pin(async move {
                    connection.execute(statement.as_str()).await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .expect("schema pool");
        PostgresWorkQueue::with_pool(pool)
            .await
            .expect("queue schema migrations")
    })
    .await;
    for index in 0..sequence.load(Ordering::Relaxed) {
        admin
            .execute(format!("DROP SCHEMA {prefix}_{index} CASCADE").as_str())
            .await
            .expect("remove owned schema");
    }
    admin.close().await;
}
