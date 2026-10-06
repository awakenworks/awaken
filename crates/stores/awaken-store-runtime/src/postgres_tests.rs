use std::time::{Duration, Instant};

use sqlx::{Connection, Executor, PgPool};

fn database_url() -> String {
    std::env::var("AWAKEN_TEST_DATABASE_URL")
        .expect("explicit live PostgreSQL URL; never silently skip this opt-in gate")
}

async fn fixture() -> (PgPool, PgPool) {
    let url = database_url();
    let observer = PgPool::connect(&url).await.expect("observer connection");
    let pool = super::postgres_pool_options()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("canonical pool");
    (observer, pool)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit test PostgreSQL administrator for an isolated permission database"]
async fn missing_permission_and_blocked_health_check_fail_within_their_boundary() {
    // R6 healthy transport + database without role's plpgsql USAGE -> fresh
    // construction fails. Checking only recycled connections would allow the
    // first operation, then silently reconnect on every later checkout. Use an
    // owned database so revoking PUBLIC cannot affect the other live cases;
    // cleanup drops only the role/database whose CREATE succeeded here.
    // R7 restore USAGE + block the fresh connection's language lookup -> the
    // same health function returns PoolTimedOut at its 500 ms boundary. The
    // isolated catalog lock cannot disturb another case; an outer deadline
    // bounds the test if the health timeout itself regresses.
    let url = database_url();
    let options: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
    let admin = PgPool::connect(&url).await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&admin)
        .await
        .unwrap();
    let name = format!("awaken_checkout_permission_{pid}");
    let mut role_created = false;
    let mut database_created = false;
    let mut isolated = None;
    let outcome = async {
        admin
            .execute(format!("CREATE ROLE {name} LOGIN").as_str())
            .await?;
        role_created = true;
        admin
            .execute(format!("CREATE DATABASE {name}").as_str())
            .await?;
        database_created = true;
        let pool = PgPool::connect_with(options.clone().database(&name)).await?;
        pool.execute("REVOKE USAGE ON LANGUAGE plpgsql FROM PUBLIC")
            .await?;
        isolated = Some(pool);
        let denied = match super::postgres_pool_options()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(1))
            .connect_with(options.clone().username(&name).database(&name))
            .await
        {
            Ok(pool) => {
                pool.close().await;
                false
            }
            Err(_) => true,
        };
        let scoped = isolated.as_ref().unwrap();
        scoped
            .execute("GRANT USAGE ON LANGUAGE plpgsql TO PUBLIC")
            .await?;
        let role_options = options.clone().username(&name).database(&name);
        let mut connection = sqlx::PgConnection::connect_with(&role_options).await?;
        let mut lock = scoped.begin().await?;
        sqlx::query("LOCK TABLE pg_language IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *lock)
            .await?;
        let delayed = tokio::time::timeout(
            Duration::from_secs(1),
            super::verify_postgres_checkout(&mut connection),
        )
        .await;
        lock.rollback().await?;
        connection.close_hard().await?;
        let bounded = matches!(delayed, Ok(Err(sqlx::Error::PoolTimedOut)));
        Ok::<_, sqlx::Error>((denied, bounded))
    }
    .await;
    if let Some(pool) = isolated {
        pool.close().await;
    }
    if database_created {
        admin
            .execute(format!("DROP DATABASE {name}").as_str())
            .await
            .unwrap();
    }
    if role_created {
        admin
            .execute(format!("DROP ROLE {name}").as_str())
            .await
            .unwrap();
    }
    admin.close().await;
    assert_eq!(
        outcome.expect("R6/R7 owned fixture"),
        (true, true),
        "R6 fresh connection rejected; R7 blocked health check times out"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "local pool-policy performance observation; requires AWAKEN_TEST_DATABASE_URL"]
async fn checkout_health_policy_performance_observation() {
    // P1 same driver/server/query/one-connection capacity and warmed statement
    // cache; P2 compare the former bounded ping policy against the canonical
    // server-state check. Alternating old/new/new/old retains every observation
    // rather than selecting the best window. All 1000 operations must succeed;
    // these local checkout rates are not Agent, ThreadCommit or Cell capacity.
    let url = database_url();
    for phase in [
        "old-ping",
        "new-server-state",
        "new-server-state",
        "old-ping",
    ] {
        let options = if phase == "old-ping" {
            sqlx::postgres::PgPoolOptions::new()
                .idle_timeout(Duration::from_secs(5))
                .test_before_acquire(false)
                .before_acquire(|connection, _| {
                    Box::pin(async move {
                        match tokio::time::timeout(Duration::from_millis(500), connection.ping())
                            .await
                        {
                            Ok(result) => result.map(|()| true),
                            Err(_) => Err(sqlx::Error::PoolTimedOut),
                        }
                    })
                })
        } else {
            super::postgres_pool_options()
        };
        let pool = options.max_connections(1).connect(&url).await.unwrap();
        for _ in 0..10 {
            pool.execute("SELECT 42").await.unwrap();
        }
        let started = Instant::now();
        let mut latencies = Vec::with_capacity(1000);
        for _ in 0..1000 {
            let operation = Instant::now();
            let value: i32 = sqlx::query_scalar("SELECT 42")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(value, 42, "P1 successful operation");
            latencies.push(operation.elapsed().as_secs_f64() * 1000.0);
        }
        let elapsed = started.elapsed().as_secs_f64();
        latencies.sort_by(f64::total_cmp);
        println!(
            "checkout_perf phase={phase} operations=1000 operations_per_second={:.3} p50_ms={:.3} p95_ms={:.3} p99_ms={:.3}",
            1000.0 / elapsed,
            latencies[499],
            latencies[949],
            latencies[989]
        );
        pool.close().await;
    }
}

async fn wait_backend(observer: &PgPool, pid: i32, sleeping: bool) -> Result<(), sqlx::Error> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let matches: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid = $1 \
                 AND CASE WHEN $2 THEN wait_event = 'PgSleep' ELSE state <> 'active' END)",
            )
            .bind(pid)
            .bind(sleeping)
            .fetch_one(observer)
            .await?;
            if matches {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| sqlx::Error::PoolTimedOut)?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit AWAKEN_TEST_DATABASE_URL and a live PostgreSQL"]
async fn cancelled_begin_cannot_poison_the_next_recovery_transaction() {
    // Cause/effect decision rule C1: the sole pool backend has executed BEGIN;
    // C2: its response is held by pg_sleep, witnessed by an independent backend;
    // C3: the caller is cancelled before a Transaction can be returned;
    // C4: the server finishes the query and the connection returns to the pool.
    // R1=C1+C2+C3+C4 -> the next borrower must obtain a clean RR/read-only
    // recovery transaction. Neither a successful ping nor client-side depth=0
    // proves that the server ended the cancelled transaction. One connection
    // and the exact backend PID prevent selecting an unrelated healthy sibling.
    // The delay belongs only to this fault fixture, not production BEGIN SQL.
    let (observer, pool) = fixture().await;
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&pool)
        .await
        .expect("exact pool backend");
    let delayed_pool = pool.clone();
    let begin = tokio::spawn(async move {
        let transaction = delayed_pool.begin_with("BEGIN; SELECT pg_sleep(1)").await?;
        transaction.rollback().await
    });
    wait_backend(&observer, pid, true)
        .await
        .expect("server has started the exact delayed BEGIN");
    begin.abort();
    assert!(
        begin.await.unwrap_err().is_cancelled(),
        "C3 exact cancellation"
    );
    wait_backend(&observer, pid, false)
        .await
        .expect("C4 server completed the delayed response");
    let outcome = async {
        let mut transaction = pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *transaction)
            .await?;
        let isolation: String = sqlx::query_scalar("SHOW transaction_isolation")
            .fetch_one(&mut *transaction)
            .await?;
        let read_only: String = sqlx::query_scalar("SHOW transaction_read_only")
            .fetch_one(&mut *transaction)
            .await?;
        transaction.rollback().await?;
        Ok::<_, sqlx::Error>((isolation, read_only))
    }
    .await;
    pool.close().await;
    observer.close().await;
    assert_eq!(
        outcome.expect("R1 clean recovery transaction"),
        ("repeatable read".into(), "on".into())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit AWAKEN_TEST_DATABASE_URL and a live PostgreSQL"]
async fn returned_transaction_cancellation_preserves_commits_and_backend_reuse() {
    // Decision table: R2 healthy checkout + acknowledged commit -> its row
    // survives later checks; R3 Transaction already returned + cancelled query
    // -> ordinary RAII rollback removes only its uncommitted row. Both require
    // the same healthy backend and its temporary table to remain available;
    // reconnecting every time cannot satisfy this reuse/durability contract.
    let (observer, pool) = fixture().await;
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.execute("CREATE TEMP TABLE checkout_rows (value INTEGER NOT NULL)")
        .await
        .unwrap();
    let mut committed = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO checkout_rows VALUES (1)")
        .execute(&mut *committed)
        .await
        .unwrap();
    committed.commit().await.unwrap();
    let mut pending = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO checkout_rows VALUES (2)")
        .execute(&mut *pending)
        .await
        .unwrap();
    let query = tokio::spawn(async move {
        sqlx::query("SELECT pg_sleep(1)")
            .execute(&mut *pending)
            .await?;
        pending.commit().await
    });
    wait_backend(&observer, pid, true).await.unwrap();
    query.abort();
    assert!(
        query.await.unwrap_err().is_cancelled(),
        "R3 query cancelled"
    );
    wait_backend(&observer, pid, false).await.unwrap();
    let rows: Vec<i32> = sqlx::query_scalar("SELECT value FROM checkout_rows ORDER BY value")
        .fetch_all(&pool)
        .await
        .unwrap();
    let final_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;
    observer.close().await;
    assert_eq!(
        rows,
        vec![1],
        "R2 committed row retained; R3 cancelled row absent"
    );
    assert_eq!(final_pid, pid, "R2/R3 healthy connection is reused");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit AWAKEN_TEST_DATABASE_URL and a live PostgreSQL"]
async fn untracked_aborted_transaction_is_retired_before_another_borrower() {
    // R4 explicit BEGIN without a Transaction owner + statement error -> the
    // server is aborted although SQLx depth is zero. The canonical checkout
    // must retire that exact backend; a replacement must execute a normal query
    // instead of inheriting 25P02. This partitions the failure from R1's open,
    // non-aborted BEGIN and R3's properly owned cancelled transaction.
    let (observer, pool) = fixture().await;
    let mut connection = pool.acquire().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    let failure = connection.execute("BEGIN; SELECT 1 / 0").await.unwrap_err();
    assert_eq!(
        failure.as_database_error().unwrap().code().as_deref(),
        Some("22012")
    );
    drop(connection);
    let value: i32 = sqlx::query_scalar("SELECT 42")
        .fetch_one(&pool)
        .await
        .unwrap();
    let replacement: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;
    observer.close().await;
    assert_eq!(value, 42, "R4 replacement has no aborted transaction");
    assert_ne!(replacement, pid, "R4 exact poisoned backend retired");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit AWAKEN_TEST_DATABASE_URL and a live PostgreSQL"]
async fn checkout_supports_read_only_role_without_schema_create_permission() {
    // R5 healthy automatic transaction + read-only default + no schema CREATE
    // + existing plpgsql USAGE -> successful checkout with unchanged identity.
    // These conditions interact: a superuser-only or schema-writing health
    // check would pass R2 yet reject the deployed read-only capability boundary.
    let (observer, pool) = fixture().await;
    let mut connection = pool.acquire().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    connection
        .execute("SET ROLE pg_read_all_data; SET default_transaction_read_only = on")
        .await
        .unwrap();
    drop(connection);
    let observed: (i32, String, bool, bool, String) = sqlx::query_as(
        "SELECT pg_backend_pid(), current_user::text, \
         has_schema_privilege(current_user, 'public', 'CREATE'), \
         has_language_privilege(current_user, 'plpgsql', 'USAGE'), \
         current_setting('transaction_read_only')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    pool.close().await;
    observer.close().await;
    assert_eq!(
        observed,
        (pid, "pg_read_all_data".into(), false, true, "on".into()),
        "R5"
    );
}
