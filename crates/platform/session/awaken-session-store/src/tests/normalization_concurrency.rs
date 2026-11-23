use super::*;
use sqlx::{Executor as _, postgres::PgConnectOptions};
use std::str::FromStr as _;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_normalization_serializes_before_a_root_writer_upgrades_its_lock() {
    // Causes: C1 a real writer holds SELECT FOR UPDATE (RowShare table lock);
    // C2 operational normalization starts before that writer's UPDATE needs
    // RowExclusive; C3 normalization rewrites root bytes or only repairs the
    // derived FK indexes. Effects: E1 neither transaction deadlocks; E2 the
    // writer commits before normalization reads/repairs; E3 root, indexes and
    // command receipts stay consistent and an ordinary read-only reopen works.
    // Rules N1: C1+C2+root rewrite; N2: C1+C2+index-only repair -> E1-E3.
    // A database lock-wait witness, not a sleep, establishes C2. The raw writer
    // uses the repository commit's exact FOR UPDATE -> UPDATE lock sequence;
    // it deliberately writes identical bytes, so it adds no business command.
    // The existing CAS tests own revision/idempotency semantics, and the atomic
    // normalization tests own injected-error rollback. This test owns their
    // physical lock interaction at an operational migration boundary.
    let Some((url, admin)) = postgres_test_admin().await else {
        return;
    };
    for rewrite in [false, true] {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let schema = format!("t_normalize_lock_{suffix}");
        admin
            // SQL safety: this fixture-owned identifier uses only a timestamp.
            .execute(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .await
            .unwrap();
        let scoped = scoped_postgres_url(&url, &schema);
        let pool = sqlx::PgPool::connect(&scoped).await.unwrap();
        let repo = PostgresManagedSessionRepository::with_pool(pool.clone())
            .await
            .unwrap();
        let mut root = create_fixture(
            &repo,
            "ws",
            sample_with_credential_source("normalization-writer", "source-a"),
            Vec::new(),
        )
        .await;
        if rewrite {
            let (original, _) = normalization_fixture(&mut root, false);
            overwrite_postgres_aggregate(&pool, root.session_id.as_str(), &original).await;
        }
        let before = postgres_session_snapshot(&pool).await;
        let migrator = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with(
                PgConnectOptions::from_str(&scoped)
                    .unwrap()
                    .application_name(&schema),
            )
            .await
            .unwrap();
        let mut writer = pool.begin().await.unwrap();
        sqlx::query("SELECT session_id FROM managed_session WHERE session_id=$1 FOR UPDATE")
            .bind(root.session_id.as_str())
            .fetch_one(&mut *writer)
            .await
            .unwrap();
        let migrating = tokio::spawn(async move {
            let result = PostgresManagedSessionRepository::with_pool(migrator.clone()).await;
            migrator.close().await;
            result.map(|_| ())
        });
        awaken_reliability_testkit::wait_for_postgres_lock_waiters(
            &pool,
            &schema,
            1,
            None,
            Duration::from_secs(10),
        )
        .await
        .expect("N1/N2 migration contends with the held root writer");
        let writing = async {
            sqlx::query("UPDATE managed_session SET revision=revision WHERE session_id=$1")
                .bind(root.session_id.as_str())
                .execute(&mut *writer)
                .await?;
            writer.commit().await
        };
        let (written, migrated) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(writing, migrating)
        })
        .await
        .expect("E1 both original transactions settle within their boundary");
        assert!(written.is_ok(), "E1 writer: {written:?}");
        assert!(
            migrated.as_ref().is_ok_and(Result::is_ok),
            "E1 migration: {migrated:?}"
        );
        assert_eq!(
            repo.get(&root.session_id).await.unwrap(),
            root,
            "E2 root retained"
        );
        let after = postgres_session_snapshot(&pool).await;
        assert_eq!(after.reconciliation, before.reconciliation, "E3 work index");
        assert_eq!(
            after.credential_sources, before.credential_sources,
            "E3 source index"
        );
        assert_eq!(after.vaults, before.vaults, "E3 Vault index");
        assert_eq!(after.receipts, before.receipts, "E3 no new command");
        PostgresManagedSessionRepository::with_existing_pool(pool.clone())
            .await
            .expect("E3 read-only server opener accepts the repaired state");
        pool.close().await;
        admin
            // SQL safety: the same fixture-owned timestamp identifier.
            .execute(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .await
            .unwrap();
    }
    admin.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_normalization_keeps_plain_reads_open_and_queues_new_row_writers() {
    // N3 reverses N1/N2's order: normalization already owns its root-table
    // lock when a new FOR UPDATE writer arrives. Hold only the fixture's source
    // index table to stop repair at its original DELETE, after root admission.
    // Effects: a plain repository read completes; a row-locking writer waits;
    // releasing the index barrier lets repair then writer settle, with no root
    // mutation. Each ordering is witnessed in PostgreSQL's owned lock waiters;
    // no timing delay or retry is used to manufacture the schedule.
    let Some((url, pool)) = postgres_test_admin().await else {
        return;
    };
    let repo = PostgresManagedSessionRepository::with_pool(pool.clone())
        .await
        .unwrap();
    let root = create_fixture(
        &repo,
        "ws",
        sample_with_credential_source("migration-first", "source-a"),
        Vec::new(),
    )
    .await;
    let app = format!("normalize-first-{}", std::process::id());
    let migrator = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(
            PgConnectOptions::from_str(&url)
                .unwrap()
                .application_name(&app),
        )
        .await
        .unwrap();
    let mut held_index = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE managed_session_credential_source_reference IN SHARE MODE")
        .execute(&mut *held_index)
        .await
        .unwrap();
    let migrating = tokio::spawn(async move {
        let result = PostgresManagedSessionRepository::with_pool(migrator.clone()).await;
        migrator.close().await;
        result.map(|_| ())
    });
    awaken_reliability_testkit::wait_for_postgres_lock_waiters(
        &pool,
        &app,
        1,
        Some("DELETE FROM managed_session_credential_source_reference"),
        Duration::from_secs(10),
    )
    .await
    .expect("N3 repair owns the root-table lock before its index write");
    let read = tokio::time::timeout(Duration::from_secs(3), repo.get(&root.session_id))
        .await
        .expect("N3 ACCESS SHARE reader must remain admitted")
        .unwrap();
    assert_eq!(read, root, "N3 plain read retains the original root");
    let writer_app = format!("{app}-writer");
    let writer = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect_with(
            PgConnectOptions::from_str(&url)
                .unwrap()
                .application_name(&writer_app),
        )
        .await
        .unwrap();
    let writing = tokio::spawn(async move {
        let result = sqlx::query("SELECT session_id FROM managed_session FOR UPDATE")
            .fetch_all(&writer)
            .await;
        writer.close().await;
        result.map(|_| ())
    });
    awaken_reliability_testkit::wait_for_postgres_lock_waiters(
        &pool,
        &writer_app,
        1,
        Some("FOR UPDATE"),
        Duration::from_secs(10),
    )
    .await
    .expect("N3 new row writer queues behind operational repair");
    held_index.commit().await.unwrap();
    let (migrated, written) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(migrating, writing)
    })
    .await
    .expect("N3 both transactions settle after the original barrier releases");
    assert!(
        migrated.as_ref().is_ok_and(Result::is_ok),
        "N3 {migrated:?}"
    );
    assert!(written.as_ref().is_ok_and(Result::is_ok), "N3 {written:?}");
    assert_eq!(
        repo.get(&root.session_id).await.unwrap(),
        root,
        "N3 no mutation"
    );
    pool.close().await;
}
