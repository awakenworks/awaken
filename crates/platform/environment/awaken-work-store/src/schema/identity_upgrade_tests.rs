//! Published history upgrades preserve live leases and fence deleted numeric identities.
use super::*;
use crate::{LeaseBook, PostgresWorkQueue, SqliteWorkQueue, test_session_access_digest};
use awaken_executable_environment_contract::{
    SessionAccessAuthenticator, SessionWorkStore, WorkQueries,
};
use awaken_sqlite_pool::EmbeddedSqlite;

const LIVE: &str = "work_0000000000000000";
const DELETED: &str = "work_0000000000009000";
const LIVE_ROW: &str = "INSERT INTO work_queue_item(work_id,seq,environment_id,data_type,data_id,metadata_json,state,created_at,lease_owner,lease_epoch,lease_expires_ms,lease_refreshed_ms,session_token_sha256) VALUES('work_0000000000000000',0,'legacy','session','live','{}','active','2026-01-01T00:00:00Z','legacy-owner',7,60000,0,'000000000000000000000000000000000000000000000000000000000000003b')";
const RETIRED_ROW: &str = "INSERT INTO work_queue_item(work_id,seq,environment_id,data_type,data_id,metadata_json,state,created_at) VALUES('work_0000000000009000',9000,'deleted','session','retired','{}','queued','2026-01-01T00:00:00Z')";

fn old_convergence() -> MigrationBundle {
    MigrationBundle::new(
        CONVERGED_BUNDLE_ID,
        converged_work_bundle().unwrap().migrations()[..2].to_vec(),
    )
    .unwrap()
}

#[tokio::test]
async fn sqlite_both_published_histories_preserve_live_leases_and_fence_deleted_ids() {
    for legacy in [false, true] {
        let pool = EmbeddedSqlite::memory().open().await.unwrap();
        let runner = SqliteMigrationRunner::with_prefix(pool.sqlx().clone(), NS).unwrap();
        let published = if legacy {
            legacy_work_bundle().unwrap()
        } else {
            work_bundle().unwrap()
        };
        runner.run_bundle(&published).await.unwrap();
        runner.run_bundle(&old_convergence()).await.unwrap();
        sqlx::query(LIVE_ROW).execute(&pool).await.unwrap();
        sqlx::query(RETIRED_ROW).execute(&pool).await.unwrap();
        sqlx::query("DELETE FROM work_queue_item WHERE environment_id='deleted'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT MAX(seq) FROM work_queue_item")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        apply_sqlite_migrations(&pool).await.unwrap();
        let before: (String, i64) =
            sqlx::query_as("SELECT generation_id,next_sequence FROM work_queue_identity_allocator")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(before.0.len(), 32);
        assert_eq!(
            before.1, 0,
            "migration creates independent identity authority; remaining MAX cannot reconstruct deleted history"
        );
        // The legacy lease was granted on the epoch-zero timeline; the store
        // judges it on its own clock, here still before that lease's deadline.
        let queue = SqliteWorkQueue {
            pool: pool.clone(),
            book: LeaseBook::default(),
            clock: std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0)),
        };
        let digest = test_session_access_digest(59);
        let access = queue
            .authenticate_session_access(&digest)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(access.work_id, LIVE);
        assert_eq!(access.lease_owner, "legacy-owner");
        assert_eq!(access.expires_at_unix_ms, 60000);
        let live = queue.get("legacy", LIVE).await.unwrap().unwrap();
        assert_eq!(live.id, LIVE);
        assert_eq!(live.created_at, "2026-01-01T00:00:00Z");
        let next = queue.enqueue_session("new", &"next".into()).await.unwrap();
        assert_eq!(next, format!("work_g{}{:016x}", before.0, 1));
        assert_ne!(next, LIVE);
        assert_ne!(next, DELETED);
        assert!(queue.get("deleted", DELETED).await.unwrap().is_none());
        let after: (String, i64) =
            sqlx::query_as("SELECT generation_id,next_sequence FROM work_queue_identity_allocator")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(after, (before.0.clone(), 2));
        apply_sqlite_migrations(&pool).await.unwrap();
        let reopened: (String, i64) =
            sqlx::query_as("SELECT generation_id,next_sequence FROM work_queue_identity_allocator")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            reopened, after,
            "migration replay never replaces generation or counter"
        );
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL; run scripts/ci/pg_tests.sh --require-docker"]
async fn postgres_both_published_histories_preserve_live_leases_and_fence_deleted_ids() {
    // A1 optional+absent reports the canonical local skip; A2 required+absent
    // or invalid configuration fails at admission/setup; A3 live runs both
    // published histories and every lease/retired-id/replay assertion below.
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
        let published = if legacy {
            legacy_work_bundle().unwrap()
        } else {
            work_bundle().unwrap()
        };
        runner.run_bundle(&published).await.unwrap();
        runner.run_bundle(&old_convergence()).await.unwrap();
        sqlx::query(LIVE_ROW).execute(&pool).await.unwrap();
        sqlx::query(RETIRED_ROW).execute(&pool).await.unwrap();
        sqlx::query("DELETE FROM work_queue_item WHERE environment_id='deleted'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT MAX(seq) FROM work_queue_item")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        apply_postgres_migrations(&pool).await.unwrap();
        let before: (String, i64) =
            sqlx::query_as("SELECT generation_id,next_sequence FROM work_queue_identity_allocator")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(before.0.len(), 32);
        assert_eq!(
            before.1, 0,
            "migration creates independent identity authority; remaining MAX cannot reconstruct deleted history"
        );
        // The legacy lease is judged on the store clock, before its deadline.
        let queue = PostgresWorkQueue::with_existing_pool(
            pool.clone(),
            std::sync::Arc::new(awaken_lease_testkit::ManualClock::new(0)),
        )
        .await
        .unwrap();
        let digest = test_session_access_digest(59);
        let access = queue
            .authenticate_session_access(&digest)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(access.work_id, LIVE);
        assert_eq!(access.lease_owner, "legacy-owner");
        assert_eq!(access.expires_at_unix_ms, 60000);
        let live = queue.get("legacy", LIVE).await.unwrap().unwrap();
        assert_eq!(live.id, LIVE);
        assert_eq!(live.created_at, "2026-01-01T00:00:00Z");
        let next = queue.enqueue_session("new", &"next".into()).await.unwrap();
        assert_eq!(next, format!("work_g{}{:016x}", before.0, 1));
        assert_ne!(next, LIVE);
        assert_ne!(next, DELETED);
        assert!(queue.get("deleted", DELETED).await.unwrap().is_none());
        let after: (String, i64) =
            sqlx::query_as("SELECT generation_id,next_sequence FROM work_queue_identity_allocator")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(after, (before.0.clone(), 2));
        apply_postgres_migrations(&pool).await.unwrap();
        let reopened: (String, i64) =
            sqlx::query_as("SELECT generation_id,next_sequence FROM work_queue_identity_allocator")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            reopened, after,
            "migration replay never replaces generation or counter"
        );
        drop(queue);
        fixture.cleanup().await;
    }
}
