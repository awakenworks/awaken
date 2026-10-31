//! T37-C03 (Work half): an exhausted identity allocator fails closed with one
//! stable error, while every legal exact replay still returns its original
//! result -- across independent handles and a reopen.
//!
//! Cause/effect graph: C1 Register rev1 committed its healthcheck Work H and a
//! Session Work S exists; C2 the persisted allocator reaches `i64::MAX`; C3 a
//! second independent handle and later a reopened handle retry. Effects:
//! E1 a new allocation fails with the same error on every handle and writes
//! nothing; E2 the allocator row is untouched (no wrap, no reset, no rebuild);
//! E3 replaying Register rev1 is AlreadyApplied and `ensure_healthcheck`
//! returns exactly H; S stays readable.
//!
//! Negative control (executed): [`allocator_verdict`] rejects the observations
//! a defaulting (counter reset to zero) or wrapping (counter becomes negative)
//! allocator would leave behind.

use super::*;
use awaken_executable_environment_contract::WorkQueueError;

const ENV: &str = "exhausted";

/// E2: the allocator row after the fault equals the row before it.
fn allocator_verdict(before: &(String, i64), after: &(String, i64)) -> Result<(), String> {
    if after.0 != before.0 {
        return Err(format!(
            "E2 generation rebuilt: {} -> {}",
            before.0, after.0
        ));
    }
    if after.1 != before.1 {
        return Err(format!("E2 counter moved: {} -> {}", before.1, after.1));
    }
    Ok(())
}

struct Replay {
    new_allocation: WorkQueueError,
    healthcheck: String,
}

/// E1 + E3 through one handle.
async fn replay_at_exhaustion<Q: WorkBackend>(queue: &Q, session: &str) -> Replay {
    assert_eq!(
        queue
            .apply_environment_revision(
                ENV,
                EnvironmentRevision(1),
                Operation::Register { healthcheck: true },
            )
            .await
            .unwrap(),
        Outcome::AlreadyApplied,
        "E3 exact replay of the committed registration"
    );
    let healthcheck = queue.ensure_healthcheck(ENV).await.unwrap();
    assert!(
        queue.get("existing", session).await.unwrap().is_some(),
        "E3 existing Session Work"
    );
    let new_allocation = queue
        .enqueue_session("new", &"new".into())
        .await
        .expect_err("E1 exhausted allocator fails closed");
    assert!(queue.list("new").await.unwrap().is_empty(), "E1 no row");
    Replay {
        new_allocation,
        healthcheck,
    }
}

/// Seed C1 and return (healthcheck id, Session id).
async fn seed<Q: WorkBackend>(queue: &Q) -> (String, String) {
    assert_eq!(
        queue
            .apply_environment_revision(
                ENV,
                EnvironmentRevision(1),
                Operation::Register { healthcheck: true },
            )
            .await
            .unwrap(),
        Outcome::Applied
    );
    let healthcheck = queue.ensure_healthcheck(ENV).await.unwrap();
    let session = queue
        .enqueue_session("existing", &"live".into())
        .await
        .unwrap();
    (healthcheck, session)
}

fn assert_negative_control(before: &(String, i64)) {
    for mutant in [
        (before.0.clone(), 0),
        (before.0.clone(), i64::MIN),
        (String::new(), 0),
    ] {
        assert!(
            allocator_verdict(before, &mutant).is_err(),
            "NC: a defaulting/wrapping/rebuilt allocator {mutant:?} is detected"
        );
    }
}

const EXHAUST: &str = "UPDATE work_queue_identity_allocator SET next_sequence=9223372036854775807";

#[tokio::test]
async fn sqlite_exhausted_allocator_still_replays_committed_results_across_handles_and_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("exhausted.db");
    let first = SqliteWorkQueue::open(path.to_str().unwrap(), awaken_time::system_clock())
        .await
        .unwrap();
    let (healthcheck, session) = seed(&first).await;
    let pool = sqlite_pool(&path).await;
    sqlx::query(EXHAUST).execute(&pool).await.unwrap();
    let before = sqlite_state(&pool).await;
    assert_eq!(before.1, i64::MAX);

    let peer = SqliteWorkQueue::open(path.to_str().unwrap(), awaken_time::system_clock())
        .await
        .unwrap();
    let on_first = replay_at_exhaustion(&first, &session).await;
    let on_peer = replay_at_exhaustion(&peer, &session).await;
    drop((first, peer));
    let reopened = SqliteWorkQueue::open(path.to_str().unwrap(), awaken_time::system_clock())
        .await
        .expect("an exhausted but well-formed allocator reopens");
    let on_reopen = replay_at_exhaustion(&reopened, &session).await;
    for replay in [&on_first, &on_peer, &on_reopen] {
        assert_eq!(replay.healthcheck, healthcheck, "E3 original healthcheck");
    }
    assert_eq!(on_first.new_allocation, on_peer.new_allocation, "E1 stable");
    assert_eq!(
        on_first.new_allocation, on_reopen.new_allocation,
        "E1 stable"
    );
    allocator_verdict(&before, &sqlite_state(&pool).await)
        .unwrap_or_else(|failure| panic!("{failure}"));
    assert_negative_control(&before);
    pool.close().await;
}

#[tokio::test]
async fn postgres_exhausted_allocator_still_replays_committed_results_across_pools() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let fixture = WorkPostgresFixture::new(&url, 4).await;
    let first = PostgresWorkQueue::with_pool(fixture.pool.clone(), awaken_time::system_clock())
        .await
        .unwrap();
    let (healthcheck, session) = seed(&first).await;
    sqlx::query(EXHAUST).execute(&fixture.pool).await.unwrap();
    let before = pg_state(&fixture.pool).await;
    let peer =
        PostgresWorkQueue::connect_existing(&fixture.scoped_url, awaken_time::system_clock())
            .await
            .unwrap();
    let on_first = replay_at_exhaustion(&first, &session).await;
    let on_peer = replay_at_exhaustion(&peer, &session).await;
    drop((first, peer));
    let reopened =
        PostgresWorkQueue::connect_existing(&fixture.scoped_url, awaken_time::system_clock())
            .await
            .expect("an exhausted but well-formed allocator reopens");
    let on_reopen = replay_at_exhaustion(&reopened, &session).await;
    for replay in [&on_first, &on_peer, &on_reopen] {
        assert_eq!(replay.healthcheck, healthcheck, "E3 original healthcheck");
    }
    assert_eq!(on_first.new_allocation, on_peer.new_allocation, "E1 stable");
    assert_eq!(
        on_first.new_allocation, on_reopen.new_allocation,
        "E1 stable"
    );
    allocator_verdict(&before, &pg_state(&fixture.pool).await)
        .unwrap_or_else(|failure| panic!("{failure}"));
    assert_negative_control(&before);
    drop(reopened);
    fixture.cleanup().await;
}

#[test]
fn postgres_exhausted_allocator_still_replays_committed_results_across_pools_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "durable::exhaustion::postgres_exhausted_allocator_still_replays_committed_results_across_pools",
    );
}
