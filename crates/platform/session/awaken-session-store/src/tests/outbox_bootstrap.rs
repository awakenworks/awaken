use super::*;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Postgres, Transaction};

struct Fixture {
    admin: PgPool,
    observer: PgPool,
    first: PgPool,
    second: PgPool,
    schema: String,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let (url, admin) = postgres_test_admin().await?;
        let schema = format!("t_outbox_gate_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .unwrap();
        let selected = scoped_postgres_url(&url, &schema);
        let observer = PgPoolOptions::new()
            .max_connections(4)
            .connect(&selected)
            .await
            .unwrap();
        let runner = awaken_scoped_migration::postgres::PostgresMigrationRunner::with_prefix(
            observer.clone(),
            NS,
        )
        .unwrap();
        runner
            .run_bundles(&[
                migration_prefix_through(&session_bundle().unwrap(), 8),
                crate::schema::converged_session_bundle().unwrap(),
                crate::postgres::replay_bundle().unwrap(),
            ])
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO managed_lifecycle_outbox (fact_id, data)
             SELECT 'old-' || id, '{}' FROM generate_series(1, 10000) id",
        )
        .execute(&observer)
        .await
        .unwrap();
        let first = PgPoolOptions::new()
            .max_connections(1)
            .connect(&selected)
            .await
            .unwrap();
        let second = PgPoolOptions::new()
            .max_connections(1)
            .connect(&selected)
            .await
            .unwrap();
        Some(Self {
            admin,
            observer,
            first,
            second,
            schema,
        })
    }

    async fn hold_old_writer(&self) -> Transaction<'_, Postgres> {
        let mut held = self.observer.begin().await.unwrap();
        sqlx::query(
            "INSERT INTO managed_lifecycle_outbox (fact_id, data)
             VALUES ('old-in-flight', '{}')",
        )
        .execute(&mut *held)
        .await
        .unwrap();
        held
    }

    async fn wait_for_index_writer_barrier(&self, pid: i32) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let waiting: bool = sqlx::query_scalar(
                    "SELECT EXISTS(
                       SELECT 1 FROM pg_stat_progress_create_index
                       WHERE pid = $1
                         AND relid = 'managed_lifecycle_outbox'::regclass
                         AND phase = 'waiting for writers before build')",
                )
                .bind(pid)
                .fetch_one(&self.observer)
                .await
                .unwrap();
                if waiting {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the real concurrent index builder reaches the old-writer barrier");
    }

    async fn wait_for_index_gate(&self, pid: i32) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let reached: bool = sqlx::query_scalar(
                    "SELECT EXISTS(
                       SELECT 1 FROM pg_stat_activity
                       WHERE pid = $1
                         AND query LIKE '%advisory_lock%'
                         AND query LIKE '%managed_lifecycle_outbox_delivery_idx%')",
                )
                .bind(pid)
                .fetch_one(&self.observer)
                .await
                .unwrap();
                if reached {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the second real initializer reaches the delivery-index gate");
    }

    async fn assert_valid_index(&self) {
        let valid: Option<bool> = sqlx::query_scalar(
            "SELECT indisvalid FROM pg_index
             WHERE indexrelid = to_regclass('managed_lifecycle_outbox_delivery_idx')
               AND indrelid = 'managed_lifecycle_outbox'::regclass",
        )
        .fetch_optional(&self.observer)
        .await
        .unwrap();
        assert_eq!(valid, Some(true));
    }

    async fn assert_first_pool_and_gate_recovered(&self) {
        let mut connection = tokio::time::timeout(Duration::from_secs(2), self.first.acquire())
            .await
            .expect("the original single-slot pool recovers its checkout")
            .unwrap();
        connection.close_on_drop();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let acquired: bool = sqlx::query_scalar(
                    "SELECT pg_try_advisory_lock(
                       hashtext(current_schema()),
                       hashtext('managed_lifecycle_outbox_delivery_idx'))",
                )
                .fetch_one(&mut *connection)
                .await
                .unwrap();
                if acquired {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the canceled index owner releases its exact session-level gate");
        connection.close().await.unwrap();
    }

    async fn cleanup(self) {
        self.first.close().await;
        self.second.close().await;
        self.observer.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA {} CASCADE",
            self.schema
        )))
        .execute(&self.admin)
        .await
        .unwrap();
        self.admin.close().await;
    }
}

async fn backend_pid(pool: &PgPool) -> i32 {
    sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn postgres_outbox_index_gate_waiters_do_not_deadlock_online_build() {
    // R1 two independent one-slot initializers and an old writer overlap.
    // The writer holds CIC before its first build while the second initializer
    // reaches the real index gate. Releasing that writer makes a blocking
    // advisory SELECT retain a VXID needed by the lock owner's later CIC scan.
    // Both original constructors must finish and ordinary writes must progress.
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let held = fixture.hold_old_writer().await;
    let first_pid = backend_pid(&fixture.first).await;
    let second_pid = backend_pid(&fixture.second).await;
    let first = tokio::spawn(PostgresManagedSessionRepository::with_pool(
        fixture.first.clone(),
    ));
    fixture.wait_for_index_writer_barrier(first_pid).await;
    let second = tokio::spawn(PostgresManagedSessionRepository::with_pool(
        fixture.second.clone(),
    ));
    fixture.wait_for_index_gate(second_pid).await;
    let writes = tokio::time::timeout(Duration::from_secs(2), async {
        for number in 0..32 {
            sqlx::query("INSERT INTO managed_lifecycle_outbox (fact_id, data) VALUES ($1, '{}')")
                .bind(format!("during-index-{number}"))
                .execute(&fixture.observer)
                .await?;
        }
        Ok::<_, sqlx::Error>(())
    })
    .await;
    held.commit().await.unwrap();
    let (first, second) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(first, second)
    })
    .await
    .expect("both index initializers finish after releasing the original writer");
    let first = first.unwrap();
    let second = second.unwrap();
    let errors = (
        first.as_ref().err().cloned(),
        second.as_ref().err().cloned(),
    );
    let both_succeeded = first.is_ok() && second.is_ok();
    drop(first);
    drop(second);
    fixture.assert_valid_index().await;
    fixture.cleanup().await;
    writes
        .expect("ordinary outbox writes keep their original two-second allowance")
        .unwrap();
    assert!(
        both_succeeded,
        "the original index constructors failed {errors:?}"
    );
}

#[tokio::test]
async fn postgres_outbox_index_gate_waiter_abort_restores_single_connection_pool() {
    // R2 aborting an initializer waiting for the same index gate closes its
    // dedicated checkout. The same one-slot pool can then acquire and migrate.
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let mut holder = fixture.observer.acquire().await.unwrap();
    holder.close_on_drop();
    sqlx::query(
        "SELECT pg_advisory_lock(
           hashtext(current_schema()),
           hashtext('managed_lifecycle_outbox_delivery_idx'))",
    )
    .execute(&mut *holder)
    .await
    .unwrap();
    let pid = backend_pid(&fixture.first).await;
    let opening = tokio::spawn(PostgresManagedSessionRepository::with_pool(
        fixture.first.clone(),
    ));
    fixture.wait_for_index_gate(pid).await;
    opening.abort();
    assert!(opening.await.is_err_and(|error| error.is_cancelled()));
    let checkout = tokio::time::timeout(Duration::from_secs(2), fixture.first.acquire())
        .await
        .expect("aborting the waiter restores the original one-slot pool")
        .unwrap();
    checkout.close().await.unwrap();
    holder.close().await.unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        PostgresManagedSessionRepository::with_pool(fixture.first.clone()),
    )
    .await
    .expect("the same pool can retry after the competing owner releases its gate")
    .unwrap();
    fixture.assert_valid_index().await;
    fixture.cleanup().await;
}

#[tokio::test]
async fn postgres_outbox_index_builder_abort_and_error_release_gate_for_retry() {
    // R3 covers both caller abort and a real server-side cancellation after the
    // index owner has acquired its gate. The original invalid-index repair and
    // one-slot checkout must remain usable on the next constructor call.
    for server_cancel in [false, true] {
        let Some(fixture) = Fixture::new().await else {
            return;
        };
        let held = fixture.hold_old_writer().await;
        let pid = backend_pid(&fixture.first).await;
        let mut opening = tokio::spawn(PostgresManagedSessionRepository::with_pool(
            fixture.first.clone(),
        ));
        fixture.wait_for_index_writer_barrier(pid).await;
        if server_cancel {
            let canceled: bool = sqlx::query_scalar("SELECT pg_cancel_backend($1)")
                .bind(pid)
                .fetch_one(&fixture.observer)
                .await
                .unwrap();
            assert!(canceled);
            let result = tokio::time::timeout(Duration::from_secs(5), &mut opening)
                .await
                .expect("the original server-side index cancellation returns")
                .unwrap();
            match result {
                Ok(_) => panic!("the canceled index build unexpectedly succeeded"),
                Err(error) => assert!(error.contains("cancel"), "{error}"),
            }
        } else {
            opening.abort();
            assert!(opening.await.is_err_and(|error| error.is_cancelled()));
        }
        held.commit().await.unwrap();
        fixture.assert_first_pool_and_gate_recovered().await;
        tokio::time::timeout(
            Duration::from_secs(5),
            PostgresManagedSessionRepository::with_pool(fixture.first.clone()),
        )
        .await
        .expect("the original invalid-index repair permits the same-pool retry")
        .unwrap();
        fixture.assert_valid_index().await;
        let preserved: i64 = sqlx::query_scalar("SELECT count(*) FROM managed_lifecycle_outbox")
            .fetch_one(&fixture.observer)
            .await
            .unwrap();
        assert_eq!(preserved, 10_001);
        fixture.cleanup().await;
    }
}
