//! Published stopped rows do not identify their original caller retroactively.
use super::*;
use crate::{LeaseBook, PostgresWorkQueue, SqliteWorkQueue};
use awaken_executable_environment_contract::{WorkConsumer, WorkMutationResult, WorkQueries};
use awaken_lease_testkit::ManualClock;
use awaken_sqlite_pool::EmbeddedSqlite;
use std::sync::Arc;

const ROWS: [&str; 2] = [
    "INSERT INTO work_queue_item(work_id,seq,environment_id,data_type,data_id,metadata_json,state,created_at,stop_requested_at,stopped_at,lease_epoch) VALUES('old',0,'env','healthcheck','old','{}','stopped','2026-01-01T00:00:00Z','2026-01-01T00:00:01Z','2026-01-01T00:00:01Z',7)",
    "INSERT INTO work_queue_item(work_id,seq,environment_id,data_type,data_id,metadata_json,state,created_at,lease_owner,lease_epoch,lease_expires_ms,lease_refreshed_ms) VALUES('live',1,'env','healthcheck','live','{}','active','2026-01-01T00:00:00Z','owner',7,60000,0)",
];

fn before_stop(legacy: bool) -> MigrationBundle {
    let bundle = if legacy {
        legacy_work_bundle().unwrap()
    } else {
        work_bundle().unwrap()
    };
    MigrationBundle::new(
        BUNDLE_ID,
        bundle.migrations()[..if legacy { 8 } else { 3 }].to_vec(),
    )
    .unwrap()
}

async fn stop_and_retry<Q: WorkConsumer + WorkQueries>(queue: &Q, clock: &ManualClock) {
    assert!(
        matches!(
            queue.stop("env", "old", "owner").await.unwrap(),
            WorkMutationResult::PreconditionFailed
        ),
        "an old timestamp and generation cannot identify the historical stop owner"
    );
    assert_eq!(
        queue
            .get("env", "old")
            .await
            .unwrap()
            .unwrap()
            .stopped_at
            .as_deref(),
        Some("2026-01-01T00:00:01Z")
    );
    let first = queue
        .stop("env", "live", "owner")
        .await
        .unwrap()
        .into_item()
        .unwrap();
    assert_eq!(
        first.stopped_at,
        Some(awaken_time::utc::epoch_millis_to_rfc3339(100))
    );
    clock.set(60_001);
    let retry = queue
        .stop("env", "live", "owner")
        .await
        .unwrap()
        .into_item()
        .unwrap();
    assert_eq!(retry.stopped_at, first.stopped_at);
    assert_eq!(retry.stop_requested_at, first.stop_requested_at);
    assert!(matches!(
        queue.stop("env", "live", "foreign").await.unwrap(),
        WorkMutationResult::PreconditionFailed
    ));
    assert!(matches!(
        queue.ack("env", "live", "owner").await.unwrap(),
        WorkMutationResult::PreconditionFailed
    ));
}

const RECEIPT: &str =
    "SELECT stop_lease_owner,stop_lease_epoch FROM work_queue_item WHERE work_id='live'";
const HALF: &str = "UPDATE work_queue_item SET stop_lease_owner='invented' WHERE work_id='old'";

#[tokio::test]
async fn sqlite_both_published_histories_preserve_unknown_old_stop_subjects() {
    for legacy in [false, true] {
        let pool = EmbeddedSqlite::memory().open().await.unwrap();
        let runner = SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), NS).unwrap();
        runner.run_bundle(&before_stop(legacy)).await.unwrap();
        runner
            .run_bundle(&converged_work_bundle().unwrap())
            .await
            .unwrap();
        let columns: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info('work_queue_item')")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(columns.iter().any(|name| name == "ack_lease_epoch"));
        assert!(!columns.iter().any(|name| name == "stop_lease_owner"));
        for row in ROWS {
            sqlx::query(row).execute(&pool).await.unwrap();
        }
        apply_sqlite_migrations(&pool).await.unwrap();
        let old: (Option<String>, Option<i64>) = sqlx::query_as(
            "SELECT stop_lease_owner,stop_lease_epoch FROM work_queue_item WHERE work_id='old'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(old, (None, None));
        let clock = Arc::new(ManualClock::new(100));
        let queue = SqliteWorkQueue {
            pool: pool.clone(),
            book: LeaseBook::default(),
            clock: clock.clone(),
        };
        stop_and_retry(&queue, &clock).await;
        assert_eq!(
            sqlx::query_as::<_, (Option<String>, Option<i64>)>(RECEIPT)
                .fetch_one(&pool)
                .await
                .unwrap(),
            (Some("owner".into()), Some(7))
        );
        assert!(
            sqlx::query(HALF).execute(&pool).await.is_err(),
            "half a receipt cannot commit"
        );
        apply_sqlite_migrations(&pool).await.unwrap();
        assert_eq!(
            sqlx::query_as::<_, (Option<String>, Option<i64>)>(RECEIPT)
                .fetch_one(&pool)
                .await
                .unwrap(),
            (Some("owner".into()), Some(7)),
            "migration replay preserves the receipt"
        );
        drop(queue);
        pool.close().await;
    }
}

#[tokio::test]
async fn postgres_both_published_histories_preserve_unknown_old_stop_subjects() {
    let Some(url) = awaken_reliability_testkit::postgres_fixture_url() else {
        return;
    };
    for legacy in [false, true] {
        let fixture = crate::work_postgres_fixture::WorkPostgresFixture::new(&url, 2).await;
        let pool = fixture.pool.clone();
        let runner = awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(
            pool.clone(),
            NS,
        )
        .unwrap();
        runner.run_bundle(&before_stop(legacy)).await.unwrap();
        runner
            .run_bundle(&converged_work_bundle().unwrap())
            .await
            .unwrap();
        let columns: Vec<String> = sqlx::query_scalar("SELECT column_name FROM information_schema.columns WHERE table_schema=current_schema() AND table_name='work_queue_item'").fetch_all(&pool).await.unwrap();
        assert!(columns.iter().any(|name| name == "ack_lease_epoch"));
        assert!(!columns.iter().any(|name| name == "stop_lease_owner"));
        for row in ROWS {
            sqlx::query(row).execute(&pool).await.unwrap();
        }
        apply_postgres_migrations(&pool).await.unwrap();
        let old: (Option<String>, Option<i64>) = sqlx::query_as(
            "SELECT stop_lease_owner,stop_lease_epoch FROM work_queue_item WHERE work_id='old'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(old, (None, None));
        let clock = Arc::new(ManualClock::new(100));
        let queue = PostgresWorkQueue::with_existing_pool(pool.clone(), clock.clone())
            .await
            .unwrap();
        stop_and_retry(&queue, &clock).await;
        assert_eq!(
            sqlx::query_as::<_, (Option<String>, Option<i64>)>(RECEIPT)
                .fetch_one(&pool)
                .await
                .unwrap(),
            (Some("owner".into()), Some(7))
        );
        assert!(
            sqlx::query(HALF).execute(&pool).await.is_err(),
            "half a receipt cannot commit"
        );
        apply_postgres_migrations(&pool).await.unwrap();
        assert_eq!(
            sqlx::query_as::<_, (Option<String>, Option<i64>)>(RECEIPT)
                .fetch_one(&pool)
                .await
                .unwrap(),
            (Some("owner".into()), Some(7)),
            "migration replay preserves the receipt"
        );
        drop(queue);
        fixture.cleanup().await;
    }
}
