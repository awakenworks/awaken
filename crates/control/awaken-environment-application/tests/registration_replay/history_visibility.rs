//! T21-C01: after an acknowledged withdrawal, a late registration replay can
//! restore no Work on any backend, while the original history stays readable.
//!
//! Cause/effect graph: C1 Register v1 then acknowledged Withdraw v2 commit on
//! handle A; C2 the store is reached again through a reconnected handle (file
//! SQLite: a new connection after drop; PostgreSQL: an independent pool); C3 the
//! delayed Register v1 arrives through a cold Coordinator. Effects: E1 no Work
//! row is re-created; E2 the retained watermark still reads (v2, Withdraw): the
//! exact replay is AlreadyApplied, a conflicting v2 Register is refused as a
//! Conflict naming Withdraw, and v1 is Superseded -- reading history never
//! re-grants admission.
//!
//! Negative control (executed): a "reconnect" that yields a fresh non-durable
//! queue (a purely sequential stub) cannot pass the delayed-registration check.

use super::*;
use awaken_executable_environment_contract::{
    EnvironmentWorkRevisionError, EnvironmentWorkRevisionOperation as Operation,
    EnvironmentWorkRevisionOutcome as Outcome,
};

/// E1 + E2 through one (reconnected) handle.
async fn history_is_readable_but_regrants_nothing<Q>(work: &Q)
where
    Q: EnvironmentWorkLifecycle + WorkQueries,
{
    assert_eq!(
        work.apply_environment_revision(
            "delayed-replay",
            EnvironmentRevision(2),
            Operation::Withdraw
        )
        .await
        .unwrap(),
        Outcome::AlreadyApplied,
        "E2 the acknowledged withdrawal is still the retained fact"
    );
    assert!(
        matches!(
            work.apply_environment_revision(
                "delayed-replay",
                EnvironmentRevision(2),
                Operation::Register { healthcheck: true },
            )
            .await,
            Err(EnvironmentWorkRevisionError::Conflict {
                current: Operation::Withdraw,
                ..
            })
        ),
        "E2 the history names Withdraw and refuses a different v2"
    );
    assert_eq!(
        work.apply_environment_revision(
            "delayed-replay",
            EnvironmentRevision(1),
            Operation::Register { healthcheck: true },
        )
        .await
        .unwrap(),
        Outcome::Superseded,
        "E2 the older registration reads as history, not as a grant"
    );
    assert!(
        work.list("delayed-replay").await.unwrap().is_empty(),
        "E1 reading history created no Work"
    );
}

#[tokio::test]
async fn volatile_withdrawn_history_stays_readable_without_regranting_admission() {
    let work = Arc::new(InMemoryWorkQueue::new(awaken_time::system_clock()));
    acknowledged_withdrawal(work.clone()).await;
    delayed_registration(work.clone()).await;
    history_is_readable_but_regrants_nothing(work.as_ref()).await;
}

#[tokio::test]
async fn sqlite_withdrawn_history_stays_readable_on_independent_handles_after_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("work.db");
    let path = path.to_str().unwrap();
    let open = move || async move {
        Arc::new(
            SqliteWorkQueue::open(path, awaken_time::system_clock())
                .await
                .unwrap(),
        )
    };
    let first = open().await;
    acknowledged_withdrawal(first.clone()).await;
    drop(first);
    let replayed = open().await;
    let observer = open().await;
    delayed_registration(replayed.clone()).await;
    history_is_readable_but_regrants_nothing(observer.as_ref()).await;
    drop((replayed, observer));
    history_is_readable_but_regrants_nothing(open().await.as_ref()).await;
}

#[tokio::test]
async fn postgres_withdrawn_history_stays_readable_on_independent_pools() {
    let Some(fixture) = PostgresFixture::configured().await else {
        return;
    };
    let first = Arc::new(
        PostgresWorkQueue::with_pool(fixture.pool.clone(), awaken_time::system_clock())
            .await
            .unwrap(),
    );
    acknowledged_withdrawal(first.clone()).await;
    drop(first);
    let replay_pool = PostgresFixture::connect(&fixture.options).await;
    let replayed = Arc::new(
        PostgresWorkQueue::with_pool(replay_pool.clone(), awaken_time::system_clock())
            .await
            .unwrap(),
    );
    delayed_registration(replayed).await;
    let observer_pool = PostgresFixture::connect(&fixture.options).await;
    let observer = PostgresWorkQueue::with_pool(observer_pool.clone(), awaken_time::system_clock())
        .await
        .unwrap();
    history_is_readable_but_regrants_nothing(&observer).await;
    drop(observer);
    replay_pool.close().await;
    observer_pool.close().await;
    fixture.cleanup().await;
}

async fn rejects_lost_withdrawal(probe: impl std::future::Future<Output = ()> + 'static) -> bool {
    let local = tokio::task::LocalSet::new();
    let Err(error) = local.run_until(local.spawn_local(probe)).await else {
        return false;
    };
    if !error.is_panic() {
        return false;
    }
    let payload = error.into_panic();
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied());
    message == Some("late revision 1 resurrected work after acknowledged withdrawal 2")
}

#[tokio::test]
async fn unrelated_faults_never_qualify_lost_withdrawal_detection() {
    assert!(!rejects_lost_withdrawal(async { panic!("work queue unavailable") }).await);
    assert!(!rejects_lost_withdrawal(async {}).await);
}

#[tokio::test]
async fn a_non_durable_reconnect_cannot_pass_the_delayed_registration_check() {
    let detected = rejects_lost_withdrawal(async {
        acknowledged_withdrawal(Arc::new(
            InMemoryWorkQueue::new(awaken_time::system_clock()),
        ))
        .await;
        // The "reconnected" handle is a fresh sequential stub with no
        // durable authority: the late v1 must look like a new grant there.
        delayed_registration(Arc::new(
            InMemoryWorkQueue::new(awaken_time::system_clock()),
        ))
        .await;
    })
    .await;
    assert!(
        detected,
        "NC: a non-durable reconnect must lose the withdrawal authority"
    );
}

#[test]
fn postgres_withdrawn_history_stays_readable_on_independent_pools_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "history_visibility::postgres_withdrawn_history_stays_readable_on_independent_pools",
    );
}

#[test]
fn postgres_delayed_registration_after_reconnect_cannot_restore_withdrawn_work_fixture_admission() {
    awaken_reliability_testkit::assert_postgres_fixture_child_contract(
        "postgres_delayed_registration_after_reconnect_cannot_restore_withdrawn_work",
    );
}
