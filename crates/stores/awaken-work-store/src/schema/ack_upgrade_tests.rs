//! An old timestamp is not evidence of which lease acknowledged the Work.
use super::*;
use crate::{LeaseBook, PostgresWorkQueue, SqliteWorkQueue};
use awaken_executable_environment_contract::{WorkConsumer, WorkMutationResult, WorkQueries};
use awaken_lease_testkit::ManualClock;
use awaken_sqlite_pool::EmbeddedSqlite;
use std::sync::Arc;

const OLD_ROW: &str = "INSERT INTO work_queue_item(work_id,seq,environment_id,data_type,data_id,metadata_json,state,created_at,acknowledged_at,lease_owner,lease_epoch,lease_expires_ms,lease_refreshed_ms) VALUES('old',0,'env','healthcheck','old','{}','active','2026-01-01T00:00:00Z','2026-01-01T00:00:01Z','owner',7,60000,0)";

fn before_ack(legacy: bool) -> MigrationBundle {
    let bundle = if legacy {
        legacy_work_bundle().unwrap()
    } else {
        work_bundle().unwrap()
    };
    MigrationBundle::new(
        BUNDLE_ID,
        // Freeze the genuine pre-ACK history even when later migrations append.
        bundle.migrations()[..if legacy { 7 } else { 2 }].to_vec(),
    )
    .unwrap()
}

async fn fresh_ack<Q: WorkConsumer + WorkQueries>(queue: &Q, clock: &ManualClock) {
    let before = queue.get("env", "old").await.unwrap().unwrap();
    assert_eq!(
        before.acknowledged_at.as_deref(),
        Some("2026-01-01T00:00:01Z")
    );
    assert!(
        matches!(
            queue.ack("env", "old", "owner").await.unwrap(),
            WorkMutationResult::PreconditionFailed
        ),
        "migration must not invent a replay identity for the old timestamp"
    );
    assert_eq!(
        queue
            .get("env", "old")
            .await
            .unwrap()
            .unwrap()
            .acknowledged_at,
        before.acknowledged_at
    );
    clock.set(100);
    let fresh = queue
        .ack("env", "old", "owner")
        .await
        .unwrap()
        .into_item()
        .unwrap();
    assert_eq!(
        fresh.acknowledged_at,
        Some(awaken_time::utc::epoch_millis_to_rfc3339(100))
    );
}

async fn replay<Q: WorkConsumer + WorkQueries>(queue: &Q) {
    let receipt = queue
        .ack("env", "old", "owner")
        .await
        .unwrap()
        .into_item()
        .unwrap();
    assert_eq!(
        receipt.acknowledged_at,
        Some(awaken_time::utc::epoch_millis_to_rfc3339(100)),
        "the committed generation receipt survives constructing another store at expiry"
    );
}

#[tokio::test]
async fn sqlite_both_published_histories_add_only_known_ack_generations() {
    for legacy in [false, true] {
        let pool = EmbeddedSqlite::memory().open().await.unwrap();
        let runner = SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), NS).unwrap();
        runner.run_bundle(&before_ack(legacy)).await.unwrap();
        runner
            .run_bundle(&converged_work_bundle().unwrap())
            .await
            .unwrap();
        sqlx::query(OLD_ROW).execute(&pool).await.unwrap();
        apply_sqlite_migrations(&pool).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, Option<i64>>(
                "SELECT ack_lease_epoch FROM work_queue_item WHERE work_id='old'"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            None
        );
        let clock = Arc::new(ManualClock::new(60_000));
        let queue = SqliteWorkQueue {
            pool: pool.clone(),
            book: LeaseBook::default(),
            clock: clock.clone(),
        };
        fresh_ack(&queue, &clock).await;
        drop(queue);
        apply_sqlite_migrations(&pool).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, Option<i64>>(
                "SELECT ack_lease_epoch FROM work_queue_item WHERE work_id='old'"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            Some(7)
        );
        let reopened = SqliteWorkQueue {
            pool: pool.clone(),
            book: LeaseBook::default(),
            clock: Arc::new(ManualClock::new(60_000)),
        };
        replay(&reopened).await;
        drop(reopened);
        pool.close().await;
    }
}

#[tokio::test]
async fn postgres_both_published_histories_add_only_known_ack_generations() {
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
        runner.run_bundle(&before_ack(legacy)).await.unwrap();
        runner
            .run_bundle(&converged_work_bundle().unwrap())
            .await
            .unwrap();
        sqlx::query(OLD_ROW).execute(&pool).await.unwrap();
        apply_postgres_migrations(&pool).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, Option<i64>>(
                "SELECT ack_lease_epoch FROM work_queue_item WHERE work_id='old'"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            None
        );
        let clock = Arc::new(ManualClock::new(60_000));
        let queue = PostgresWorkQueue::with_existing_pool(pool.clone(), clock.clone())
            .await
            .unwrap();
        fresh_ack(&queue, &clock).await;
        drop(queue);
        apply_postgres_migrations(&pool).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, Option<i64>>(
                "SELECT ack_lease_epoch FROM work_queue_item WHERE work_id='old'"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            Some(7)
        );
        let reopened = PostgresWorkQueue::connect_existing(
            &fixture.scoped_url,
            Arc::new(ManualClock::new(60_000)),
        )
        .await
        .unwrap();
        replay(&reopened).await;
        drop(reopened);
        fixture.cleanup().await;
    }
}
