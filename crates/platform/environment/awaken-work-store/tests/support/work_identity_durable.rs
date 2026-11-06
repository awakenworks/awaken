//! Durable allocation witnesses, using independent clients and SQL fault injection.
use super::work_postgres_fixture::WorkPostgresFixture;
use awaken_environment_contract::EnvironmentRevision;
use awaken_executable_environment_contract::{
    EnvironmentWorkLifecycle, EnvironmentWorkRevisionOperation as Operation,
    EnvironmentWorkRevisionOutcome as Outcome, SessionWorkStore, WorkQueries,
};
use awaken_store_conformance::work_queue::WorkBackend;
use awaken_work_store::{PostgresWorkQueue, SqliteWorkQueue};

#[path = "work_identity_durable_exhaustion.rs"]
mod exhaustion;
#[path = "work_identity_durable_nc.rs"]
mod nc;

async fn concurrent<Q: WorkBackend>(a: &Q, b: &Q) -> Vec<String> {
    let mut ids = Vec::new();
    for ordinal in 0..16 {
        let first = format!("first-{ordinal}").into();
        let second = format!("second-{ordinal}").into();
        let (left, right) = tokio::join!(
            a.enqueue_session("concurrent", &first),
            b.enqueue_session("concurrent", &second)
        );
        ids.extend([left.unwrap(), right.unwrap()]);
    }
    let unique: std::collections::BTreeSet<_> = ids.iter().collect();
    assert_eq!(
        unique.len(),
        32,
        "independent handles must allocate distinct identities"
    );
    let rows = a.list("concurrent").await.unwrap();
    assert_eq!(rows.len(), 32);
    assert_eq!(
        rows.iter()
            .map(|row| &row.id)
            .collect::<std::collections::BTreeSet<_>>(),
        unique
    );
    // Delete the tail and every other row, then allocate again through the
    // other physical handle. No remaining Work row can serve as the counter.
    a.remove_env("concurrent").await.unwrap();
    let next = b
        .enqueue_session("concurrent", &"after-clear".into())
        .await
        .unwrap();
    assert!(!ids.contains(&next));
    assert_eq!(a.list("concurrent").await.unwrap().len(), 1);
    ids.push(next);
    ids
}

#[tokio::test]
async fn sqlite_independent_handles_allocate_concurrently_and_reopen_without_reset() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("allocation.db");
    let a = SqliteWorkQueue::open(path.to_str().unwrap(), awaken_time::system_clock())
        .await
        .unwrap();
    let b = SqliteWorkQueue::open(path.to_str().unwrap(), awaken_time::system_clock())
        .await
        .unwrap();
    let ids = concurrent(&a, &b).await;
    drop((a, b));
    let c = SqliteWorkQueue::open(path.to_str().unwrap(), awaken_time::system_clock())
        .await
        .unwrap();
    let next = c.enqueue_session("reopened", &"new".into()).await.unwrap();
    assert!(!ids.contains(&next));
    let pool = sqlite_pool(&path).await;
    assert_eq!(sqlite_state(&pool).await.1, 34);
    assert_eq!(c.list("concurrent").await.unwrap()[0].id, ids[32]);
    pool.close().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL; run scripts/ci/pg_tests.sh --require-docker"]
async fn postgres_independent_pools_allocate_concurrently_without_reset() {
    // A1 optional+absent reports a local skip; A2 required+absent or invalid
    // configuration fails; A3 live verifies independent clients, concurrent
    // allocation and reopen against this one isolated physical schema.
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    let fixture = WorkPostgresFixture::new(&url, 4).await;
    let a = PostgresWorkQueue::with_pool(fixture.pool.clone(), awaken_time::system_clock())
        .await
        .unwrap();
    let b = PostgresWorkQueue::connect_existing(&fixture.scoped_url, awaken_time::system_clock())
        .await
        .unwrap();
    let ids = concurrent(&a, &b).await;
    drop((a, b));
    let c = PostgresWorkQueue::connect_existing(&fixture.scoped_url, awaken_time::system_clock())
        .await
        .unwrap();
    let next = c.enqueue_session("reopened", &"new".into()).await.unwrap();
    assert!(!ids.contains(&next));
    assert_eq!(pg_state(&fixture.pool).await.1, 34);
    drop(c);
    fixture.cleanup().await;
}

async fn sqlite_pool(path: &std::path::Path) -> sqlx::SqlitePool {
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(path))
        .await
        .unwrap()
}

async fn sqlite_state(pool: &sqlx::SqlitePool) -> (String, i64) {
    sqlx::query_as(
        "SELECT generation_id,next_sequence FROM work_queue_identity_allocator WHERE singleton=1",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn pg_state(pool: &sqlx::PgPool) -> (String, i64) {
    sqlx::query_as(
        "SELECT generation_id,next_sequence FROM work_queue_identity_allocator WHERE singleton=1",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn sqlite_allocator_work_and_watermark_roll_back_as_one_transaction() {
    for table in [
        "work_queue_identity_allocator",
        "work_queue_item",
        "work_queue_environment_revision",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollback.db");
        let queue = SqliteWorkQueue::open(path.to_str().unwrap(), awaken_time::system_clock())
            .await
            .unwrap();
        let pool = sqlite_pool(&path).await;
        let before = sqlite_state(&pool).await;
        let event = if table.ends_with("allocator") {
            "UPDATE"
        } else {
            "INSERT"
        };
        sqlx::raw_sql(&format!("CREATE TRIGGER identity_failure BEFORE {event} ON {table} BEGIN SELECT RAISE(ABORT,'identity rollback witness'); END;")).execute(&pool).await.unwrap();
        let failure = queue
            .apply_environment_revision(
                "atomic",
                EnvironmentRevision(1),
                Operation::Register { healthcheck: true },
            )
            .await
            .unwrap_err();
        assert!(failure.to_string().contains("identity rollback witness"));
        assert_eq!(
            sqlite_state(&pool).await,
            before,
            "{table} must roll allocation back"
        );
        assert!(queue.list("atomic").await.unwrap().is_empty());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM work_queue_environment_revision")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        sqlx::raw_sql("DROP TRIGGER identity_failure")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            queue
                .apply_environment_revision(
                    "atomic",
                    EnvironmentRevision(1),
                    Operation::Register { healthcheck: true }
                )
                .await
                .unwrap(),
            Outcome::Applied
        );
        let committed = sqlite_state(&pool).await;
        assert_eq!(committed, (before.0, before.1 + 1));
        assert_eq!(
            queue
                .apply_environment_revision(
                    "atomic",
                    EnvironmentRevision(1),
                    Operation::Register { healthcheck: true }
                )
                .await
                .unwrap(),
            Outcome::AlreadyApplied
        );
        let id = queue.ensure_healthcheck("atomic").await.unwrap();
        assert_eq!(queue.ensure_healthcheck("atomic").await.unwrap(), id);
        assert_eq!(
            sqlite_state(&pool).await,
            committed,
            "lost-response replay must not allocate again"
        );
        pool.close().await;
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL; run scripts/ci/pg_tests.sh --require-docker"]
async fn postgres_allocator_work_and_watermark_roll_back_as_one_transaction() {
    // A1 optional+absent reports a local skip; A2 required+absent or invalid
    // configuration fails; A3 live injects each SQL failure and checks the
    // allocator, Work and revision rollback in the same transaction.
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    for table in [
        "work_queue_identity_allocator",
        "work_queue_item",
        "work_queue_environment_revision",
    ] {
        let fixture = WorkPostgresFixture::new(&url, 4).await;
        let queue = PostgresWorkQueue::with_pool(fixture.pool.clone(), awaken_time::system_clock())
            .await
            .unwrap();
        let before = pg_state(&fixture.pool).await;
        let event = if table.ends_with("allocator") {
            "UPDATE"
        } else {
            "INSERT"
        };
        sqlx::raw_sql(&format!("CREATE FUNCTION fail_identity() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'identity rollback witness'; END; $$; CREATE TRIGGER identity_failure BEFORE {event} ON {table} FOR EACH ROW EXECUTE FUNCTION fail_identity();")).execute(&fixture.pool).await.unwrap();
        let failure = queue
            .apply_environment_revision(
                "atomic",
                EnvironmentRevision(1),
                Operation::Register { healthcheck: true },
            )
            .await
            .unwrap_err();
        assert!(failure.to_string().contains("identity rollback witness"));
        assert_eq!(
            pg_state(&fixture.pool).await,
            before,
            "{table} must roll allocation back"
        );
        assert!(queue.list("atomic").await.unwrap().is_empty());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM work_queue_environment_revision")
                .fetch_one(&fixture.pool)
                .await
                .unwrap(),
            0
        );
        sqlx::raw_sql(&format!(
            "DROP TRIGGER identity_failure ON {table}; DROP FUNCTION fail_identity()"
        ))
        .execute(&fixture.pool)
        .await
        .unwrap();
        assert_eq!(
            queue
                .apply_environment_revision(
                    "atomic",
                    EnvironmentRevision(1),
                    Operation::Register { healthcheck: true }
                )
                .await
                .unwrap(),
            Outcome::Applied
        );
        let committed = pg_state(&fixture.pool).await;
        assert_eq!(committed, (before.0, before.1 + 1));
        assert_eq!(
            queue
                .apply_environment_revision(
                    "atomic",
                    EnvironmentRevision(1),
                    Operation::Register { healthcheck: true }
                )
                .await
                .unwrap(),
            Outcome::AlreadyApplied
        );
        let id = queue.ensure_healthcheck("atomic").await.unwrap();
        assert_eq!(queue.ensure_healthcheck("atomic").await.unwrap(), id);
        assert_eq!(
            pg_state(&fixture.pool).await,
            committed,
            "lost-response replay must not allocate again"
        );
        drop(queue);
        fixture.cleanup().await;
    }
}

#[tokio::test]
async fn sqlite_missing_malformed_and_exhausted_allocators_fail_closed() {
    for fault in ["missing", "malformed", "exhausted"] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("damaged.db");
        let queue = SqliteWorkQueue::open(path.to_str().unwrap(), awaken_time::system_clock())
            .await
            .unwrap();
        let old = queue
            .enqueue_session("existing", &"live".into())
            .await
            .unwrap();
        let pool = sqlite_pool(&path).await;
        let statement = match fault {
            "missing" => "DELETE FROM work_queue_identity_allocator",
            "malformed" => {
                "UPDATE work_queue_identity_allocator SET generation_id='ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ'"
            }
            _ => "UPDATE work_queue_identity_allocator SET next_sequence=9223372036854775807",
        };
        sqlx::query(statement).execute(&pool).await.unwrap();
        assert!(
            queue.enqueue_session("new", &"new".into()).await.is_err(),
            "{fault}"
        );
        assert_eq!(queue.get("existing", &old).await.unwrap().unwrap().id, old);
        assert!(queue.list("new").await.unwrap().is_empty());
        drop(queue);
        let reopened =
            SqliteWorkQueue::open(path.to_str().unwrap(), awaken_time::system_clock()).await;
        if fault == "exhausted" {
            assert!(reopened.is_ok());
        } else {
            assert!(reopened.is_err());
        }
        let rows =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM work_queue_identity_allocator")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            rows,
            if fault == "missing" { 0 } else { 1 },
            "reopen must never regenerate authority"
        );
        pool.close().await;
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL; run scripts/ci/pg_tests.sh --require-docker"]
async fn postgres_missing_malformed_and_exhausted_allocators_fail_closed() {
    // A1 optional+absent reports a local skip; A2 required+absent or invalid
    // configuration fails; A3 live runs all three allocator faults without
    // reconstructing a missing or malformed identity authority on reopen.
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    for fault in ["missing", "malformed", "exhausted"] {
        let fixture = WorkPostgresFixture::new(&url, 4).await;
        let queue = PostgresWorkQueue::with_pool(fixture.pool.clone(), awaken_time::system_clock())
            .await
            .unwrap();
        let old = queue
            .enqueue_session("existing", &"live".into())
            .await
            .unwrap();
        let statement = match fault {
            "missing" => "DELETE FROM work_queue_identity_allocator",
            "malformed" => {
                "UPDATE work_queue_identity_allocator SET generation_id='ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ'"
            }
            _ => "UPDATE work_queue_identity_allocator SET next_sequence=9223372036854775807",
        };
        sqlx::query(statement).execute(&fixture.pool).await.unwrap();
        assert!(
            queue.enqueue_session("new", &"new".into()).await.is_err(),
            "{fault}"
        );
        assert_eq!(queue.get("existing", &old).await.unwrap().unwrap().id, old);
        assert!(queue.list("new").await.unwrap().is_empty());
        drop(queue);
        let reopened = PostgresWorkQueue::with_existing_pool(
            fixture.pool.clone(),
            awaken_time::system_clock(),
        )
        .await;
        if fault == "exhausted" {
            assert!(reopened.is_ok());
        } else {
            assert!(reopened.is_err());
        }
        let rows =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM work_queue_identity_allocator")
                .fetch_one(&fixture.pool)
                .await
                .unwrap();
        assert_eq!(
            rows,
            if fault == "missing" { 0 } else { 1 },
            "verification must never regenerate authority"
        );
        drop(reopened);
        fixture.cleanup().await;
    }
}
